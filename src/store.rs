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
/// things the operator tells us: the price of bitcoin, what providers charge
const SETTINGS: TableDefinition<&str, &str> = TableDefinition::new("settings");
/// "<day>:<provider>" -> [DayStats]
const DAILY: TableDefinition<&str, &str> = TableDefinition::new("daily");
/// what the operator wrote next to an entry of the books, by the reference's id in hex. the
/// amounts themselves are in the ledger.
const NOTES: TableDefinition<&str, &str> = TableDefinition::new("notes");

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
    /// what had arrived when the order was settled, which can be more than was asked
    #[serde(default)]
    pub received_sats: Option<u64>,
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

/// what the operator says bitcoin is worth
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq)]
pub struct Rate {
    /// minor units of fiat for 1 BTC
    pub fiat_minor_per_btc: u64,
    pub set_at: u64,
}

/// the arkade wallet set up in the operator interface (when `[payments]` is not in the config
/// file). whoever has the seed has the money.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ArkadeSettings {
    pub server_url: String,
    /// 32 bytes in hex
    pub seed: String,
}

/// what a provider charges, in minor units of fiat. both are optional.
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq)]
pub struct ProviderPrices {
    pub storage_per_gb_month_minor: f64,
    pub egress_per_gb_minor: f64,
}

/// what happened with a provider in a day
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct DayStats {
    /// files charged rent that day, and their size
    pub files: u64,
    pub bytes: u64,
    /// sats the rent brought in
    pub rent_sats: u64,
    /// size of the files people were sent to fetch, which is as close as we get to egress
    pub redirected_bytes: u64,
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
        tx.open_table(SETTINGS)?;
        tx.open_table(DAILY)?;
        tx.open_table(NOTES)?;
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

    fn setting<T: for<'de> Deserialize<'de>>(&self, key: &str) -> Result<Option<T>> {
        let tx = self.db.begin_read()?;
        let table = tx.open_table(SETTINGS)?;
        Ok(table
            .get(key)?
            .and_then(|v| serde_json::from_str(v.value()).ok()))
    }

    fn set_setting<T: Serialize>(&self, key: &str, value: &T) -> Result<()> {
        let json = serde_json::to_string(value).expect("settings always serialize");
        let tx = self.db.begin_write()?;
        tx.open_table(SETTINGS)?.insert(key, json.as_str())?;
        tx.commit()?;
        Ok(())
    }

    pub fn rate(&self) -> Result<Option<Rate>> {
        self.setting("rate")
    }

    pub fn set_rate(&self, rate: Rate) -> Result<()> {
        self.set_setting("rate", &rate)
    }

    pub fn arkade(&self) -> Result<Option<ArkadeSettings>> {
        self.setting("arkade")
    }

    pub fn set_arkade(&self, settings: &ArkadeSettings) -> Result<()> {
        self.set_setting("arkade", settings)
    }

    pub fn prices(&self, provider: u32) -> Result<Option<ProviderPrices>> {
        self.setting(&format!("prices:{provider}"))
    }

    pub fn set_prices(&self, provider: u32, prices: ProviderPrices) -> Result<()> {
        self.set_setting(&format!("prices:{provider}"), &prices)
    }

    pub fn note(&self, reference_hex: &str) -> Result<Option<String>> {
        let tx = self.db.begin_read()?;
        Ok(tx.open_table(NOTES)?.get(reference_hex)?.map(|v| v.value().to_string()))
    }

    pub fn set_note(&self, reference_hex: &str, note: &str) -> Result<()> {
        let tx = self.db.begin_write()?;
        tx.open_table(NOTES)?.insert(reference_hex, note)?;
        tx.commit()?;
        Ok(())
    }

    fn update_day(&self, day: u64, provider: u32, change: impl FnOnce(&mut DayStats)) -> Result<()> {
        let key = format!("{day:010}:{provider:010}");
        let tx = self.db.begin_write()?;
        {
            let mut table = tx.open_table(DAILY)?;
            let mut stats: DayStats = table
                .get(key.as_str())?
                .and_then(|v| serde_json::from_str(v.value()).ok())
                .unwrap_or_default();
            change(&mut stats);
            let json = serde_json::to_string(&stats).expect("stats always serialize");
            table.insert(key.as_str(), json.as_str())?;
        }
        tx.commit()?;
        Ok(())
    }

    /// what the daily sweep saw. sweeping a day twice keeps the bigger numbers, since the
    /// second time files that ran out in the first are gone.
    pub fn record_sweep(&self, day: u64, provider: u32, files: u64, bytes: u64, rent_sats: u64) -> Result<()> {
        self.update_day(day, provider, |s| {
            s.files = s.files.max(files);
            s.bytes = s.bytes.max(bytes);
            s.rent_sats = s.rent_sats.max(rent_sats);
        })
    }

    pub fn add_redirected(&self, day: u64, provider: u32, bytes: u64) -> Result<()> {
        self.update_day(day, provider, |s| s.redirected_bytes += bytes)
    }

    /// every provider's stats for the days from `from` to `to`, both included
    pub fn day_stats(&self, from: u64, to: u64) -> Result<Vec<(u64, u32, DayStats)>> {
        let tx = self.db.begin_read()?;
        let table = tx.open_table(DAILY)?;
        let (lo, hi) = (format!("{from:010}:"), format!("{:010}:", to + 1));
        let mut out = vec![];
        for entry in table.range(lo.as_str()..hi.as_str())? {
            let (k, v) = entry?;
            let Some((day, provider)) = k.value().split_once(':') else { continue };
            if let (Ok(day), Ok(provider), Ok(stats)) =
                (day.parse(), provider.parse(), serde_json::from_str(v.value()))
            {
                out.push((day, provider, stats));
            }
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
            received_sats: None,
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
    fn settings_notes_and_stats() {
        let store = Store::temporary().unwrap();
        assert_eq!(store.rate().unwrap(), None);
        store.set_rate(Rate { fiat_minor_per_btc: 6_000_000, set_at: 5 }).unwrap();
        assert_eq!(store.rate().unwrap().unwrap().fiat_minor_per_btc, 6_000_000);

        assert_eq!(store.prices(1).unwrap(), None);
        let prices = ProviderPrices { storage_per_gb_month_minor: 0.6, egress_per_gb_minor: 1.0 };
        store.set_prices(1, prices).unwrap();
        assert_eq!(store.prices(1).unwrap(), Some(prices));
        assert_eq!(store.prices(2).unwrap(), None);

        store.set_note("ab", "march invoice").unwrap();
        assert_eq!(store.note("ab").unwrap().as_deref(), Some("march invoice"));

        // the sweep of a day can be repeated, redirects add up
        store.record_sweep(10, 1, 3, 300, 30).unwrap();
        store.record_sweep(10, 1, 2, 200, 20).unwrap();
        store.add_redirected(10, 1, 7).unwrap();
        store.add_redirected(10, 1, 8).unwrap();
        store.record_sweep(11, 1, 1, 100, 10).unwrap();
        store.record_sweep(11, 2, 1, 50, 5).unwrap();
        store.record_sweep(12, 1, 1, 100, 10).unwrap();

        let stats = store.day_stats(10, 11).unwrap();
        assert_eq!(stats.len(), 3);
        assert_eq!(stats[0], (10, 1, DayStats { files: 3, bytes: 300, rent_sats: 30, redirected_bytes: 15 }));
        assert_eq!((stats[1].0, stats[1].1, stats[2].0, stats[2].1), (11, 1, 11, 2));
        assert_eq!(store.day_stats(0, 9).unwrap().len(), 0);
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
