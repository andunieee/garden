use std::collections::{BTreeMap, HashMap};
use std::sync::Mutex;

use super::*;

/// a ledger that lives in memory, for tests and development
#[derive(Default)]
pub struct MemoryLedger {
    state: Mutex<State>,
}

#[derive(Default)]
struct State {
    users: HashMap<PubKeyBytes, User>,
    files: BTreeMap<u64, File>,
    by_hash: HashMap<Hash, u64>,
    /// transfer ids that were applied, with what they moved
    applied: HashMap<u128, u64>,
    clock: u64,
    totals: Totals,
    conversions: Vec<(String, Conversion)>,
    expenses: Vec<(String, Expense)>,
}

struct User {
    balance: u64,
    timestamp: u64,
}

struct File {
    info: FileInfo,
    uploaders: Vec<PubKeyBytes>,
}

impl MemoryLedger {
    pub fn new() -> Self {
        Self::default()
    }
}

impl State {
    fn tick(&mut self) -> u64 {
        self.clock += 1;
        self.clock
    }
}

#[async_trait::async_trait]
impl Ledger for MemoryLedger {
    async fn user(&self, pubkey: &PubKeyBytes) -> Result<Option<UserInfo>> {
        let st = self.state.lock().unwrap();
        Ok(st.users.get(pubkey).map(|u| UserInfo {
            pubkey: *pubkey,
            balance: u.balance,
            timestamp: u.timestamp,
        }))
    }

    async fn credit_user(&self, pubkey: &PubKeyBytes, sats: u64, payment_id: u128) -> Result<()> {
        let mut st = self.state.lock().unwrap();
        let first_time = st
            .applied
            .insert(transfer_id(&[b"pay", &payment_id.to_be_bytes()]), sats)
            .is_none();
        if !st.users.contains_key(pubkey) {
            let timestamp = st.tick();
            st.users.insert(*pubkey, User { balance: 0, timestamp });
        }
        if first_time {
            st.users.get_mut(pubkey).unwrap().balance += sats;
            st.totals.received += sats;
        }
        Ok(())
    }

    async fn file(&self, hash: &Hash) -> Result<Option<FileInfo>> {
        let st = self.state.lock().unwrap();
        Ok(st.by_hash.get(hash).map(|ts| st.files[ts].info.clone()))
    }

    async fn create_file(
        &self,
        hash: &Hash,
        size: u64,
        provider: u32,
        uploader: &PubKeyBytes,
        funding: u64,
    ) -> Result<()> {
        let mut st = self.state.lock().unwrap();
        if let Some(ts) = st.by_hash.get(hash)
            && !st.files[ts].info.closed
        {
            return Err(LedgerError::FileExists);
        }

        let user = st.users.get_mut(uploader).ok_or(LedgerError::NoSuchAccount)?;
        user.balance = user.balance.checked_sub(funding).ok_or(LedgerError::InsufficientBalance)?;

        match st.by_hash.get(hash).copied() {
            Some(ts) => {
                let f = st.files.get_mut(&ts).unwrap();
                f.info.closed = false;
                f.info.balance += funding;
                f.uploaders.push(*uploader);
            }
            None => {
                let ts = st.tick();
                st.by_hash.insert(*hash, ts);
                st.files.insert(
                    ts,
                    File {
                        info: FileInfo {
                            hash: *hash,
                            size,
                            provider,
                            balance: funding,
                            closed: false,
                            timestamp: ts,
                        },
                        uploaders: vec![*uploader],
                    },
                );
            }
        }
        Ok(())
    }

    async fn credit_file(&self, hash: &Hash, sats: u64, payment_id: u128) -> Result<Credited> {
        let mut st = self.state.lock().unwrap();
        let Some(&ts) = st.by_hash.get(hash) else {
            return Ok(Credited::NoSuchFile);
        };
        if st.files[&ts].info.closed {
            return Ok(Credited::NoSuchFile);
        }
        let id = transfer_id(&[b"topup", &payment_id.to_be_bytes(), hash]);
        if st.applied.insert(id, sats).is_some() {
            return Ok(Credited::AlreadyDone);
        }
        st.files.get_mut(&ts).unwrap().info.balance += sats;
        st.totals.received += sats;
        Ok(Credited::Yes)
    }

    async fn charge_file(&self, hash: &Hash, sats: u64, charge_id: u128) -> Result<u64> {
        let mut st = self.state.lock().unwrap();
        let Some(&ts) = st.by_hash.get(hash) else {
            return Err(LedgerError::NoSuchAccount);
        };
        let id = transfer_id(&[b"rent", &charge_id.to_be_bytes(), hash]);
        if let Some(taken) = st.applied.get(&id) {
            // like the real thing, repeating a charge reports what was taken the first time
            return Ok(*taken);
        }
        let f = st.files.get_mut(&ts).unwrap();
        let taken = sats.min(f.info.balance);
        f.info.balance -= taken;
        if taken > 0 {
            st.applied.insert(id, taken);
            st.totals.earned += taken;
        }
        Ok(taken)
    }

