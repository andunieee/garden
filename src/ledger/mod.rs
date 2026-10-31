//! the ledger is the only thing that knows about accounts and files. the books are ours, kept
//! with orangedb: how they are laid out is in [schema].
//!
//! there are these kinds of accounts:
//!
//! - **user accounts**, created when the first payment arrives.
//! - **file accounts**, created when a file is uploaded. they keep the size and the storage
//!   provider of the file.
//! - **system accounts** in sats: the treasury, where payments come from; the revenue, where
//!   the daily rent goes (what the operator has earned and not sold yet); the suspense, for
//!   sats we received that no order claimed; and the sold account, where sats go when the
//!   operator sells them for fiat.
//! - accounts in **fiat** (minor units, e.g. cents): a bank account (what the operator has in
//!   fiat), a liquidity account for the exchange and one expense account for each storage
//!   provider. both are fed by what the operator enters: selling sats is a pair of transfers,
//!   one in each currency, and paying a provider's bill is a transfer from the bank to its
//!   expense account.
//!
//! uploading is a transfer from the user to the file account, which is how we can find out who
//! uploaded a file (and what a given user has uploaded), no other record of this exists.
//! user and file accounts can't go below zero. when a file expires its account is closed.

mod orange;
mod schema;

use std::sync::Arc;

use sha2::Digest;

pub use orange::OrangeLedger;

pub const LEDGER_SATS: u32 = 1;
pub const LEDGER_FIAT: u32 = 2;

pub const CODE_USER: u16 = 1;
pub const CODE_FILE: u16 = 2;
pub const CODE_SYSTEM: u16 = 3;
pub const CODE_EXPENSE: u16 = 4;

pub const T_PAYMENT: u16 = 1; // treasury -> user
pub const T_UPLOAD: u16 = 2; // user -> file
pub const T_TOPUP: u16 = 3; // treasury -> file
pub const T_RENT: u16 = 4; // file -> revenue
pub const T_UNCLAIMED: u16 = 6; // treasury -> suspense
pub const T_CONVERT: u16 = 7; // sats sold for fiat (revenue -> sold, and fx -> bank in fiat)
pub const T_EXPENSE: u16 = 8; // bank -> a provider's expense account, in fiat

pub type Hash = [u8; 32];
pub type PubKeyBytes = [u8; 32];

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

/// deterministic key of a transfer, so that repeating an operation is harmless
pub fn transfer_id(parts: &[&[u8]]) -> u128 {
    let mut h = sha2::Sha256::new();
    for p in parts {
        h.update((p.len() as u32).to_be_bytes());
        h.update(p);
    }
    let out: [u8; 32] = h.finalize().into();
    u128::from_be_bytes(out[..16].try_into().unwrap())
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UserInfo {
    pub pubkey: PubKeyBytes,
    pub balance: u64,
    /// when the account was created, in unix nanoseconds
    pub timestamp: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileInfo {
    pub hash: Hash,
    pub size: u64,
    pub provider: u32,
    pub balance: u64,
    pub closed: bool,
    /// when the account was created, in unix nanoseconds
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

/// what was taken from a file as rent, and what it has left after that
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Charged {
    pub taken: u64,
    pub left: u64,
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

    /// [Ledger::charge_file] for many files, all with the same `charge_id`, in a few
    /// transactions instead of one per file. each file gets its own result, in the same order.
    async fn charge_files(&self, charges: &[(Hash, u64)], charge_id: u128) -> Result<Vec<Result<Charged>>>;

    /// no more credits and no more serving. the files can come back if uploaded again.
    async fn close_file(&self, hash: &Hash) -> Result<()>;

    /// everyone who funded the creation of this file, oldest first
    async fn uploaders(&self, hash: &Hash) -> Result<Vec<PubKeyBytes>>;

    /// all the files this user has funded the creation of
    async fn files_by_uploader(&self, pubkey: &PubKeyBytes) -> Result<Vec<Hash>>;

    /// every file account ever created, oldest first
    async fn files(&self) -> Result<Vec<FileInfo>>;

    /// every user account, oldest first
    async fn users(&self) -> Result<Vec<UserInfo>>;

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

        // and the same in batches: each file on its own, and repeating is harmless
        let (h2, h3) = ([0x12; 32], [0x13; 32]);
        ledger.credit_user(&alice, 30, 78).await.unwrap();
        ledger.create_file(&h2, 5, 7, &alice, 20).await.unwrap();
        ledger.create_file(&h3, 5, 7, &alice, 10).await.unwrap();
        let batch = [(h2, 15), (h3, 15), ([0x99; 32], 15)];
        for _ in 0..2 {
            let out = ledger.charge_files(&batch, 12).await.unwrap();
            assert_eq!(out[0].as_ref().unwrap(), &Charged { taken: 15, left: 5 });
            assert_eq!(out[1].as_ref().unwrap(), &Charged { taken: 10, left: 0 });
            assert!(matches!(out[2], Err(LedgerError::NoSuchAccount)));
        }
        ledger.close_file(&h2).await.unwrap();
        ledger.close_file(&h3).await.unwrap();

        // listing
        let listed = ledger.files().await.unwrap();
        assert_eq!(listed.iter().map(|f| f.hash).collect::<Vec<_>>(), vec![hash, h2, h3]);

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
        assert_eq!(ledger.files_by_uploader(&alice).await.unwrap(), vec![hash, h2, h3]);
        ledger.close_file(&hash).await.unwrap();
        assert!(ledger.file(&hash).await.unwrap().unwrap().closed);

        books(ledger, alice).await;
    }

    /// the operator's side. continues from the end of [run]: alice has 600 sats, the one file
    /// has 150 and has been closed (and two more, with 5 and 0), 375 sats of rent were earned
    /// and 1130 sats received
    async fn books(ledger: &dyn Ledger, alice: PubKeyBytes) {
        let t = ledger.totals().await.unwrap();
        assert_eq!((t.received, t.earned, t.sold, t.unclaimed), (1130, 375, 0, 0));
        assert_eq!((t.fiat_in, t.fiat_out), (0, 0));

        // money nobody claimed, once per id
        ledger.record_unclaimed(30, 5).await.unwrap();
        ledger.record_unclaimed(30, 5).await.unwrap();
        ledger.record_unclaimed(0, 6).await.unwrap();
        let t = ledger.totals().await.unwrap();
        assert_eq!((t.received, t.unclaimed), (1160, 30));

        // selling sats moves them out of the revenue and fiat into the bank
        assert!(ledger.record_conversion("receipt-1", 200, 5000).await.unwrap());
        assert!(!ledger.record_conversion("receipt-1", 200, 5000).await.unwrap());
        assert!(matches!(
            ledger.record_conversion("receipt-1", 300, 5000).await,
            Err(LedgerError::ReferenceReused)
        ));
        let t = ledger.totals().await.unwrap();
        assert_eq!((t.sold, t.fiat_in, t.earned), (200, 5000, 375));
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
        let users = ledger.users().await.unwrap();
        assert_eq!(users.len(), 1);
        assert_eq!((users[0].pubkey, users[0].balance), (alice, 600));

        // and everything adds up: what came in is somewhere
        let users: u64 = ledger.users().await.unwrap().iter().map(|u| u.balance).sum();
        let files: u64 = ledger.files().await.unwrap().iter().map(|f| f.balance).sum();
        let held = users + files;
        let t = ledger.totals().await.unwrap();
        assert_eq!(t.received, held + t.earned + t.unclaimed);
    }
}
