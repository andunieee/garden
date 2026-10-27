use ::tigerbeetle as tb;

use super::*;

pub struct TigerBeetleLedger {
    client: tb::Client,
}

fn other(e: impl std::fmt::Display) -> LedgerError {
    LedgerError::Other(e.to_string())
}

fn sats(v: u128) -> u64 {
    u64::try_from(v).unwrap_or(u64::MAX)
}

impl TigerBeetleLedger {
    pub async fn connect(cluster_id: u64, addresses: &str) -> Result<Self> {
        let ledger = Self {
            client: tb::Client::new(cluster_id as u128, addresses).map_err(other)?,
        };

        let system = |id| tb::Account {
            id,
            ledger: LEDGER_SATS,
            code: CODE_SYSTEM,
            ..Default::default()
        };
        ledger
            .create_accounts(&[system(TREASURY_ID), system(REVENUE_ID)])
            .await?;
        Ok(ledger)
    }

    /// creates the accounts, those that exist already are fine
    async fn create_accounts(&self, accounts: &[tb::Account]) -> Result<()> {
        let results = self.client.create_accounts(accounts).await.map_err(other)?;
        for r in results {
            match r.status {
                tb::CreateAccountStatus::Created | tb::CreateAccountStatus::Exists => {}
                status => return Err(other(format!("creating account: {status:?}"))),
            }
        }
        Ok(())
    }

    async fn lookup(&self, id: u128) -> Result<Option<tb::Account>> {
        let accounts = self.client.lookup_accounts(&[id]).await.map_err(other)?;
        Ok(accounts.into_iter().next())
    }

    async fn transfer(&self, t: tb::Transfer) -> Result<tb::CreateTransferStatus> {
        let results = self.client.create_transfers(&[t]).await.map_err(other)?;
        Ok(results
            .first()
            .ok_or_else(|| other("empty response"))?
            .status)
    }

    async fn lookup_file(&self, hash: &Hash) -> Result<Option<tb::Account>> {
        let id = account_id(hash);
        if !valid_account_id(id) {
            return Ok(None);
        }
        Ok(self
            .lookup(id)
            .await?
            .filter(|a| a.code == CODE_FILE && a.user_data_128 == account_rest(hash)))
    }
}

fn balance(a: &tb::Account) -> u64 {
    sats(a.credits_posted.saturating_sub(a.debits_posted))
}

fn is_closed(a: &tb::Account) -> bool {
    a.flags.0 & tb::AccountFlags::Closed.0 != 0
}

fn file_info(a: &tb::Account) -> FileInfo {
    FileInfo {
        hash: join_account(a.id, a.user_data_128),
        size: a.user_data_64,
        provider: a.user_data_32,
        balance: balance(a),
        closed: is_closed(a),
        timestamp: a.timestamp,
    }
}

#[async_trait::async_trait]
impl Ledger for TigerBeetleLedger {
    async fn user(&self, pubkey: &PubKeyBytes) -> Result<Option<UserInfo>> {
        let id = account_id(pubkey);
        if !valid_account_id(id) {
            return Ok(None);
        }
        Ok(self
            .lookup(id)
            .await?
            .filter(|a| a.code == CODE_USER && a.user_data_128 == account_rest(pubkey))
            .map(|a| UserInfo {
                pubkey: *pubkey,
                balance: balance(&a),
            }))
    }

    async fn credit_user(&self, pubkey: &PubKeyBytes, amount: u64, payment_id: u128) -> Result<()> {
        let id = account_id(pubkey);
        if !valid_account_id(id) {
            return Err(other("unusable pubkey"));
        }

        self.create_accounts(&[tb::Account {
            id,
            ledger: LEDGER_SATS,
            code: CODE_USER,
            flags: tb::AccountFlags::DebitsMustNotExceedCredits,
            user_data_128: account_rest(pubkey),
            ..Default::default()
        }])
        .await?;

        let status = self
            .transfer(tb::Transfer {
                id: transfer_id(&[b"pay", &payment_id.to_be_bytes()]),
                debit_account_id: TREASURY_ID,
                credit_account_id: id,
                amount: amount as u128,
                ledger: LEDGER_SATS,
                code: T_PAYMENT,
                ..Default::default()
            })
            .await?;
        match status {
            tb::CreateTransferStatus::Created | tb::CreateTransferStatus::Exists => Ok(()),
            status => Err(other(format!("crediting user: {status:?}"))),
        }
    }

    async fn file(&self, hash: &Hash) -> Result<Option<FileInfo>> {
        Ok(self.lookup_file(hash).await?.as_ref().map(file_info))
    }

