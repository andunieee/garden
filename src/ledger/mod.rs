//! the ledger is the only thing that knows about accounts and files.
//!
//! there are three kinds of accounts, all in the same ledger (sats):
//!
//! - **user accounts**, created when the first payment arrives. the id is the first half of the
//!   pubkey and `user_data_128` has the second half, so the pubkey is fully recoverable.
//! - **file accounts**, created when a file is uploaded. same trick with the sha256 of the file.
//!   `user_data_64` has the size and `user_data_32` the id of the storage provider.
//! - **system accounts** in the sats ledger: the treasury, where payments come from; the
//!   revenue, where the daily rent goes (what the operator has earned and not sold yet); the
//!   suspense, for sats we received that no order claimed; and the sold account, where sats
//!   go when the operator sells them for fiat.
//! - a second ledger in **fiat** (minor units, e.g. cents) with a bank account (what the
//!   operator has in fiat), a liquidity account for the exchange and one expense account for
//!   each storage provider. Both are fed by what the operator enters: selling sats is a pair
//!   of linked transfers, one in each ledger, and paying a provider's bill is a transfer from
//!   the bank to its expense account.
//!
//! uploading is a transfer from the user to the file account, which is how we can find out who
//! uploaded a file (and what a given user has uploaded), no other record of this exists.
//! file accounts can't go below zero. when a file expires its account is closed.

mod memory;
mod tigerbeetle;

use std::sync::Arc;

use sha2::Digest;

pub use memory::MemoryLedger;
pub use tigerbeetle::TigerBeetleLedger;

pub const LEDGER_SATS: u32 = 1;
pub const LEDGER_FIAT: u32 = 2;

pub const CODE_USER: u16 = 1;
pub const CODE_FILE: u16 = 2;
pub const CODE_SYSTEM: u16 = 3;
pub const CODE_EXPENSE: u16 = 4;

pub const TREASURY_ID: u128 = 1;
pub const REVENUE_ID: u128 = 2;
pub const SUSPENSE_ID: u128 = 3;
pub const SOLD_ID: u128 = 4;
pub const BANK_ID: u128 = 5;
pub const FX_FIAT_ID: u128 = 6;

pub const T_PAYMENT: u16 = 1; // treasury -> user
pub const T_UPLOAD: u16 = 2; // user -> file
pub const T_TOPUP: u16 = 3; // treasury -> file
pub const T_RENT: u16 = 4; // file -> revenue
pub const T_CLOSE: u16 = 5; // closes a file account
pub const T_UNCLAIMED: u16 = 6; // treasury -> suspense
pub const T_CONVERT: u16 = 7; // sats sold for fiat (revenue -> sold, and fx -> bank in fiat)
pub const T_EXPENSE: u16 = 8; // bank -> a provider's expense account, in fiat

pub type Hash = [u8; 32];
pub type PubKeyBytes = [u8; 32];

/// first half of 32 bytes, as an account id
pub fn account_id(bytes: &[u8; 32]) -> u128 {
    u128::from_be_bytes(bytes[..16].try_into().unwrap())
}

/// second half of 32 bytes, to be kept in `user_data_128`
pub fn account_rest(bytes: &[u8; 32]) -> u128 {
    u128::from_be_bytes(bytes[16..].try_into().unwrap())
}

pub fn join_account(id: u128, rest: u128) -> [u8; 32] {
    let mut out = [0u8; 32];
    out[..16].copy_from_slice(&id.to_be_bytes());
    out[16..].copy_from_slice(&rest.to_be_bytes());
    out
}

/// ids that can't be used in tigerbeetle or collide with the system accounts
pub fn valid_account_id(id: u128) -> bool {
    id > 0xFFFF && id != u128::MAX
}

/// the fiat account where the bills of a provider are recorded
pub fn expense_id(provider: u32) -> u128 {
    (1u128 << 100) | provider as u128
}

/// the same, as the key the notes about an entry are kept under
pub fn reference_hex(reference: &str) -> String {
    reference_hex_of(reference_id(reference))
}

pub fn reference_hex_of(id: u128) -> String {
    lowercase_hex::encode(id.to_be_bytes())
}

/// what we keep in the ledger of the free text the operator types to identify an entry (an
/// invoice number, an exchange receipt). entering the same reference twice is a no-op.
pub fn reference_id(reference: &str) -> u128 {
    transfer_id(&[b"ref", reference.as_bytes()])
}

