use std::path::Path;
use std::sync::Arc;

use super::schema::books::*;
use super::*;

/// the books, kept by us in an orangedb database (see [super::schema])
pub struct OrangeLedger {
    db: Arc<Db>,
    system: System,
}

/// the ids of the system accounts, which never change once created
#[derive(Clone, Copy)]
struct System {
    treasury: AccountId,
    revenue: AccountId,
    suspense: AccountId,
    sold: AccountId,
    bank: AccountId,
    fx: AccountId,
}

impl From<orangedb::Error> for LedgerError {
    fn from(e: orangedb::Error) -> Self {
        LedgerError::Other(e.to_string())
    }
}

fn other(e: impl std::fmt::Display) -> LedgerError {
    LedgerError::Other(e.to_string())
}

fn nanos() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0)
}

fn expense_name(provider: u32) -> String {
    format!("expense/{provider}")
}

fn balance(a: &Account) -> u64 {
    a.credits.saturating_sub(a.debits)
}

impl OrangeLedger {
    pub fn open(path: impl AsRef<Path>) -> Result<Self> {
        if let Some(dir) = path.as_ref().parent() {
            std::fs::create_dir_all(dir).map_err(other)?;
        }
        Self::from_db(Db::open(path)?)
    }

    /// forgets everything when dropped, for tests and development
    pub fn in_memory() -> Result<Self> {
        Self::from_db(Db::in_memory()?)
    }

    fn from_db(db: Db) -> Result<Self> {
        let system = db.write(|tx| {
            let account = |name: &str, ledger| named_account(tx, name, ledger, CODE_SYSTEM);
            Ok::<_, LedgerError>(System {
                treasury: account("treasury", LEDGER_SATS)?,
                revenue: account("revenue", LEDGER_SATS)?,
                suspense: account("suspense", LEDGER_SATS)?,
                sold: account("sold", LEDGER_SATS)?,
                bank: account("bank", LEDGER_FIAT)?,
                fx: account("fx", LEDGER_FIAT)?,
            })
        })?;
        Ok(Self { db: Arc::new(db), system })
    }

    /// redb waits for the disk on every commit, so this runs away from the async workers
    async fn write<T: Send + 'static>(
        &self,
        f: impl FnOnce(&WriteTx, System) -> Result<T> + Send + 'static,
    ) -> Result<T> {
        let (db, system) = (self.db.clone(), self.system);
        tokio::task::spawn_blocking(move || db.write(|tx| f(tx, system)))
            .await
            .map_err(other)?
    }

    async fn read<T: Send + 'static>(
        &self,
        f: impl FnOnce(&ReadTx, System) -> Result<T> + Send + 'static,
    ) -> Result<T> {
        let (db, system) = (self.db.clone(), self.system);
        tokio::task::spawn_blocking(move || db.read(|tx| f(tx, system)))
            .await
            .map_err(other)?
    }
}

/// the id of the account with this name, created if it doesn't exist
fn named_account(tx: &WriteTx, name: &str, ledger: u32, code: u16) -> Result<AccountId> {
    if let Some(a) = tx.accounts().by_name(name)? {
        return Ok(a.id);
    }
    Ok(new_account(tx, Some(name.to_string()), ledger, code, false)?.id)
}

fn new_account(
    tx: &WriteTx,
    name: Option<String>,
    ledger: u32,
    code: u16,
    debits_must_not_exceed_credits: bool,
) -> Result<Account> {
    Ok(tx.accounts().create(|id| Account {
        id,
        name,
        ledger,
        code,
        debits: 0,
        credits: 0,
        debits_must_not_exceed_credits,
        closed: false,
        created_at: nanos(),
    })?)
}

fn account(tx: &WriteTx, id: AccountId) -> Result<Account> {
    tx.accounts().get(id)?.ok_or(LedgerError::NoSuchAccount)
}

/// the one place where money moves: records the transfer and updates both accounts
fn post(
    tx: &WriteTx,
    key: Option<u128>,
    debit: AccountId,
    credit: AccountId,
    amount: u64,
    code: u16,
    memo: Memo,
) -> Result<()> {
    let mut from = account(tx, debit)?;
    let mut to = account(tx, credit)?;
    if from.ledger != to.ledger {
        return Err(other("transfer between two currencies"));
    }
    if from.closed || to.closed {
        return Err(other("transfer with a closed account"));
    }
    if from.debits_must_not_exceed_credits && balance(&from) < amount {
        return Err(LedgerError::InsufficientBalance);
    }
    from.debits = from.debits.checked_add(amount).ok_or_else(|| other("overflow"))?;
    to.credits = to.credits.checked_add(amount).ok_or_else(|| other("overflow"))?;
    tx.accounts().update(&from)?;
    tx.accounts().update(&to)?;
    tx.transfers().create(|id| Transfer {
        id,
        key,
        debit,
        credit,
        amount,
        code,
        at: nanos(),
        memo,
    })?;
    Ok(())
}

