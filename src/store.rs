//! local state that doesn't belong in the ledger: orders waiting for their payment, bans, and
//! blobs that still have to be deleted from their providers.
//!
//! nothing here says who owns what, accounts and files only exist in the ledger.

use std::collections::BTreeMap;
use std::path::Path;

use redb::{Database, ReadableDatabase, ReadableTable, TableDefinition};
use serde::{Deserialize, Serialize};

use crate::payments::PaymentRequest;

const ORDERS: TableDefinition<&str, &str> = TableDefinition::new("orders");
/// ids of the orders that are still waiting for the money
const WAITING: TableDefinition<&str, u64> = TableDefinition::new("waiting");
const BANNED_PUBKEYS: TableDefinition<&str, u64> = TableDefinition::new("banned_pubkeys");
const BANNED_HASHES: TableDefinition<&str, u64> = TableDefinition::new("banned_hashes");
/// hash -> provider
const PENDING_DELETES: TableDefinition<&str, u32> = TableDefinition::new("pending_deletes");
const META: TableDefinition<&str, u64> = TableDefinition::new("meta");

#[derive(thiserror::Error, Debug)]
#[error("local storage failure: {0}")]
pub struct StoreError(String);

impl<E: Into<redb::Error>> From<E> for StoreError {
    fn from(e: E) -> Self {
        StoreError(e.into().to_string())
    }
}

