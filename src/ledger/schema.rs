//! the tables of the books, kept with orangedb in `ledger.redb`.
//!
//! it is double-entry: every sat (and every cent) that moves is a [Transfer] from one [Account]
//! to another in the same currency, and the transfer and the new totals of both accounts are
//! written in the same transaction. accounts keep running totals of what was debited and
//! credited, so balances never have to be summed up from the transfers, and in each currency
//! the debits of all accounts always add up to their credits.
//!
//! [User] and [File] are what gives meaning to an account: each points at its own account. the
//! system accounts (treasury, revenue, ...) and the expense account of each provider have a
//! unique `name` instead.
//!
//! uploads are transfers from the account of a user to the account of a file, and that is the
//! only record of who uploaded what: the indexes on [Transfer::debit] and [Transfer::credit]
//! answer it both ways.

#[orangedb::schema]
pub mod books {
    use serde::{Deserialize, Serialize};

    #[entity]
    #[derive(Debug, Clone, PartialEq)]
    pub struct Account {
        /// in the order they were created
        #[id(auto)]
        pub id: AccountId,
        /// only system and expense accounts have one: "treasury", "revenue", "expense/7", ...
        #[unique]
        pub name: Option<String>,
        /// LEDGER_SATS or LEDGER_FIAT. transfers only happen inside one
        pub ledger: u32,
        /// CODE_USER, CODE_FILE, ...
        pub code: u16,
        pub debits: u64,
        pub credits: u64,
        /// users and files can't give what they don't have
        pub debits_must_not_exceed_credits: bool,
        /// a closed account takes no transfers. files are closed when they expire and opened
        /// again when they are uploaded again.
        pub closed: bool,
        /// unix nanoseconds
        pub created_at: u64,
    }

    #[entity]
    #[derive(Debug, Clone, PartialEq)]
    pub struct User {
        #[id([u8; 32])]
        pub pubkey: Pubkey,
        #[references(Account)]
        #[unique]
        pub account: AccountId,
    }

    #[entity]
    #[derive(Debug, Clone, PartialEq)]
    pub struct File {
        #[id([u8; 32])]
        pub hash: FileHash,
        #[references(Account)]
        #[unique]
        pub account: AccountId,
        pub size: u64,
        /// the storage provider it was put in
        pub provider: u32,
    }

    #[entity]
    #[derive(Debug, Clone, PartialEq)]
    pub struct Transfer {
        /// in the order they were made
        #[id(auto)]
        pub id: TransferId,
        /// what makes repeating an operation harmless (see `transfer_id`): a
        /// second transfer with the same key is refused. uploads have none.
        #[unique]
        pub key: Option<u128>,
        #[references(Account)]
        pub debit: AccountId,
        #[references(Account)]
        pub credit: AccountId,
        pub amount: u64,
        /// T_PAYMENT, T_UPLOAD, ...
        pub code: u16,
        /// unix nanoseconds
        pub at: u64,
        pub memo: Memo,
    }

    /// what a transfer says besides moving money
    #[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
    pub enum Memo {
        None,
        /// the day of rent a file paid, and to which provider it went
        Rent { provider: u32, day: u64 },
        /// both halves of a sale of sats carry the whole of it
        Conversion { reference: u128, sats: u64, fiat_minor: u64 },
        /// a bill paid to a provider, `date` in unix seconds
        Expense { provider: u32, reference: u128, date: u64 },
    }
}