    async fn create_file(
        &self,
        hash: &Hash,
        size: u64,
        provider: u32,
        uploader: &PubKeyBytes,
        funding: u64,
    ) -> Result<()> {
        let id = account_id(hash);
        if !valid_account_id(id) {
            return Err(other("unusable hash"));
        }
        let user_id = account_id(uploader);

        // fail early, before touching the file account
        match self.user(uploader).await? {
            None => return Err(LedgerError::NoSuchAccount),
            Some(u) if u.balance < funding => return Err(LedgerError::InsufficientBalance),
            Some(_) => {}
        }

        match self.lookup_file(hash).await? {
            Some(existing) if !is_closed(&existing) => return Err(LedgerError::FileExists),
            Some(_) => {
                // it expired before: undo the closing
                let closing = self
                    .client
                    .get_account_transfers(tb::AccountFilter {
                        account_id: id,
                        code: T_CLOSE,
                        limit: 1,
                        flags: tb::AccountFilterFlags::Credits | tb::AccountFilterFlags::Reversed,
                        ..Default::default()
                    })
                    .await
                    .map_err(other)?
                    .into_iter()
                    .next()
                    .ok_or_else(|| other("closed account without a closing transfer"))?;
                let status = self
                    .transfer(tb::Transfer {
                        id: tb::id(),
                        pending_id: closing.id,
                        flags: tb::TransferFlags::VoidPendingTransfer,
                        ..Default::default()
                    })
                    .await?;
                if status != tb::CreateTransferStatus::Created {
                    return Err(other(format!("reopening file account: {status:?}")));
                }
            }
            None => {
                self.create_accounts(&[tb::Account {
                    id,
                    ledger: LEDGER_SATS,
                    code: CODE_FILE,
                    flags: tb::AccountFlags::DebitsMustNotExceedCredits,
                    user_data_128: account_rest(hash),
                    user_data_64: size,
                    user_data_32: provider,
                    ..Default::default()
                }])
                .await?;
            }
        }

        let status = self
            .transfer(tb::Transfer {
                id: tb::id(),
                debit_account_id: user_id,
                credit_account_id: id,
                amount: funding as u128,
                ledger: LEDGER_SATS,
                code: T_UPLOAD,
                ..Default::default()
            })
            .await?;
        match status {
            tb::CreateTransferStatus::Created => Ok(()),
            tb::CreateTransferStatus::ExceedsCredits => Err(LedgerError::InsufficientBalance),
            tb::CreateTransferStatus::DebitAccountNotFound => Err(LedgerError::NoSuchAccount),
            status => Err(other(format!("funding file: {status:?}"))),
        }
    }

    async fn credit_file(&self, hash: &Hash, amount: u64, payment_id: u128) -> Result<Credited> {
        let id = account_id(hash);
        let status = self
            .transfer(tb::Transfer {
                id: transfer_id(&[b"topup", &payment_id.to_be_bytes(), hash]),
                debit_account_id: TREASURY_ID,
                credit_account_id: id,
                amount: amount as u128,
                ledger: LEDGER_SATS,
                code: T_TOPUP,
                ..Default::default()
            })
            .await?;
        match status {
            tb::CreateTransferStatus::Created => Ok(Credited::Yes),
            tb::CreateTransferStatus::Exists => Ok(Credited::AlreadyDone),
            tb::CreateTransferStatus::CreditAccountNotFound
            | tb::CreateTransferStatus::CreditAccountAlreadyClosed => Ok(Credited::NoSuchFile),
            status => Err(other(format!("crediting file: {status:?}"))),
        }
    }

    async fn charge_file(&self, hash: &Hash, amount: u64, charge_id: u128) -> Result<u64> {
        let file = self
            .lookup_file(hash)
            .await?
            .ok_or(LedgerError::NoSuchAccount)?;
        let id = transfer_id(&[b"rent", &charge_id.to_be_bytes(), hash]);

        // already charged: say how much it was
        if let Some(t) = self.client.lookup_transfers(&[id]).await.map_err(other)?.first() {
            return Ok(sats(t.amount));
        }
        if balance(&file) == 0 || is_closed(&file) {
            return Ok(0);
        }

        let status = self
            .transfer(tb::Transfer {
                id,
                debit_account_id: account_id(hash),
                credit_account_id: REVENUE_ID,
                amount: amount as u128,
                // takes less if there is less
                flags: tb::TransferFlags::BalancingDebit,
                ledger: LEDGER_SATS,
                code: T_RENT,
                ..Default::default()
            })
            .await?;
        match status {
            tb::CreateTransferStatus::Created | tb::CreateTransferStatus::Exists => {}
            // somebody else got the last sats first
            tb::CreateTransferStatus::ExceedsCredits => return Ok(0),
            status => return Err(other(format!("charging file: {status:?}"))),
        }

        let t = self.client.lookup_transfers(&[id]).await.map_err(other)?;
        Ok(t.first().map(|t| sats(t.amount)).unwrap_or(0))
    }

