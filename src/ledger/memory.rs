use std::collections::{BTreeMap, HashMap, HashSet};
use std::sync::Mutex;

use super::*;

/// a ledger that lives in memory, for tests and development
#[derive(Default)]
pub struct MemoryLedger {
    state: Mutex<State>,
}

#[derive(Default)]
struct State {
    users: HashMap<PubKeyBytes, u64>,
    files: BTreeMap<u64, File>,
    by_hash: HashMap<Hash, u64>,
    applied: HashSet<u128>,
    clock: u64,
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

#[async_trait::async_trait]
impl Ledger for MemoryLedger {
    async fn user(&self, pubkey: &PubKeyBytes) -> Result<Option<UserInfo>> {
        let st = self.state.lock().unwrap();
        Ok(st.users.get(pubkey).map(|&balance| UserInfo {
            pubkey: *pubkey,
            balance,
        }))
    }

    async fn credit_user(&self, pubkey: &PubKeyBytes, sats: u64, payment_id: u128) -> Result<()> {
        let mut st = self.state.lock().unwrap();
        let first_time = st.applied.insert(transfer_id(&[b"pay", &payment_id.to_be_bytes()]));
        let balance = st.users.entry(*pubkey).or_insert(0);
        if first_time {
            *balance += sats;
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

        let balance = st.users.get_mut(uploader).ok_or(LedgerError::NoSuchAccount)?;
        *balance = balance.checked_sub(funding).ok_or(LedgerError::InsufficientBalance)?;

        match st.by_hash.get(hash).copied() {
            Some(ts) => {
                let f = st.files.get_mut(&ts).unwrap();
                f.info.closed = false;
                f.info.balance += funding;
                f.uploaders.push(*uploader);
            }
            None => {
                st.clock += 1;
                let ts = st.clock;
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
        if !st.applied.insert(transfer_id(&[b"topup", &payment_id.to_be_bytes(), hash])) {
            return Ok(Credited::AlreadyDone);
        }
        st.files.get_mut(&ts).unwrap().info.balance += sats;
        Ok(Credited::Yes)
    }

    async fn charge_file(&self, hash: &Hash, sats: u64, charge_id: u128) -> Result<u64> {
        let mut st = self.state.lock().unwrap();
        let Some(&ts) = st.by_hash.get(hash) else {
            return Err(LedgerError::NoSuchAccount);
        };
        let seen = !st.applied.insert(transfer_id(&[b"rent", &charge_id.to_be_bytes(), hash]));
        let f = st.files.get_mut(&ts).unwrap();
        if seen {
            // like the real thing, repeating the charge reports what was taken the first time
            return Ok(sats);
        }
        let taken = sats.min(f.info.balance);
        f.info.balance -= taken;
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
}

#[cfg(test)]
mod tests {
    #[tokio::test]
    async fn conforms() {
        super::super::conformance::run(&super::MemoryLedger::new()).await;
    }
}