/// take up to `sats` of rent from a file, once per `charge_id`
fn charge(tx: &WriteTx, sys: System, hash: &Hash, sats: u64, charge_id: u128) -> Result<Charged> {
    let f = tx.files().get(FileHash(*hash))?.ok_or(LedgerError::NoSuchAccount)?;
    let a = account(tx, f.account)?;
    let key = transfer_id(&[b"rent", &charge_id.to_be_bytes(), hash]);
    // already charged: say how much it was
    if let Some(t) = tx.transfers().by_key(key)? {
        return Ok(Charged {
            taken: t.amount,
            left: balance(&a),
        });
    }
    let taken = sats.min(balance(&a));
    if taken == 0 || a.closed {
        return Ok(Charged {
            taken: 0,
            left: balance(&a),
        });
    }
    let memo = Memo::Rent {
        provider: f.provider,
        day: charge_id as u64,
    };
    post(tx, Some(key), a.id, sys.revenue, taken, T_RENT, memo)?;
    Ok(Charged {
        taken,
        left: balance(&a) - taken,
    })
}

fn user_info(u: &User, a: &Account) -> UserInfo {
    UserInfo {
        pubkey: u.pubkey.0,
        balance: balance(a),
        timestamp: a.created_at,
    }
}

fn file_info(f: &File, a: &Account) -> FileInfo {
    FileInfo {
        hash: f.hash.0,
        size: f.size,
        provider: f.provider,
        balance: balance(a),
        closed: a.closed,
        timestamp: a.created_at,
    }
}

/// the same queries in read and write transactions
macro_rules! lookups {
    ($tx:expr) => {{
        let tx = $tx;
        let user = |pubkey: &PubKeyBytes| -> Result<Option<(User, Account)>> {
            let Some(u) = tx.users().get(Pubkey(*pubkey))? else { return Ok(None) };
            let a = tx.accounts().get(u.account)?.ok_or(LedgerError::NoSuchAccount)?;
            Ok(Some((u, a)))
        };
        let file = |hash: &Hash| -> Result<Option<(File, Account)>> {
            let Some(f) = tx.files().get(FileHash(*hash))? else { return Ok(None) };
            let a = tx.accounts().get(f.account)?.ok_or(LedgerError::NoSuchAccount)?;
            Ok(Some((f, a)))
        };
        (user, file)
    }};
}

#[async_trait::async_trait]
impl Ledger for OrangeLedger {
    async fn user(&self, pubkey: &PubKeyBytes) -> Result<Option<UserInfo>> {
        let pubkey = *pubkey;
        self.read(move |tx, _| {
            let (user, _) = lookups!(tx);
            Ok(user(&pubkey)?.map(|(u, a)| user_info(&u, &a)))
        })
        .await
    }

    async fn credit_user(&self, pubkey: &PubKeyBytes, sats: u64, payment_id: u128) -> Result<()> {
        let pubkey = *pubkey;
        self.write(move |tx, sys| {
            let account = match tx.users().get(Pubkey(pubkey))? {
                Some(u) => u.account,
                None => {
                    let account = new_account(tx, None, LEDGER_SATS, CODE_USER, true)?.id;
                    tx.users().insert(&User { pubkey: Pubkey(pubkey), account })?;
                    account
                }
            };
            let key = transfer_id(&[b"pay", &payment_id.to_be_bytes()]);
            if tx.transfers().by_key(key)?.is_none() {
                post(tx, Some(key), sys.treasury, account, sats, T_PAYMENT, Memo::None)?;
            }
            Ok(())
        })
        .await
    }

    async fn file(&self, hash: &Hash) -> Result<Option<FileInfo>> {
        let hash = *hash;
        self.read(move |tx, _| {
            let (_, file) = lookups!(tx);
            Ok(file(&hash)?.map(|(f, a)| file_info(&f, &a)))
        })
        .await
    }

    async fn create_file(
        &self,
        hash: &Hash,
        size: u64,
        provider: u32,
        uploader: &PubKeyBytes,
        funding: u64,
    ) -> Result<()> {
        let (hash, uploader) = (*hash, *uploader);
        self.write(move |tx, _| {
            let (user, file) = lookups!(tx);
            let (_, from) = user(&uploader)?.ok_or(LedgerError::NoSuchAccount)?;
            if balance(&from) < funding {
                return Err(LedgerError::InsufficientBalance);
            }

            let to = match file(&hash)? {
                Some((_, a)) if !a.closed => return Err(LedgerError::FileExists),
                // it expired before, and comes back as it was
                Some((_, mut a)) => {
                    a.closed = false;
                    tx.accounts().update(&a)?;
                    a.id
                }
                None => {
                    let account = new_account(tx, None, LEDGER_SATS, CODE_FILE, true)?.id;
                    tx.files().insert(&File {
                        hash: FileHash(hash),
                        account,
                        size,
                        provider,
                    })?;
                    account
                }
            };
            post(tx, None, from.id, to, funding, T_UPLOAD, Memo::None)
        })
        .await
    }

