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

    /// send sats to an ark address, returning the transaction id. this is the operator moving
    /// money out of the wallet; the caller records it in the books.
    async fn send(&self, _address: &str, _sats: u64) -> Result<String, PaymentError> {
        Err(PaymentError("this wallet can't send".into()))
    }

    /// everything the wallet holds right now, if we can tell. this is what the audit checks the
    /// ledger against.
    async fn wallet_balance(&self) -> Result<Option<u64>, PaymentError> {
        Ok(None)
    }

    /// the bitcoin network the money is on ("bitcoin" for mainnet), if it is real
    async fn network(&self) -> Result<Option<String>, PaymentError> {
        Ok(None)
    }

    /// pretend that money arrived. only the mock supports this.
    fn simulate(&self, _request: &PaymentRequest, _sats: u64) -> bool {
        false
    }
}

pub type SharedPayments = Arc<dyn Payments>;

/// the payments in use, which the operator can set up or change while we run. until then
/// there are none, and no orders can be made.
#[derive(Default)]
pub struct Switch {
    current: std::sync::RwLock<Option<SharedPayments>>,
}

impl Switch {
    pub fn new(payments: Option<SharedPayments>) -> Self {
        Self {
            current: std::sync::RwLock::new(payments),
        }
    }

    pub fn set(&self, payments: SharedPayments) {
        *self.current.write().unwrap() = Some(payments);
    }

    pub fn is_ready(&self) -> bool {
        self.current.read().unwrap().is_some()
    }

    fn get(&self) -> Result<SharedPayments, PaymentError> {
        self.current
            .read()
            .unwrap()
            .clone()
            .ok_or_else(|| PaymentError("payments are not set up yet".into()))
    }
}

#[async_trait::async_trait]
impl Payments for Switch {
    async fn request(&self, order_id: &str) -> Result<PaymentRequest, PaymentError> {
        self.get()?.request(order_id).await
    }

    async fn received(&self, request: &PaymentRequest) -> Result<u64, PaymentError> {
        self.get()?.received(request).await
    }

    async fn send(&self, address: &str, sats: u64) -> Result<String, PaymentError> {
        self.get()?.send(address, sats).await
    }

    async fn wallet_balance(&self) -> Result<Option<u64>, PaymentError> {
        match self.get() {
            Ok(p) => p.wallet_balance().await,
            Err(_) => Ok(None),
        }
    }

    async fn network(&self) -> Result<Option<String>, PaymentError> {
        self.get()?.network().await
    }

    fn simulate(&self, request: &PaymentRequest, sats: u64) -> bool {
        self.get().is_ok_and(|p| p.simulate(request, sats))
    }
}

#[derive(Default)]
pub struct MockPayments {
    received: Mutex<HashMap<String, u64>>,
    /// sats the operator took out of the wallet
    #[allow(dead_code)]
    pub spent: Mutex<u64>,
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

    async fn send(&self, _address: &str, sats: u64) -> Result<String, PaymentError> {
        *self.spent.lock().unwrap() += sats;
        Ok(format!("mock-send-{sats}"))
    }

    async fn wallet_balance(&self) -> Result<Option<u64>, PaymentError> {
        let total: u64 = self.received.lock().unwrap().values().sum();
        Ok(Some(total.saturating_sub(*self.spent.lock().unwrap())))
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
        Bip32KeyProvider, Blockchain, Client, ContractStore, DiscoverableKeyProvider,
        Error as ArkError, InMemorySwapStorage, KeyProvider, OfflineClient, OfflineClientConfig,
        SpendStatus, TxStatus,
    };
    use ark_core::contract::{ContractState, StoredContract};
    use ark_core::{ArkAddress, ExplorerUtxo, UtxoCoinSelection};
    use ark_core::send::SendReceiver;
    use bitcoin::bip32::{DerivationPath, Xpriv};
    use bitcoin::key::Keypair;
    use bitcoin::secp256k1::XOnlyPublicKey;
    use bitcoin::{Address, Amount, FeeRate, Network, Psbt, Script, ScriptBuf, Transaction, Txid};