/// deterministic transfer id, so that repeating an operation is harmless
pub fn transfer_id(parts: &[&[u8]]) -> u128 {
    let mut h = sha2::Sha256::new();
    for p in parts {
        h.update((p.len() as u32).to_be_bytes());
        h.update(p);
    }
    let out: [u8; 32] = h.finalize().into();
    let id = u128::from_be_bytes(out[..16].try_into().unwrap());
    if id == 0 || id == u128::MAX { 1 << 100 } else { id }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UserInfo {
    pub pubkey: PubKeyBytes,
    pub balance: u64,
    /// when the account was created, in nanoseconds (this is what paginates listings)
    pub timestamp: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileInfo {
    pub hash: Hash,
    pub size: u64,
    pub provider: u32,
    pub balance: u64,
    pub closed: bool,
    /// when the account was created, in nanoseconds (this is what paginates listings)
    pub timestamp: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Credited {
    Yes,
    /// the same credit had been applied before
    AlreadyDone,
    /// there is no file, or it is gone
    NoSuchFile,
}

#[derive(thiserror::Error, Debug)]
pub enum LedgerError {
    #[error("not enough sats in the account")]
    InsufficientBalance,
    #[error("a file with this hash exists already")]
    FileExists,
    #[error("no such account")]
    NoSuchAccount,
    #[error("that reference was used before with other values")]
    ReferenceReused,
    #[error("ledger failure: {0}")]
    Other(String),
}

pub type Result<T> = std::result::Result<T, LedgerError>;

/// what the system accounts add up to, all in sats unless the name says otherwise
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Totals {
    /// everything that ever arrived through payments
    pub received: u64,
    /// everything rent has ever brought in
    pub earned: u64,
    /// of which the operator has sold for fiat
    pub sold: u64,
    /// received but not claimed by any order (overpayments, payments for files that expired)
    pub unclaimed: u64,
    /// fiat (minor units) received from selling sats
    pub fiat_in: u64,
    /// fiat (minor units) paid to providers
    pub fiat_out: u64,
}

/// a bill the operator paid to a storage provider
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Expense {
    pub provider: u32,
    pub amount_minor: u64,
    /// the date of the bill, in unix seconds
    pub date: u64,
    pub timestamp: u64,
    pub reference: u128,
}

/// sats the operator sold
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Conversion {
    pub sats: u64,
    pub fiat_minor: u64,
    pub timestamp: u64,
    pub reference: u128,
}

#[async_trait::async_trait]
pub trait Ledger: Send + Sync {
    async fn user(&self, pubkey: &PubKeyBytes) -> Result<Option<UserInfo>>;

    /// add sats to the user account, creating it if it doesn't exist. calling this twice with
    /// the same `payment_id` credits only once.
    async fn credit_user(&self, pubkey: &PubKeyBytes, sats: u64, payment_id: u128) -> Result<()>;

    async fn file(&self, hash: &Hash) -> Result<Option<FileInfo>>;

    /// create the account of a file (or reopen the one of a file that expired) and move `funding`
    /// sats into it from the user. fails with [LedgerError::FileExists] if it is alive already
    /// and [LedgerError::InsufficientBalance] if the user can't pay.
    async fn create_file(
        &self,
        hash: &Hash,
        size: u64,
        provider: u32,
        uploader: &PubKeyBytes,
        funding: u64,
    ) -> Result<()>;

    /// add sats to a file coming from nobody in particular. idempotent for each `payment_id`.
    async fn credit_file(&self, hash: &Hash, sats: u64, payment_id: u128) -> Result<Credited>;

    /// take up to `sats` from a file into the revenue account. idempotent for each `charge_id`.
    /// returns how much was actually taken.
    async fn charge_file(&self, hash: &Hash, sats: u64, charge_id: u128) -> Result<u64>;

    /// no more credits and no more serving. the files can come back if uploaded again.
    async fn close_file(&self, hash: &Hash) -> Result<()>;

    /// everyone who funded the creation of this file, oldest first
    async fn uploaders(&self, hash: &Hash) -> Result<Vec<PubKeyBytes>>;

    /// all the files this user has funded the creation of
    async fn files_by_uploader(&self, pubkey: &PubKeyBytes) -> Result<Vec<Hash>>;

    /// file accounts created at or after `since` (see [FileInfo::timestamp]), oldest first
    async fn list_files(&self, since: u64, limit: usize) -> Result<Vec<FileInfo>>;

    /// user accounts created at or after `since` (see [UserInfo::timestamp]), oldest first
    async fn list_users(&self, since: u64, limit: usize) -> Result<Vec<UserInfo>>;

    // the books of the operator

    async fn totals(&self) -> Result<Totals>;

    /// sats that were received and that no order claimed, so the ledger still matches the
    /// wallet. idempotent for each `id`.
    async fn record_unclaimed(&self, sats: u64, id: u128) -> Result<()>;

    /// the operator sold `sats` and got `fiat_minor` for them. false if this `reference` had
    /// been recorded already, an error if with other numbers.
    async fn record_conversion(&self, reference: &str, sats: u64, fiat_minor: u64) -> Result<bool>;

    /// the operator paid a bill of `amount_minor` to a provider, dated `date` (unix seconds).
    /// false if this `reference` had been recorded already.
    async fn record_expense(
        &self,
        provider: u32,
        reference: &str,
        amount_minor: u64,
        date: u64,
    ) -> Result<bool>;

    async fn expenses(&self, provider: u32) -> Result<Vec<Expense>>;

    async fn conversions(&self) -> Result<Vec<Conversion>>;
}

pub type SharedLedger = Arc<dyn Ledger>;

/// every file account ever created, in pages
pub async fn all_files(ledger: &dyn Ledger, mut each: impl FnMut(FileInfo)) -> Result<()> {
    let mut since = 0;
    loop {
        let page = ledger.list_files(since, 1000).await?;
        let Some(last) = page.last() else {
            return Ok(());
        };
        since = last.timestamp + 1;
        let full = page.len() >= 1000;
        page.into_iter().for_each(&mut each);
        if !full {
            return Ok(());
        }
    }
}

/// every user account, in pages
pub async fn all_users(ledger: &dyn Ledger, mut each: impl FnMut(UserInfo)) -> Result<()> {
    let mut since = 0;
    loop {
        let page = ledger.list_users(since, 1000).await?;
        let Some(last) = page.last() else {
            return Ok(());
        };
        since = last.timestamp + 1;
        let full = page.len() >= 1000;
        page.into_iter().for_each(&mut each);
        if !full {
            return Ok(());
        }
    }
}

#[cfg(test)]
pub(crate) mod conformance {
    //! tests that every [Ledger] implementation must pass

    use super::*;

    pub async fn run(ledger: &dyn Ledger) {
        let alice = [0xaa; 32];
        let bob = [0xbb; 32];
        let hash = [0x11; 32];

        // accounts appear with the first payment, and payments apply once
        assert_eq!(ledger.user(&alice).await.unwrap(), None);
        ledger.credit_user(&alice, 1000, 77).await.unwrap();
        ledger.credit_user(&alice, 1000, 77).await.unwrap();
        assert_eq!(ledger.user(&alice).await.unwrap().unwrap().balance, 1000);
        assert_eq!(ledger.user(&alice).await.unwrap().unwrap().pubkey, alice);

        // can't fund a file without money
        assert!(matches!(
            ledger.create_file(&hash, 5, 1, &bob, 10).await,
            Err(LedgerError::NoSuchAccount) | Err(LedgerError::InsufficientBalance)
        ));
        assert!(matches!(
            ledger.create_file(&hash, 5, 1, &alice, 5000).await,
            Err(LedgerError::InsufficientBalance)
        ));

        // funding moves the sats and leaves a trace of who did it
        ledger.create_file(&hash, 5, 7, &alice, 300).await.unwrap();
        assert_eq!(ledger.user(&alice).await.unwrap().unwrap().balance, 700);
        let f = ledger.file(&hash).await.unwrap().unwrap();
        assert_eq!((f.size, f.provider, f.balance, f.closed), (5, 7, 300, false));
        assert_eq!(ledger.uploaders(&hash).await.unwrap(), vec![alice]);
        assert_eq!(ledger.files_by_uploader(&alice).await.unwrap(), vec![hash]);
        assert!(ledger.files_by_uploader(&bob).await.unwrap().is_empty());
        assert!(matches!(
            ledger.create_file(&hash, 5, 7, &alice, 10).await,
            Err(LedgerError::FileExists)
        ));

        // anyone can add to a file, once per payment
        assert_eq!(ledger.credit_file(&hash, 50, 1).await.unwrap(), Credited::Yes);
        assert_eq!(ledger.credit_file(&hash, 50, 1).await.unwrap(), Credited::AlreadyDone);
        assert_eq!(ledger.credit_file(&[0x22; 32], 50, 2).await.unwrap(), Credited::NoSuchFile);
        assert_eq!(ledger.file(&hash).await.unwrap().unwrap().balance, 350);
        // credits to files are not uploads
        assert_eq!(ledger.uploaders(&hash).await.unwrap(), vec![alice]);

        // rent is taken once per charge id and never more than there is
        assert_eq!(ledger.charge_file(&hash, 100, 10).await.unwrap(), 100);
        assert_eq!(ledger.charge_file(&hash, 100, 10).await.unwrap(), 100);
        assert_eq!(ledger.file(&hash).await.unwrap().unwrap().balance, 250);
        assert_eq!(ledger.charge_file(&hash, 1000, 11).await.unwrap(), 250);
        assert_eq!(ledger.file(&hash).await.unwrap().unwrap().balance, 0);

        // listing
        let listed = ledger.list_files(0, 10).await.unwrap();
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].hash, hash);
        assert!(ledger.list_files(listed[0].timestamp + 1, 10).await.unwrap().is_empty());

        // closed files take no more credits
        ledger.close_file(&hash).await.unwrap();
        ledger.close_file(&hash).await.unwrap();
        assert!(ledger.file(&hash).await.unwrap().unwrap().closed);
        assert_eq!(ledger.credit_file(&hash, 50, 3).await.unwrap(), Credited::NoSuchFile);

        // and come back when uploaded again
        ledger.create_file(&hash, 5, 7, &alice, 100).await.unwrap();
        let f = ledger.file(&hash).await.unwrap().unwrap();
        assert_eq!((f.balance, f.closed), (100, false));
        assert_eq!(ledger.credit_file(&hash, 50, 4).await.unwrap(), Credited::Yes);
        assert_eq!(ledger.uploaders(&hash).await.unwrap(), vec![alice, alice]);
        assert_eq!(ledger.files_by_uploader(&alice).await.unwrap().len(), 1);
        ledger.close_file(&hash).await.unwrap();
        assert!(ledger.file(&hash).await.unwrap().unwrap().closed);

        books(ledger, alice).await;
    }

    /// the operator's side. continues from the end of [run]: alice has 600 sats, the one file
    /// has 150 and has been closed, 350 sats of rent were earned and 1100 sats received
    async fn books(ledger: &dyn Ledger, alice: PubKeyBytes) {
        let t = ledger.totals().await.unwrap();
        assert_eq!((t.received, t.earned, t.sold, t.unclaimed), (1100, 350, 0, 0));
        assert_eq!((t.fiat_in, t.fiat_out), (0, 0));

        // money nobody claimed, once per id
        ledger.record_unclaimed(30, 5).await.unwrap();
        ledger.record_unclaimed(30, 5).await.unwrap();
        ledger.record_unclaimed(0, 6).await.unwrap();
        let t = ledger.totals().await.unwrap();
        assert_eq!((t.received, t.unclaimed), (1130, 30));

        // selling sats moves them out of the revenue and fiat into the bank
        assert!(ledger.record_conversion("receipt-1", 200, 5000).await.unwrap());
        assert!(!ledger.record_conversion("receipt-1", 200, 5000).await.unwrap());
        assert!(matches!(
            ledger.record_conversion("receipt-1", 300, 5000).await,
            Err(LedgerError::ReferenceReused)
        ));
        let t = ledger.totals().await.unwrap();
        assert_eq!((t.sold, t.fiat_in, t.earned), (200, 5000, 350));
        let c = ledger.conversions().await.unwrap();
        assert_eq!(c.len(), 1);
        assert_eq!((c[0].sats, c[0].fiat_minor, c[0].reference), (200, 5000, reference_id("receipt-1")));

        // bills
        let date = 1_700_000_000;
        assert!(ledger.record_expense(7, "inv-1", 1200, date).await.unwrap());
        assert!(!ledger.record_expense(7, "inv-1", 1200, date).await.unwrap());
        assert!(ledger.record_expense(7, "inv-2", 300, date + 1).await.unwrap());
        assert!(ledger.record_expense(8, "inv-1", 99, date).await.unwrap());
        assert_eq!(ledger.totals().await.unwrap().fiat_out, 1599);
        let e = ledger.expenses(7).await.unwrap();
        assert_eq!(e.iter().map(|e| e.amount_minor).sum::<u64>(), 1500);
        assert!(e.iter().all(|e| e.provider == 7));
        assert_eq!(e.iter().map(|e| e.date).collect::<Vec<_>>(), vec![date, date + 1]);
        assert_eq!(ledger.expenses(9).await.unwrap(), vec![]);

        // users can be listed
        let users = ledger.list_users(0, 10).await.unwrap();
        assert_eq!(users.len(), 1);
        assert_eq!((users[0].pubkey, users[0].balance), (alice, 600));
        assert!(ledger.list_users(users[0].timestamp + 1, 10).await.unwrap().is_empty());

        // and everything adds up: what came in is somewhere
        let mut held = 0;
        all_users(ledger, |u| held += u.balance).await.unwrap();
        all_files(ledger, |f| held += f.balance).await.unwrap();
        let t = ledger.totals().await.unwrap();
        assert_eq!(t.received, held + t.earned + t.unclaimed);
    }
}