    async fn credit_file(&self, hash: &Hash, sats: u64, payment_id: u128) -> Result<Credited> {
        let hash = *hash;
        self.write(move |tx, sys| {
            let (_, file) = lookups!(tx);
            let key = transfer_id(&[b"topup", &payment_id.to_be_bytes(), &hash]);
            if tx.transfers().by_key(key)?.is_some() {
                return Ok(Credited::AlreadyDone);
            }
            match file(&hash)? {
                Some((_, a)) if !a.closed => {
                    post(tx, Some(key), sys.treasury, a.id, sats, T_TOPUP, Memo::None)?;
                    Ok(Credited::Yes)
                }
                _ => Ok(Credited::NoSuchFile),
            }
        })
        .await
    }

    async fn charge_file(&self, hash: &Hash, sats: u64, charge_id: u128) -> Result<u64> {
        let hash = *hash;
        self.write(move |tx, sys| Ok(charge(tx, sys, &hash, sats, charge_id)?.taken))
            .await
    }

    async fn charge_files(&self, charges: &[(Hash, u64)], charge_id: u128) -> Result<Vec<Result<Charged>>> {
        // big enough that a disk sync per transaction doesn't matter, small enough that other
        // writers don't wait long behind it
        const CHUNK: usize = 1000;
        let mut out = Vec::with_capacity(charges.len());
        for chunk in charges.chunks(CHUNK) {
            let chunk = chunk.to_vec();
            out.extend(
                self.write(move |tx, sys| {
                    // a file that fails is refused before anything of it is written, so the
                    // others go on
                    Ok(chunk
                        .iter()
                        .map(|(hash, sats)| charge(tx, sys, hash, *sats, charge_id))
                        .collect::<Vec<_>>())
                })
                .await?,
            );
        }
        Ok(out)
    }

    async fn close_file(&self, hash: &Hash) -> Result<()> {
        let hash = *hash;
        self.write(move |tx, _| {
            let (_, file) = lookups!(tx);
            let (_, mut a) = file(&hash)?.ok_or(LedgerError::NoSuchAccount)?;
            if !a.closed {
                a.closed = true;
                tx.accounts().update(&a)?;
            }
            Ok(())
        })
        .await
    }

    async fn uploaders(&self, hash: &Hash) -> Result<Vec<PubKeyBytes>> {
        let hash = FileHash(*hash);
        self.read(move |tx, _| {
            let Some(f) = tx.files().get(hash)? else { return Ok(vec![]) };
            let mut out = vec![];
            for t in tx.transfers().by_credit(f.account)? {
                if t.code == T_UPLOAD
                    && let Some(u) = tx.users().by_account(t.debit)?
                {
                    out.push(u.pubkey.0);
                }
            }
            Ok(out)
        })
        .await
    }

    async fn files_by_uploader(&self, pubkey: &PubKeyBytes) -> Result<Vec<Hash>> {
        let pubkey = Pubkey(*pubkey);
        self.read(move |tx, _| {
            let Some(u) = tx.users().get(pubkey)? else { return Ok(vec![]) };
            let mut out = vec![];
            for t in tx.transfers().by_debit(u.account)? {
                if t.code == T_UPLOAD
                    && let Some(f) = tx.files().by_account(t.credit)?
                    && !out.contains(&f.hash.0)
                {
                    out.push(f.hash.0);
                }
            }
            Ok(out)
        })
        .await
    }

    async fn files(&self) -> Result<Vec<FileInfo>> {
        self.read(|tx, _| {
            let mut out = vec![];
            for f in tx.files().all()? {
                let a = tx.accounts().get(f.account)?.ok_or(LedgerError::NoSuchAccount)?;
                out.push(file_info(&f, &a));
            }
            out.sort_by_key(|f| f.timestamp);
            Ok(out)
        })
        .await
    }

    async fn users(&self) -> Result<Vec<UserInfo>> {
        self.read(|tx, _| {
            let mut out = vec![];
            for u in tx.users().all()? {
                let a = tx.accounts().get(u.account)?.ok_or(LedgerError::NoSuchAccount)?;
                out.push(user_info(&u, &a));
            }
            out.sort_by_key(|u| u.timestamp);
            Ok(out)
        })
        .await
    }