    use super::*;
    use crate::store::Store;
    use sha2::{Digest, Sha256};

    const NEXT_INDEX: &str = "arkade_next_index";

    fn unsupported() -> ArkError {
        ArkError::wallet("this wallet only receives offchain payments")
    }

    /// we only receive offchain, nothing here is ever supposed to run
    struct NoChain;

    impl Blockchain for NoChain {
        async fn find_outpoints(&self, _: &Address) -> Result<Vec<ExplorerUtxo>, ArkError> {
            // we only take offchain payments, so there is never a boarding output to look for
            Ok(vec![])
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

    impl FreshKeys {
        /// never hand out a key we have already used, even after a restore found money at it
        fn advance(&self, index: u32) -> Result<(), ArkError> {
            let mut next = self.next.lock().unwrap();
            if index >= *next {
                *next = index
                    .checked_add(1)
                    .ok_or_else(|| ArkError::consumer("key derivation index overflow"))?;
                self.store
                    .set_meta(NEXT_INDEX, *next as u64)
                    .map_err(|e| ArkError::wallet(e.to_string()))?;
            }
            Ok(())
        }
    }

    impl DiscoverableKeyProvider for FreshKeys {
        fn get_derivation_index_for_pk(&self, pk: &XOnlyPublicKey) -> Option<u32> {
            self.inner.get_derivation_index_for_pk(pk)
        }

        fn derive_at_discovery_index(&self, index: u32) -> Result<Option<Keypair>, ArkError> {
            self.inner.derive_at_discovery_index(index)
        }

        fn cache_discovered_keypair(&self, index: u32, kp: Keypair) -> Result<(), ArkError> {
            self.inner.cache_discovered_keypair(index, kp)?;
            self.advance(index)
        }

        fn cache_keypair_at_index(&self, index: u32) -> Result<(), ArkError> {
            // loading a contract we already know must not move the receiving index
            self.inner.cache_keypair_at_index(index)
        }

        fn mark_as_used(&self, pk: &XOnlyPublicKey) -> Result<(), ArkError> {
            self.inner.mark_as_used(pk)
        }
    }

    /// the arkd servers of arkade, by network
    pub const MAINNET_URL: &str = ark_client::ARKADE_MAINNET_URL;
    pub const MUTINYNET_URL: &str = ark_client::ARKADE_MUTINYNET_URL;

    fn contracts_key(server_url: &str, seed_hex: &str) -> String {
        let mut h = Sha256::new();
        h.update(b"arkade-contracts");
        h.update(seed_hex.as_bytes());
        h.update(server_url.as_bytes());
        format!("arkade_contracts:{}", lowercase_hex::encode(h.finalize()))
    }

    /// the wallet's contracts, kept in garden.redb. without them a restart forgets which
    /// addresses it handed out, and the money at them can't be seen or spent.
    struct Contracts {
        store: Arc<Store>,
        /// the setting this wallet's contracts are kept under, so changing the seed or the
        /// server starts with a clean wallet
        key: String,
        map: Mutex<HashMap<ScriptBuf, StoredContract>>,
    }

    impl Contracts {
        fn open(store: Arc<Store>, key: String) -> Result<Self, ArkError> {
            let list: Vec<StoredContract> = store
                .setting(&key)
                .map_err(|e| ArkError::wallet(e.to_string()))?
                .unwrap_or_default();
            let map = list.into_iter().map(|c| (c.script_pubkey.clone(), c)).collect();
            Ok(Self { store, key, map: Mutex::new(map) })
        }

        fn save(&self, map: &HashMap<ScriptBuf, StoredContract>) -> Result<(), ArkError> {
            let list: Vec<&StoredContract> = map.values().collect();
            self.store
                .set_setting(&self.key, &list)
                .map_err(|e| ArkError::wallet(e.to_string()))
        }
    }

    impl ContractStore for Contracts {
        fn insert(&mut self, contract: StoredContract) -> Result<(), ArkError> {
            let mut map = self.map.lock().unwrap();
            if map.contains_key(&contract.script_pubkey) {
                return Err(ArkError::consumer("contract script already exists"));
            }
            map.insert(contract.script_pubkey.clone(), contract);
            self.save(&map)
        }

        fn get_by_script(&self, script_pubkey: &Script) -> Result<Option<StoredContract>, ArkError> {
            Ok(self.map.lock().unwrap().get(script_pubkey).cloned())
        }

        fn list(&self) -> Result<Vec<StoredContract>, ArkError> {
            Ok(self.map.lock().unwrap().values().cloned().collect())
        }

        fn update_state(&mut self, script_pubkey: &Script, state: ContractState) -> Result<(), ArkError> {
            let mut map = self.map.lock().unwrap();
            let contract = map
                .get_mut(script_pubkey)
                .ok_or_else(|| ArkError::consumer("unknown contract script"))?;
            contract.state = state;
            self.save(&map)
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
                store: store.clone(),
                next: Mutex::new(start),
            });
            let contracts = Contracts::open(store, contracts_key(server_url, seed_hex)).map_err(err)?;
            // only the first time (or after restoring a seed) is there anything to find; after
            // that every address we hand out saves its contract
            let discover = contracts.list().map_err(err)?.is_empty();

            let client = OfflineClient::with_discoverable_key_provider(
                OfflineClientConfig {
                    ark_server_url: server_url.to_string(),
                    ..Default::default()
                },
                keys,
                Arc::new(NoChain),
                Arc::new(NoWallet),
                Arc::new(InMemorySwapStorage::default()),
            )
            .with_contract_store(Box::new(contracts))
            .connect()
            .await
            .map_err(err)?;

            if discover {
                // the contracts we saved tell us the addresses we handed out before a restart;
                // this finds the money that arrived at them
                let gap = start.saturating_add(20).max(20);
                client.restore_contracts(gap).await.map_err(err)?;
            }

            Ok(Self { client })
        }
    }