    async fn close_file(&self, hash: &Hash) -> Result<()> {
        let mut st = self.state.lock().unwrap();
        let ts = *st.by_hash.get(hash).ok_or(LedgerError::NoSuchAccount)?;
        st.files.get_mut(&ts).unwrap().info.closed = true;
        Ok(())
    }

    async fn uploaders(&self, hash: &Hash) -> Result<Vec<PubKeyBytes>> {
        let st = self.state.lock().unwrap();
        Ok(st
            .by_hash
            .get(hash)
            .map(|ts| st.files[ts].uploaders.clone())
            .unwrap_or_default())
    }

    async fn files_by_uploader(&self, pubkey: &PubKeyBytes) -> Result<Vec<Hash>> {
        let st = self.state.lock().unwrap();
        Ok(st
            .files
            .values()
            .filter(|f| f.uploaders.contains(pubkey))
            .map(|f| f.info.hash)
            .collect())
    }

    async fn list_files(&self, since: u64, limit: usize) -> Result<Vec<FileInfo>> {
        let st = self.state.lock().unwrap();
        Ok(st
            .files
            .range(since..)
            .take(limit)
            .map(|(_, f)| f.info.clone())
            .collect())
    }

    async fn list_users(&self, since: u64, limit: usize) -> Result<Vec<UserInfo>> {
        let st = self.state.lock().unwrap();
        let mut users: Vec<_> = st
            .users
            .iter()
            .filter(|(_, u)| u.timestamp >= since)
            .map(|(pk, u)| UserInfo {
                pubkey: *pk,
                balance: u.balance,
                timestamp: u.timestamp,
            })
            .collect();
        users.sort_by_key(|u| u.timestamp);
        users.truncate(limit);
        Ok(users)
    }

    async fn totals(&self) -> Result<Totals> {
        Ok(self.state.lock().unwrap().totals)
    }

    async fn record_unclaimed(&self, sats: u64, id: u128) -> Result<()> {
        let mut st = self.state.lock().unwrap();
        if sats > 0 && st.applied.insert(transfer_id(&[b"unclaimed", &id.to_be_bytes()]), sats).is_none() {
            st.totals.received += sats;
            st.totals.unclaimed += sats;
        }
        Ok(())
    }

    async fn record_conversion(&self, reference: &str, sats: u64, fiat_minor: u64) -> Result<bool> {
        let mut st = self.state.lock().unwrap();
        if let Some((_, c)) = st.conversions.iter().find(|(r, _)| r == reference) {
            return if (c.sats, c.fiat_minor) == (sats, fiat_minor) {
                Ok(false)
            } else {
                Err(LedgerError::ReferenceReused)
            };
        }
        let timestamp = st.tick();
        st.conversions.push((
            reference.to_string(),
            Conversion {
                sats,
                fiat_minor,
                timestamp,
                reference: reference_id(reference),
            },
        ));
        st.totals.sold += sats;
        st.totals.fiat_in += fiat_minor;
        Ok(true)
    }

    async fn record_expense(&self, provider: u32, reference: &str, amount_minor: u64, date: u64) -> Result<bool> {
        let mut st = self.state.lock().unwrap();
        if let Some((_, e)) = st
            .expenses
            .iter()
            .find(|(r, e)| r == reference && e.provider == provider)
        {
            return if (e.amount_minor, e.date) == (amount_minor, date) {
                Ok(false)
            } else {
                Err(LedgerError::ReferenceReused)
            };
        }
        let timestamp = st.tick();
        st.expenses.push((
            reference.to_string(),
            Expense {
                provider,
                amount_minor,
                date,
                timestamp,
                reference: reference_id(reference),
            },
        ));
        st.totals.fiat_out += amount_minor;
        Ok(true)
    }

    async fn expenses(&self, provider: u32) -> Result<Vec<Expense>> {
        let st = self.state.lock().unwrap();
        Ok(st
            .expenses
            .iter()
            .filter(|(_, e)| e.provider == provider)
            .map(|(_, e)| e.clone())
            .collect())
    }

    async fn conversions(&self) -> Result<Vec<Conversion>> {
        let st = self.state.lock().unwrap();
        Ok(st.conversions.iter().map(|(_, c)| c.clone()).collect())
    }
}

#[cfg(test)]
mod tests {
    #[tokio::test]
    async fn conforms() {
        super::super::conformance::run(&super::MemoryLedger::new()).await;
    }
}