pub type Result<T> = std::result::Result<T, StoreError>;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum OrderKind {
    /// pay to create an account or to add credit to it
    Account { pubkey: String },
    /// pay to add sats to files, each file gets its share. no account involved.
    Files { files: BTreeMap<String, u64> },
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum OrderStatus {
    Pending,
    Paid,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FileCredit {
    pub hash: String,
    pub sats: u64,
    /// `credited`, or `gone` if the file expired or vanished before we got the money
    pub result: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Order {
    pub id: String,
    pub created_at: u64,
    pub kind: OrderKind,
    /// what the payer must send
    pub amount_sats: u64,
    pub status: OrderStatus,
    pub payment: PaymentRequest,
    /// filled when the order is paid, for file orders
    #[serde(default)]
    pub credits: Vec<FileCredit>,
}

impl Order {
    /// the id we use in the ledger so that applying an order twice is harmless
    pub fn payment_id(&self) -> u128 {
        u128::from_str_radix(&self.id, 16).unwrap_or(0)
    }
}

pub struct Store {
    db: Database,
}

impl Store {
    pub fn open(dir: &str) -> Result<Self> {
        std::fs::create_dir_all(dir).map_err(|e| StoreError(e.to_string()))?;
        Self::from_db(Database::create(Path::new(dir).join("garden.redb"))?)
    }

    #[cfg(test)]
    pub fn temporary() -> Result<Self> {
        let file = std::env::temp_dir().join(format!("garden-{:032x}.redb", rand::random::<u128>()));
        Self::from_db(Database::create(file)?)
    }

    fn from_db(db: Database) -> Result<Self> {
        let tx = db.begin_write()?;
        tx.open_table(ORDERS)?;
        tx.open_table(WAITING)?;
        tx.open_table(BANNED_PUBKEYS)?;
        tx.open_table(BANNED_HASHES)?;
        tx.open_table(PENDING_DELETES)?;
        tx.open_table(META)?;
        tx.commit()?;
        Ok(Self { db })
    }

    pub fn put_order(&self, order: &Order) -> Result<()> {
        let json = serde_json::to_string(order).expect("orders always serialize");
        let tx = self.db.begin_write()?;
        {
            tx.open_table(ORDERS)?.insert(order.id.as_str(), json.as_str())?;
            let mut waiting = tx.open_table(WAITING)?;
            match order.status {
                OrderStatus::Pending => waiting.insert(order.id.as_str(), order.created_at)?,
                OrderStatus::Paid => waiting.remove(order.id.as_str())?,
            };
        }
        tx.commit()?;
        Ok(())
    }

    pub fn order(&self, id: &str) -> Result<Option<Order>> {
        let tx = self.db.begin_read()?;
        let table = tx.open_table(ORDERS)?;
        Ok(table
            .get(id)?
            .and_then(|v| serde_json::from_str(v.value()).ok()))
    }

    /// orders created at `since` or later that haven't been paid
    pub fn waiting_orders(&self, since: u64) -> Result<Vec<Order>> {
        let tx = self.db.begin_read()?;
        let waiting = tx.open_table(WAITING)?;
        let orders = tx.open_table(ORDERS)?;
        let mut out = vec![];
        for entry in waiting.iter()? {
            let (id, created_at) = entry?;
            if created_at.value() < since {
                continue;
            }
            if let Some(v) = orders.get(id.value())?
                && let Ok(order) = serde_json::from_str(v.value())
            {
                out.push(order);
            }
        }
        Ok(out)
    }

    fn set_has(&self, def: TableDefinition<&str, u64>, key: &str) -> Result<bool> {
        let tx = self.db.begin_read()?;
        Ok(tx.open_table(def)?.get(key)?.is_some())
    }

    fn set_add(&self, def: TableDefinition<&str, u64>, key: &str, now: u64) -> Result<()> {
        let tx = self.db.begin_write()?;
        tx.open_table(def)?.insert(key, now)?;
        tx.commit()?;
        Ok(())
    }

    fn set_list(&self, def: TableDefinition<&str, u64>) -> Result<Vec<String>> {
        let tx = self.db.begin_read()?;
        let table = tx.open_table(def)?;
        let mut out = vec![];
        for entry in table.iter()? {
            out.push(entry?.0.value().to_string());
        }
        Ok(out)
    }

    pub fn ban_pubkey(&self, pubkey_hex: &str, now: u64) -> Result<()> {
        self.set_add(BANNED_PUBKEYS, pubkey_hex, now)
    }

    pub fn is_pubkey_banned(&self, pubkey_hex: &str) -> Result<bool> {
        self.set_has(BANNED_PUBKEYS, pubkey_hex)
    }

    pub fn banned_pubkeys(&self) -> Result<Vec<String>> {
        self.set_list(BANNED_PUBKEYS)
    }

    pub fn ban_hash(&self, hash_hex: &str, now: u64) -> Result<()> {
        self.set_add(BANNED_HASHES, hash_hex, now)
    }

    pub fn is_hash_banned(&self, hash_hex: &str) -> Result<bool> {
        self.set_has(BANNED_HASHES, hash_hex)
    }

    pub fn banned_hashes(&self) -> Result<Vec<String>> {
        self.set_list(BANNED_HASHES)
    }

    pub fn add_pending_delete(&self, hash_hex: &str, provider: u32) -> Result<()> {
        let tx = self.db.begin_write()?;
        tx.open_table(PENDING_DELETES)?.insert(hash_hex, provider)?;
        tx.commit()?;
        Ok(())
    }

    pub fn remove_pending_delete(&self, hash_hex: &str) -> Result<()> {
        let tx = self.db.begin_write()?;
        tx.open_table(PENDING_DELETES)?.remove(hash_hex)?;
        tx.commit()?;
        Ok(())
    }

    pub fn pending_deletes(&self) -> Result<Vec<(String, u32)>> {
        let tx = self.db.begin_read()?;
        let table = tx.open_table(PENDING_DELETES)?;
        let mut out = vec![];
        for entry in table.iter()? {
            let (k, v) = entry?;
            out.push((k.value().to_string(), v.value()));
        }
        Ok(out)
    }

    pub fn meta(&self, key: &str) -> Result<Option<u64>> {
        let tx = self.db.begin_read()?;
        Ok(tx.open_table(META)?.get(key)?.map(|v| v.value()))
    }

    pub fn set_meta(&self, key: &str, value: u64) -> Result<()> {
        let tx = self.db.begin_write()?;
        tx.open_table(META)?.insert(key, value)?;
        tx.commit()?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn order(id: &str, status: OrderStatus, created_at: u64) -> Order {
        Order {
            id: id.into(),
            created_at,
            kind: OrderKind::Files { files: BTreeMap::new() },
            amount_sats: 5,
            status,
            payment: PaymentRequest {
                method: "mock".into(),
                address: "mock:x".into(),
                uri: "mock:x".into(),
            },
            credits: vec![],
        }
    }

    #[test]
    fn orders_wait_until_paid() {
        let store = Store::temporary().unwrap();
        store.put_order(&order("a", OrderStatus::Pending, 100)).unwrap();
        store.put_order(&order("b", OrderStatus::Pending, 5)).unwrap();
        assert_eq!(store.waiting_orders(50).unwrap().len(), 1);
        assert_eq!(store.waiting_orders(0).unwrap().len(), 2);

        store.put_order(&order("a", OrderStatus::Paid, 100)).unwrap();
        assert!(store.waiting_orders(50).unwrap().is_empty());
        assert_eq!(store.order("a").unwrap().unwrap().status, OrderStatus::Paid);
        assert!(store.order("zzz").unwrap().is_none());
    }

    #[test]
    fn bans_and_meta() {
        let store = Store::temporary().unwrap();
        assert!(!store.is_pubkey_banned("aa").unwrap());
        store.ban_pubkey("aa", 1).unwrap();
        assert!(store.is_pubkey_banned("aa").unwrap());
        assert!(!store.is_hash_banned("aa").unwrap());

        store.add_pending_delete("h", 3).unwrap();
        assert_eq!(store.pending_deletes().unwrap(), vec![("h".to_string(), 3)]);
        store.remove_pending_delete("h").unwrap();
        assert!(store.pending_deletes().unwrap().is_empty());

        assert_eq!(store.meta("x").unwrap(), None);
        store.set_meta("x", 9).unwrap();
        assert_eq!(store.meta("x").unwrap(), Some(9));
    }
}