    /// a new random seed, in hex
    pub fn new_seed() -> String {
        lowercase_hex::encode(rand::random::<[u8; 32]>())
    }

    pub fn valid_seed(seed: &str) -> bool {
        seed.len() == 64 && lowercase_hex::decode(seed).is_ok()
    }

    /// whether this is an ark address (ark1… on mainnet, tark1… on the test networks)
    pub fn valid_address(address: &str) -> bool {
        ArkAddress::decode(address).is_ok()
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

        async fn send(&self, address: &str, sats: u64) -> Result<String, PaymentError> {
            if sats == 0 {
                return Err(PaymentError("the amount can't be zero".into()));
            }
            let address = ArkAddress::decode(address).map_err(err)?;
            let addresses = self.client.get_offchain_addresses().await.map_err(err)?;
            let vtxos = self
                .client
                .list_vtxos_for_addresses(addresses.into_iter().map(|(a, _)| a))
                .await
                .map_err(err)?;
            let mut outpoints = vec![];
            let mut total = 0u64;
            for v in vtxos.all_unspent() {
                outpoints.push(v.outpoint);
                total = total.saturating_add(v.amount.to_sat());
                if total >= sats {
                    break;
                }
            }
            if total < sats {
                return Err(PaymentError(format!("the wallet holds only {total} sats")));
            }
            let txid = self
                .client
                .send_selection(&outpoints, vec![SendReceiver::bitcoin(address, Amount::from_sat(sats))])
                .await
                .map_err(err)?;
            Ok(txid.to_string())
        }

        async fn wallet_balance(&self) -> Result<Option<u64>, PaymentError> {
            let addresses = self.client.get_offchain_addresses().await.map_err(err)?;
            let vtxos = self
                .client
                .list_vtxos_for_addresses(addresses.into_iter().map(|(a, _)| a))
                .await
                .map_err(err)?;
            Ok(Some(vtxos.all_unspent().map(|v| v.amount.to_sat()).sum()))
        }

        /// what the server says it is on: "bitcoin" on mainnet, "signet" on mutinynet
        async fn network(&self) -> Result<Option<String>, PaymentError> {
            Ok(Some(self.client.server_info().await.map_err(err)?.network.to_string()))
        }
    }
}
