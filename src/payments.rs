//! how we get paid.
//!
//! every order gets its own payment request (an address nobody else was given) and we find out
//! how much was received there, so payments can't be mixed up and payers don't need to
//! identify themselves.

use std::collections::HashMap;
use std::str::FromStr;
use std::sync::Arc;
use std::sync::Mutex;

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct PaymentRequest {
    /// "arkade"
    pub method: String,
    pub address: String,
    /// what to put in a qr code or link
    pub uri: String,
}

#[derive(thiserror::Error, Debug)]
#[error("payment provider failure: {0}")]
pub struct PaymentError(pub String);

fn err(e: impl std::fmt::Display) -> PaymentError {
    PaymentError(e.to_string())
}

#[async_trait::async_trait]
pub trait Payments: Send + Sync {
    /// a new place to pay for this order
    async fn request(&self, order_id: &str) -> Result<PaymentRequest, PaymentError>;

    /// total sats that arrived at a request so far
    async fn received(&self, request: &PaymentRequest) -> Result<u64, PaymentError>;

    /// pretend that money arrived. only the mock supports this.
    fn simulate(&self, _request: &PaymentRequest, _sats: u64) -> bool {
        false
    }
}

pub type SharedPayments = Arc<dyn Payments>;

#[derive(Default)]
pub struct MockPayments {
    received: Mutex<HashMap<String, u64>>,
}

#[async_trait::async_trait]
impl Payments for MockPayments {
    async fn request(&self, order_id: &str) -> Result<PaymentRequest, PaymentError> {
        let address = format!("mock:{order_id}");
        Ok(PaymentRequest {
            method: "mock".into(),
            uri: address.clone(),
            address,
        })
    }

    async fn received(&self, request: &PaymentRequest) -> Result<u64, PaymentError> {
        Ok(*self.received.lock().unwrap().get(&request.address).unwrap_or(&0))
    }

    fn simulate(&self, request: &PaymentRequest, sats: u64) -> bool {
        *self
            .received
            .lock()
            .unwrap()
            .entry(request.address.clone())
            .or_default() += sats;
        true
    }
}

pub mod arkade {
    //! payments on arkade (https://arkade.sh).
    //!
    //! we run an offchain-only wallet from a seed. each order gets the next address of the
    //! wallet (hd derived, so every order has a different one) and we look for vtxos at it. the
    //! money ends up in a normal arkade wallet that anyone with the seed can restore.

    use ark_client::key_provider::KeypairIndex;
    use ark_client::wallet::{Balance, OnchainWallet};
    use ark_client::{
        Bip32KeyProvider, Blockchain, Client, Error as ArkError, InMemorySwapStorage, KeyProvider,
        OfflineClient, OfflineClientConfig, SpendStatus, TxStatus,
    };
    use ark_core::{ArkAddress, ExplorerUtxo, UtxoCoinSelection};
    use bitcoin::bip32::{DerivationPath, Xpriv};
    use bitcoin::key::Keypair;
    use bitcoin::secp256k1::XOnlyPublicKey;
    use bitcoin::{Address, Amount, FeeRate, Network, Psbt, Transaction, Txid};

    use super::*;
    use crate::store::Store;

    const NEXT_INDEX: &str = "arkade_next_index";

    fn unsupported() -> ArkError {
        ArkError::wallet("this wallet only receives offchain payments")
    }

    /// we only receive offchain, nothing here is ever supposed to run
    struct NoChain;

    impl Blockchain for NoChain {
        async fn find_outpoints(&self, _: &Address) -> Result<Vec<ExplorerUtxo>, ArkError> {
            Err(unsupported())
        }
        async fn find_tx(&self, _: &Txid) -> Result<Option<Transaction>, ArkError> {
            Err(unsupported())
        }
        async fn get_tx_status(&self, _: &Txid) -> Result<TxStatus, ArkError> {
            Err(unsupported())
        }
        async fn get_output_status(&self, _: &Txid, _: u32) -> Result<SpendStatus, ArkError> {
            Err(unsupported())
        }
        async fn broadcast(&self, _: &Transaction) -> Result<(), ArkError> {
            Err(unsupported())
        }
        async fn get_fee_rate(&self) -> Result<f64, ArkError> {
            Err(unsupported())
        }
        async fn broadcast_package(&self, _: &[&Transaction]) -> Result<(), ArkError> {
            Err(unsupported())
        }
    }