    async fn close_file(&self, hash: &Hash) -> Result<()> {
        let file = self
            .lookup_file(hash)
            .await?
            .ok_or(LedgerError::NoSuchAccount)?;
        if is_closed(&file) {
            return Ok(());
        }

        let status = self
            .transfer(tb::Transfer {
                id: tb::id(),
                debit_account_id: REVENUE_ID,
                credit_account_id: file.id,
                amount: 0,
                // pending without a timeout, so it stays until it's voided on a new upload
                flags: tb::TransferFlags::Pending | tb::TransferFlags::ClosingCredit,
                ledger: LEDGER_SATS,
                code: T_CLOSE,
                ..Default::default()
            })
            .await?;
        match status {
            tb::CreateTransferStatus::Created | tb::CreateTransferStatus::CreditAccountAlreadyClosed => Ok(()),
            status => Err(other(format!("closing file: {status:?}"))),
        }
    }

    async fn uploaders(&self, hash: &Hash) -> Result<Vec<PubKeyBytes>> {
        let transfers = self
            .client
            .get_account_transfers(tb::AccountFilter {
                account_id: account_id(hash),
                code: T_UPLOAD,
                limit: 1000,
                flags: tb::AccountFilterFlags::Credits,
                ..Default::default()
            })
            .await
            .map_err(other)?;

        let ids: Vec<u128> = transfers.iter().map(|t| t.debit_account_id).collect();
        let users = self.accounts_by_id(&ids).await?;
        Ok(ids
            .iter()
            .filter_map(|id| users.get(id))
            .map(|a| join_account(a.id, a.user_data_128))
            .collect())
    }

    async fn files_by_uploader(&self, pubkey: &PubKeyBytes) -> Result<Vec<Hash>> {
        const PAGE: u32 = 8189;
        let mut ids: Vec<u128> = Vec::new();
        let mut after = 0;
        loop {
            let transfers = self
                .client
                .get_account_transfers(tb::AccountFilter {
                    account_id: account_id(pubkey),
                    code: T_UPLOAD,
                    limit: PAGE,
                    timestamp_min: after,
                    flags: tb::AccountFilterFlags::Debits,
                    ..Default::default()
                })
                .await
                .map_err(other)?;
            let Some(last) = transfers.last() else { break };
            after = last.timestamp + 1;
            let done = transfers.len() < PAGE as usize;
            for t in transfers {
                if !ids.contains(&t.credit_account_id) {
                    ids.push(t.credit_account_id);
                }
            }
            if done {
                break;
            }
        }

        let files = self.accounts_by_id(&ids).await?;
        Ok(ids
            .iter()
            .filter_map(|id| files.get(id))
            .map(|a| join_account(a.id, a.user_data_128))
            .collect())
    }

    async fn list_files(&self, since: u64, limit: usize) -> Result<Vec<FileInfo>> {
        let accounts = self
            .client
            .query_accounts(tb::QueryFilter {
                ledger: LEDGER_SATS,
                code: CODE_FILE,
                timestamp_min: since,
                limit: limit.min(8189) as u32,
                ..Default::default()
            })
            .await
            .map_err(other)?;
        Ok(accounts.iter().map(file_info).collect())
    }
}

impl TigerBeetleLedger {
    async fn accounts_by_id(&self, ids: &[u128]) -> Result<std::collections::HashMap<u128, tb::Account>> {
        let mut out = std::collections::HashMap::new();
        for chunk in ids.chunks(8000) {
            for a in self.client.lookup_accounts(chunk).await.map_err(other)? {
                out.insert(a.id, a);
            }
        }
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    /// needs a fresh cluster, since the test uses fixed ids:
    ///
    /// ```text
    /// tigerbeetle format --cluster=0 --replica=0 --replica-count=1 --development 0_0.tigerbeetle
    /// tigerbeetle start --addresses=3000 --development 0_0.tigerbeetle
    /// TB_ADDRESS=3000 cargo test -- --ignored tigerbeetle
    /// ```
    #[tokio::test]
    #[ignore = "needs a running tigerbeetle"]
    async fn conforms() {
        let address = std::env::var("TB_ADDRESS").unwrap_or_else(|_| "3000".into());
        let ledger = super::TigerBeetleLedger::connect(0, &address).await.unwrap();
        super::super::conformance::run(&ledger).await;
    }
}