    async fn totals(&self) -> Result<Totals> {
        self.read(|tx, sys| {
            let get = |id| tx.accounts().get(id)?.ok_or_else(|| other("system account missing"));
            let (treasury, revenue, suspense, bank) =
                (get(sys.treasury)?, get(sys.revenue)?, get(sys.suspense)?, get(sys.bank)?);
            Ok(Totals {
                received: treasury.debits,
                earned: revenue.credits,
                sold: revenue.debits,
                unclaimed: suspense.credits,
                fiat_in: bank.credits,
                fiat_out: bank.debits,
            })
        })
        .await
    }

    async fn record_unclaimed(&self, sats: u64, id: u128) -> Result<()> {
        if sats == 0 {
            return Ok(());
        }
        self.write(move |tx, sys| {
            // if more money turned up after it was recorded the first time, it stays as it was
            // and the audit sees the difference with the wallet
            let key = transfer_id(&[b"unclaimed", &id.to_be_bytes()]);
            if tx.transfers().by_key(key)?.is_none() {
                post(tx, Some(key), sys.treasury, sys.suspense, sats, T_UNCLAIMED, Memo::None)?;
            }
            Ok(())
        })
        .await
    }

    async fn record_conversion(&self, reference: &str, sats: u64, fiat_minor: u64) -> Result<bool> {
        let reference = reference.to_string();
        self.write(move |tx, sys| {
            let sats_key = transfer_id(&[b"convert", reference.as_bytes()]);
            let fiat_key = transfer_id(&[b"convert-fiat", reference.as_bytes()]);
            let memo = Memo::Conversion {
                reference: reference_id(&reference),
                sats,
                fiat_minor,
            };
            if let Some(before) = tx.transfers().by_key(sats_key)? {
                return if before.memo == memo { Ok(false) } else { Err(LedgerError::ReferenceReused) };
            }
            post(tx, Some(sats_key), sys.revenue, sys.sold, sats, T_CONVERT, memo.clone())?;
            post(tx, Some(fiat_key), sys.fx, sys.bank, fiat_minor, T_CONVERT, memo)?;
            Ok(true)
        })
        .await
    }

    async fn record_expense(&self, provider: u32, reference: &str, amount_minor: u64, date: u64) -> Result<bool> {
        let reference = reference.to_string();
        self.write(move |tx, sys| {
            let key = transfer_id(&[b"expense", &provider.to_be_bytes(), reference.as_bytes()]);
            if let Some(before) = tx.transfers().by_key(key)? {
                let same = matches!(before.memo, Memo::Expense { date: d, .. } if d == date);
                return if same && before.amount == amount_minor {
                    Ok(false)
                } else {
                    Err(LedgerError::ReferenceReused)
                };
            }
            let account = named_account(tx, &expense_name(provider), LEDGER_FIAT, CODE_EXPENSE)?;
            let memo = Memo::Expense {
                provider,
                reference: reference_id(&reference),
                date,
            };
            post(tx, Some(key), sys.bank, account, amount_minor, T_EXPENSE, memo)?;
            Ok(true)
        })
        .await
    }

    async fn expenses(&self, provider: u32) -> Result<Vec<Expense>> {
        self.read(move |tx, _| {
            let Some(account) = tx.accounts().by_name(expense_name(provider))? else {
                return Ok(vec![]);
            };
            Ok(tx
                .transfers()
                .by_credit(account.id)?
                .into_iter()
                .filter_map(|t| match t.memo {
                    Memo::Expense { reference, date, .. } => Some(Expense {
                        provider,
                        amount_minor: t.amount,
                        date,
                        timestamp: t.at,
                        reference,
                    }),
                    _ => None,
                })
                .collect())
        })
        .await
    }

    async fn conversions(&self) -> Result<Vec<Conversion>> {
        self.read(|tx, sys| {
            Ok(tx
                .transfers()
                .by_credit(sys.sold)?
                .into_iter()
                .filter_map(|t| match t.memo {
                    Memo::Conversion { reference, sats, fiat_minor } => Some(Conversion {
                        sats,
                        fiat_minor,
                        timestamp: t.at,
                        reference,
                    }),
                    _ => None,
                })
                .collect())
        })
        .await
    }
}

#[cfg(test)]
mod tests {
    #[tokio::test]
    async fn conforms() {
        super::super::conformance::run(&super::OrangeLedger::in_memory().unwrap()).await;
    }

    #[tokio::test]
    async fn survives_a_restart() {
        let path = std::env::temp_dir().join(format!("garden-ledger-{:032x}.redb", rand::random::<u128>()));
        {
            let ledger = super::OrangeLedger::open(&path).unwrap();
            super::Ledger::credit_user(&ledger, &[1; 32], 500, 1).await.unwrap();
        }
        let ledger = super::OrangeLedger::open(&path).unwrap();
        assert_eq!(super::Ledger::user(&ledger, &[1; 32]).await.unwrap().unwrap().balance, 500);
        assert_eq!(super::Ledger::totals(&ledger).await.unwrap().received, 500);
        drop(ledger);
        let _ = std::fs::remove_file(&path);
    }
}