    struct NoWallet;

    impl OnchainWallet for NoWallet {
        fn get_onchain_address(&self) -> Result<Address, ArkError> {
            Err(unsupported())
        }
        async fn sync(&self) -> Result<(), ArkError> {
            Ok(())
        }
        fn balance(&self) -> Result<Balance, ArkError> {
            Err(unsupported())
        }
        fn prepare_send_to_address(&self, _: Address, _: Amount, _: FeeRate) -> Result<Psbt, ArkError> {
            Err(unsupported())
        }
        fn sign(&self, _: &mut Psbt) -> Result<bool, ArkError> {
            Err(unsupported())
        }
        fn select_coins(&self, _: Amount) -> Result<UtxoCoinSelection, ArkError> {
            Err(unsupported())
        }
    }

    /// the default key provider keeps handing out the same key until it receives something,
    /// we need a new one for each order, and to never go back after a restart
    struct FreshKeys {
        inner: Bip32KeyProvider,
        store: Arc<Store>,
        next: Mutex<u32>,
    }

    impl KeyProvider for FreshKeys {
        fn get_next_keypair(&self, _: KeypairIndex) -> Result<Keypair, ArkError> {
            let mut next = self.next.lock().unwrap();
            let kp = self.inner.get_next_keypair(KeypairIndex::New)?;
            *next += 1;
            self.store
                .set_meta(NEXT_INDEX, *next as u64)
                .map_err(|e| ArkError::wallet(e.to_string()))?;
            Ok(kp)
        }

        fn get_keypair_for_path(&self, path: &[u32]) -> Result<Keypair, ArkError> {
            self.inner.get_keypair_for_path(path)
        }

        fn get_keypair_for_pk(&self, pk: &XOnlyPublicKey) -> Result<Keypair, ArkError> {
            self.inner.get_keypair_for_pk(pk)
        }

        fn get_cached_pks(&self) -> Result<Vec<XOnlyPublicKey>, ArkError> {
            self.inner.get_cached_pks()
        }
    }

    pub struct Arkade {
        client: Client<NoChain, NoWallet, InMemorySwapStorage>,
    }

    impl Arkade {
        pub async fn connect(server_url: &str, seed_hex: &str, store: Arc<Store>) -> Result<Self, PaymentError> {
            let seed = lowercase_hex::decode(seed_hex).map_err(err)?;
            let xpriv = Xpriv::new_master(Network::Bitcoin, &seed).map_err(err)?;
            let path = DerivationPath::from_str(ark_core::DEFAULT_DERIVATION_PATH).map_err(err)?;

            // leave a margin in case we crashed after handing out a key but before saving
            let start = store.meta(NEXT_INDEX).map_err(err)?.unwrap_or(0) as u32;
            let keys = Arc::new(FreshKeys {
                inner: Bip32KeyProvider::new_with_index(xpriv, path, start),
                store,
                next: Mutex::new(start),
            });

            let client = OfflineClient::with_key_provider(
                OfflineClientConfig {
                    ark_server_url: server_url.to_string(),
                    ..Default::default()
                },
                keys,
                Arc::new(NoChain),
                Arc::new(NoWallet),
                Arc::new(InMemorySwapStorage::default()),
            )
            .connect()
            .await
            .map_err(err)?;

            Ok(Self { client })
        }
    }

    #[async_trait::async_trait]
    impl Payments for Arkade {
        async fn request(&self, _order_id: &str) -> Result<PaymentRequest, PaymentError> {
            let (address, _vtxo) = self.client.get_offchain_address().await.map_err(err)?;
            let address = address.encode();
            Ok(PaymentRequest {
                method: "arkade".into(),
                uri: format!("ark:{address}"),
                address,
            })
        }

        async fn received(&self, request: &PaymentRequest) -> Result<u64, PaymentError> {
            let address = ArkAddress::decode(&request.address).map_err(err)?;
            let vtxos = self
                .client
                .list_vtxos_for_addresses(std::iter::once(address))
                .await
                .map_err(err)?;
            // spent ones count, whatever was received once was received
            Ok(vtxos.all().map(|v| v.amount.to_sat()).sum())
        }
    }
}
