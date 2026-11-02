//! what the server does, independent of http

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use bytes::Bytes;
use dashmap::DashMap;
use ritualistic::blossom::{self, BlobDescriptor};
use serde::Serialize;
use sha2::Digest;

use crate::config::{Config, Denied};
use crate::ledger::{self, Credited, FileInfo, Hash, LedgerError, SharedLedger, WithdrawalFrom};
use crate::payments::{Payments, Switch};
use crate::payments::arkade::Arkade;
use crate::storage::Providers;
use crate::store::{ArkadeSettings, FileCredit, Order, OrderKind, OrderStatus, Store};

#[derive(thiserror::Error, Debug)]
pub enum AppError {
    #[error("{0}")]
    BadRequest(String),
    #[error("{0}")]
    Unauthorized(String),
    #[error("{0}")]
    PaymentRequired(String),
    #[error("{0}")]
    Forbidden(String),
    #[error("{0}")]
    NotFound(String),
    #[error("{0}")]
    TooLarge(String),
    #[error("{0}")]
    UnsupportedType(String),
    #[error("{0}")]
    Unavailable(String),
    #[error("{0}")]
    Internal(String),
}

pub type Result<T> = std::result::Result<T, AppError>;

macro_rules! internal {
    ($($t:ty),*) => {$(
        impl From<$t> for AppError {
            fn from(e: $t) -> Self {
                log::error!("{e}");
                AppError::Internal("something went wrong on our side".into())
            }
        }
    )*};
}
internal!(crate::store::StoreError, crate::payments::PaymentError);

impl From<LedgerError> for AppError {
    fn from(e: LedgerError) -> Self {
        match e {
            LedgerError::InsufficientBalance => {
                AppError::PaymentRequired("not enough sats in your account".into())
            }
            LedgerError::NoSuchAccount => AppError::PaymentRequired("you don't have an account yet".into()),
            LedgerError::ReferenceReused => AppError::BadRequest(e.to_string()),
            e => {
                log::error!("{e}");
                AppError::Internal("something went wrong on our side".into())
            }
        }
    }
}

impl From<Denied> for AppError {
    fn from(e: Denied) -> Self {
        match e {
            Denied::Pubkey => AppError::Forbidden(e.to_string()),
            Denied::TooBig(..) => AppError::TooLarge(e.to_string()),
            Denied::MimeType(_) => AppError::UnsupportedType(e.to_string()),
        }
    }
}

pub fn now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

pub fn parse_hash(s: &str) -> Option<Hash> {
    if !blossom::is_sha256_hex(s) {
        return None;
    }
    let mut out = [0u8; 32];
    lowercase_hex::decode_to_slice(s, &mut out).ok()?;
    Some(out)
}

pub fn parse_pubkey(s: &str) -> Option<[u8; 32]> {
    parse_hash(&s.to_lowercase())
}

fn hex(bytes: &[u8]) -> String {
    lowercase_hex::encode(bytes)
}

/// what came of sending sats out of the wallet
#[derive(Debug, Clone, Serialize)]
pub struct SendReport {
    pub txid: String,
    /// what the wallet holds now, if it can tell
    pub wallet_balance: Option<u64>,
}

pub struct App {
    pub config: Config,
    pub ledger: SharedLedger,
    pub providers: Providers,
    pub payments: Arc<Switch>,
    pub store: Arc<Store>,
    /// one at a time for each file when creating or expiring it
    locks: DashMap<Hash, Arc<tokio::sync::Mutex<()>>>,
    /// a short memory of which files exist, so serving doesn't hit the ledger every time
    pub(crate) served: DashMap<Hash, (Instant, Option<FileInfo>)>,
    /// bytes of the files people were sent to fetch, by day and provider, not saved yet
    redirected: DashMap<(u64, u32), u64>,
    pub(crate) sessions: crate::operator::transport::Sessions,
    /// who may use the operator interface: `operators` from the config, and whoever claims it
    /// with the setup link
    operators: std::sync::RwLock<Vec<String>>,
    /// the secret of the setup link, while there is no operator
    setup_token: std::sync::Mutex<Option<String>>,
}

const SERVED_CACHE: Duration = Duration::from_secs(30);

#[derive(Debug, Serialize)]
pub struct AccountView {
    pub pubkey: String,
    pub exists: bool,
    pub balance_sats: u64,
}

#[derive(Debug, Serialize)]
pub struct FileView {
    pub sha256: String,
    pub size: u64,
    pub balance_sats: u64,
    pub rent_sats_per_day: u64,
    pub days_left: u64,
    pub expired: bool,
}

#[derive(Debug, Serialize, Default)]
pub struct SweepReport {
    pub files: usize,
    pub charged_sats: u64,
    pub expired: usize,
}

impl App {
    pub fn new(
        config: Config,
        ledger: SharedLedger,
        providers: Providers,
        payments: Arc<Switch>,
        store: Arc<Store>,
    ) -> Arc<Self> {
        let operators = config.operators.iter().map(|k| k.to_lowercase()).collect();
        Arc::new(Self {
            operators: std::sync::RwLock::new(operators),
            setup_token: Default::default(),
            config,
            ledger,
            providers,
            payments,
            store,
            locks: DashMap::new(),
            served: DashMap::new(),
            redirected: DashMap::new(),
            sessions: Default::default(),
        })
    }

    fn lock(&self, hash: &Hash) -> Arc<tokio::sync::Mutex<()>> {
        self.locks.entry(*hash).or_default().clone()
    }

    // accounts

    /// is this pubkey allowed to be here?
    pub fn check_allowed(&self, pubkey_hex: &str) -> Result<()> {
        if self.store.is_pubkey_banned(pubkey_hex)? {
            return Err(AppError::Forbidden("this pubkey is banned".into()));
        }
        Ok(self.config.restrictions.check_account(pubkey_hex)?)
    }

    pub async fn account(&self, pubkey: &[u8; 32]) -> Result<AccountView> {
        let user = self.ledger.user(pubkey).await?;
        Ok(AccountView {
            pubkey: hex(pubkey),
            exists: user.is_some(),
            balance_sats: user.map(|u| u.balance).unwrap_or(0),
        })
    }

    // orders

    /// pay to create an account, or to put more sats in it
    pub async fn account_order(&self, pubkey: &[u8; 32], sats: u64) -> Result<Order> {
        let pubkey_hex = hex(pubkey);
        self.check_allowed(&pubkey_hex)?;

        let r = &self.config.restrictions;
        let exists = self.ledger.user(pubkey).await?.is_some();
        let min = if exists { r.min_order_sats } else { r.min_account_payment_sats };
        if sats < min {
            return Err(AppError::BadRequest(format!("the minimum is {min} sats")));
        }

        self.new_order(OrderKind::Account { pubkey: pubkey_hex }, sats).await
    }

    /// anyone can pay to add sats to any number of files
    pub async fn files_order(&self, files: BTreeMap<String, u64>) -> Result<Order> {
        let r = &self.config.restrictions;
        if files.is_empty() {
            return Err(AppError::BadRequest("no files given".into()));
        }
        if files.len() > r.max_files_per_order {
            return Err(AppError::BadRequest(format!(
                "at most {} files per order",
                r.max_files_per_order
            )));
        }

        let mut total = 0u64;
        let mut normalized = BTreeMap::new();
        for (hash, sats) in files {
            let hash = hash.to_lowercase();
            let parsed = parse_hash(&hash)
                .ok_or_else(|| AppError::BadRequest(format!("{hash} is not a sha256")))?;
            if sats == 0 {
                return Err(AppError::BadRequest(format!("{hash}: amount must be positive")));
            }
            match self.ledger.file(&parsed).await? {
                Some(f) if !f.closed => {}
                _ => return Err(AppError::NotFound(format!("{hash} is not hosted here"))),
            }
            total = total
                .checked_add(sats)
                .ok_or_else(|| AppError::BadRequest("that's too many sats".into()))?;
            normalized.insert(hash, sats);
        }
        if total < r.min_order_sats {
            return Err(AppError::BadRequest(format!("the minimum is {} sats", r.min_order_sats)));
        }

        self.new_order(OrderKind::Files { files: normalized }, total).await
    }

    async fn new_order(&self, kind: OrderKind, amount_sats: u64) -> Result<Order> {
        if !self.payments.is_ready() {
            return Err(AppError::Unavailable("this server can't take payments yet".into()));
        }
        let id = hex(&rand::random::<[u8; 16]>());
        let order = Order {
            payment: self.payments.request(&id).await?,
            id,
            created_at: now(),
            kind,
            amount_sats,
            status: OrderStatus::Pending,
            received_sats: None,
            credits: vec![],
        };
        self.store.put_order(&order)?;
        Ok(order)
    }

    /// look at the payment of an order and, if the money is there, apply it
    pub async fn check_order(&self, mut order: Order) -> Result<Order> {
        if order.status == OrderStatus::Paid {
            return Ok(order);
        }
        let received = self.payments.received(&order.payment).await?;
        if received < order.amount_sats {
            return Ok(order);
        }
        if received > order.amount_sats {
            log::warn!("order {} was overpaid: {received} instead of {}", order.id, order.amount_sats);
        }

        // everything below can be repeated safely: the ledger applies each order once.
        // what was paid and not used goes to the suspense account, so that the ledger keeps
        // matching the wallet.
        let mut unclaimed = received - order.amount_sats;
        order.received_sats = Some(received);
        match &order.kind {
            OrderKind::Account { pubkey } => {
                let pubkey = parse_pubkey(pubkey).expect("validated when the order was created");
                self.ledger
                    .credit_user(&pubkey, order.amount_sats, order.payment_id())
                    .await?;
            }
            OrderKind::Files { files } => {
                let mut credits = vec![];
                for (hash, &sats) in files {
                    let parsed = parse_hash(hash).expect("validated when the order was created");
                    let result = match self.ledger.credit_file(&parsed, sats, order.payment_id()).await? {
                        Credited::Yes | Credited::AlreadyDone => "credited",
                        Credited::NoSuchFile => {
                            log::warn!("order {}: {hash} is gone, {sats} sats are unclaimed", order.id);
                            unclaimed += sats;
                            "gone"
                        }
                    };
                    credits.push(FileCredit {
                        hash: hash.clone(),
                        sats,
                        result: result.into(),
                    });
                }
                order.credits = credits;
            }
        }
        self.ledger.record_unclaimed(unclaimed, order.payment_id()).await?;

        order.status = OrderStatus::Paid;
        self.store.put_order(&order)?;
        Ok(order)
    }

    // operators

    pub fn has_operators(&self) -> bool {
        !self.operators.read().unwrap().is_empty()
    }

    pub fn is_operator(&self, key: &str) -> bool {
        self.operators.read().unwrap().iter().any(|k| *k == key.to_lowercase())
    }

    /// while nobody runs the server, a link that makes whoever opens it first the operator.
    /// it has a secret that changes every time we start and works once.
    pub fn setup_link(&self) -> Option<String> {
        if self.has_operators() {
            return None;
        }
        let token = self
            .setup_token
            .lock()
            .unwrap()
            .get_or_insert_with(|| hex(&rand::random::<[u8; 32]>()))
            .clone();
        // hyperb picks http for ip addresses and localhost, https for everything else
        let url = url::Url::parse(&self.config.public_url).ok()?;
        let host = url.host_str()?;
        let plain = url.scheme() == "http"
            && !(host == "localhost" || host.ends_with(".localhost") || host.parse::<std::net::IpAddr>().is_ok()
                || host.starts_with('['));
        let port = url.port().map(|p| format!(":{p}")).unwrap_or_default();
        let scheme = if plain { "hyperb+http" } else { "hyperb" };
        Some(format!("{scheme}://{host}{port}/operator#setup={token}"))
    }

    /// the key that opened the setup link becomes the operator, and is added to the config
    /// file. false if the token is wrong or someone did it already. what couldn't be saved is
    /// said in the error.
    pub fn claim_operator(&self, key: &str, token: &str) -> std::result::Result<bool, String> {
        let mut setup = self.setup_token.lock().unwrap();
        let same = setup.as_deref().is_some_and(|t| {
            // compared as hashes, so the time it takes says nothing about the secret
            let h = |s: &str| sha2::Sha256::digest(s.as_bytes());
            h(t) == h(token)
        });
        if !same || self.has_operators() {
            return Ok(false);
        }
        *setup = None;
        let key = key.to_lowercase();
        self.operators.write().unwrap().push(key.clone());
        log::info!("{key} opened the setup link and is now the operator");
        match &self.config.path {
            Some(path) => crate::config::add_operator(path, &key)
                .map(|_| true)
                .map_err(|e| format!("couldn't add the key to {}: {e}", path.display())),
            None => Ok(true),
        }
    }

    // payments

    /// the arkade wallet set up in the operator interface, if payments are not running yet.
    /// called at startup, and again later in case the server couldn't be reached.
    pub async fn connect_payments(&self) {
        if self.config.payments.is_some() || self.payments.is_ready() {
            return;
        }
        match self.store.arkade() {
            Ok(Some(s)) => match Arkade::connect(&s.server_url, &s.seed, self.store.clone()).await {
                Ok(a) => {
                    log::info!("payments on arkade through {}", s.server_url);
                    self.payments.set(Arc::new(a));
                }
                Err(e) => log::error!("couldn't connect to {}, will try again: {e}", s.server_url),
            },
            Ok(None) => {}
            Err(e) => log::error!("{e}"),
        }
    }

    /// orders paid to the current wallet that may still be paid
    pub fn waiting_orders(&self) -> Result<Vec<Order>> {
        let since = now().saturating_sub(self.config.restrictions.order_retention_hours * 3600);
        Ok(self.store.waiting_orders(since)?)
    }

    /// use this arkade server and wallet from now on. it's connected to before anything is
    /// saved, so nothing changes if it doesn't work. returns the network it is on.
    pub async fn set_up_arkade(&self, settings: ArkadeSettings) -> Result<String> {
        if self.config.payments.is_some() {
            return Err(AppError::BadRequest(
                "payments are set in the config file; remove [payments] there to set them up here".into(),
            ));
        }
        if !crate::payments::arkade::valid_seed(&settings.seed) {
            return Err(AppError::BadRequest("a seed is 32 bytes in hex".into()));
        }
        let current = self.store.arkade()?;
        if current.as_ref() == Some(&settings) && self.payments.is_ready() {
            return Err(AppError::BadRequest("that's what is in use already".into()));
        }
        // they are watched at the addresses of the wallet in use
        let waiting = self.waiting_orders()?.len();
        if current.is_some() && waiting > 0 {
            return Err(AppError::BadRequest(format!(
                "{waiting} orders are waiting for their payment at the current wallet; change it once they are paid or have expired"
            )));
        }

        let connect = Arkade::connect(&settings.server_url, &settings.seed, self.store.clone());
        let arkade = tokio::time::timeout(Duration::from_secs(30), connect)
            .await
            .map_err(|_| AppError::Unavailable(format!("{} didn't answer", settings.server_url)))??;
        let network = arkade.network().await?.unwrap_or_default();

        self.store.set_arkade(&settings)?;
        self.payments.set(Arc::new(arkade));
        log::info!("payments on arkade ({network}) through {}, set up by the operator", settings.server_url);
        Ok(network)
    }

    /// look for payments of all the orders that are waiting
    pub async fn check_waiting_orders(&self) {
        let orders = match self.waiting_orders() {
            Ok(o) => o,
            Err(e) => return log::error!("{e}"),
        };
        for order in orders {
            let id = order.id.clone();
            if let Err(e) = self.check_order(order).await {
                log::warn!("checking order {id}: {e}");
            }
        }
    }

    // files

    pub async fn file_view(&self, hash: &Hash) -> Result<FileView> {
        let file = self
            .ledger
            .file(hash)
            .await?
            .ok_or_else(|| AppError::NotFound("no such file".into()))?;
        let rent = self.config.pricing.rent_per_day(file.size);
        Ok(FileView {
            sha256: hex(hash),
            size: file.size,
            balance_sats: file.balance,
            rent_sats_per_day: rent,
            days_left: if file.closed { 0 } else { file.balance / rent },
            expired: file.closed,
        })
    }

    /// where a file is, if it's alive
    pub async fn locate(&self, hash: &Hash) -> Result<Option<String>> {
        let file = match self.served.get(hash) {
            Some(e) if e.0.elapsed() < SERVED_CACHE => e.1.clone(),
            _ => {
                let file = self.ledger.file(hash).await?.filter(|f| !f.closed);
                self.served.insert(*hash, (Instant::now(), file.clone()));
                file
            }
        };
        let Some(file) = file else { return Ok(None) };

        let storage = self
            .providers
            .get(file.provider)
            .ok_or_else(|| AppError::Unavailable("the provider of this file is not configured".into()))?;
        let url = storage
            .url(&hex(hash))
            .await
            .map_err(|e| AppError::Unavailable(e.to_string()))?;
        *self.redirected.entry((now() / 86400, file.provider)).or_default() += file.size;
        Ok(Some(url))
    }

    /// save the redirect counters, so that they survive and the audit sees them
    pub fn save_redirects(&self) {
        let keys: Vec<_> = self.redirected.iter().map(|e| *e.key()).collect();
        for key in keys {
            if let Some((_, bytes)) = self.redirected.remove(&key)
                && let Err(e) = self.store.add_redirected(key.0, key.1, bytes)
            {
                log::error!("{e}");
            }
        }
    }

    pub fn descriptor(&self, hash_hex: &str, size: u64, mime: &str, uploaded: u64) -> BlobDescriptor {
        BlobDescriptor {
            url: format!("{}/{hash_hex}{}", self.config.public_url, extension_for(mime)),
            sha256: hash_hex.to_string(),
            size,
            mime: mime.to_string(),
            uploaded,
        }
    }

    /// store a file, paying for it with the uploader's sats
    pub async fn upload(
        &self,
        uploader: &[u8; 32],
        content_type: &str,
        body: Bytes,
        // the authorization may name the blobs it is good for, this tells if ours is one of them
        covered: impl Fn(&str) -> bool,
    ) -> Result<BlobDescriptor> {
        let uploader_hex = hex(uploader);
        self.check_allowed(&uploader_hex)?;

        let mime = if content_type.trim().is_empty() { "application/octet-stream" } else { content_type };
        let size = body.len() as u64;
        if size == 0 {
            return Err(AppError::BadRequest("empty files are not accepted".into()));
        }
        self.config.restrictions.check_upload(&uploader_hex, size, mime)?;

        let hash_hex = blossom::sha256_hex(&body);
        let hash = parse_hash(&hash_hex).unwrap();
        if !covered(&hash_hex) {
            return Err(AppError::Unauthorized("the authorization is for a different file".into()));
        }
        if self.store.is_hash_banned(&hash_hex)? {
            return Err(AppError::Forbidden("this file is banned".into()));
        }

        let lock = self.lock(&hash);
        let _guard = lock.lock().await;

        let existing = self.ledger.file(&hash).await?;
        if let Some(f) = existing.as_ref().filter(|f| !f.closed) {
            // already here and paid for by someone, nothing to do (and nothing to charge)
            return Ok(self.descriptor(&hash_hex, f.size, mime, now()));
        }

        let funding = self.config.pricing.initial_funding(size);
        let user = self
            .ledger
            .user(uploader)
            .await?
            .ok_or_else(|| AppError::PaymentRequired("you need to create an account (and pay) first".into()))?;
        if user.balance < funding {
            return Err(AppError::PaymentRequired(format!(
                "this file costs {funding} sats to host for {} days and you have {}",
                self.config.pricing.min_days, user.balance
            )));
        }

        // a file that expired stays with the provider it was in
        let provider = match &existing {
            Some(f) => f.provider,
            None => self
                .providers
                .pick()
                .ok_or_else(|| AppError::Unavailable("no storage available".into()))?,
        };
        let storage = self
            .providers
            .get(provider)
            .ok_or_else(|| AppError::Unavailable("the storage of this file is not available".into()))?;

        storage
            .put(&hash_hex, mime, body)
            .await
            .map_err(|e| AppError::Unavailable(e.to_string()))?;

        if let Err(e) = self.ledger.create_file(&hash, size, provider, uploader, funding).await {
            if let Err(e) = storage.delete(&hash_hex).await {
                log::warn!("couldn't clean up {hash_hex} after a failed upload: {e}");
            }
            return Err(e.into());
        }
        self.store.remove_pending_delete(&hash_hex)?;
        self.served.remove(&hash);

        log::info!("{hash_hex} ({size} bytes) stored by {uploader_hex} with {funding} sats");
        Ok(self.descriptor(&hash_hex, size, mime, now()))
    }

    // rent

    /// take one day of rent from every file and get rid of those that ran out of sats
    pub async fn sweep(&self, day: u64) -> Result<SweepReport> {
        let mut files = self.ledger.files().await?;
        files.retain(|f| !f.closed);

        let mut report = SweepReport {
            files: files.len(),
            ..Default::default()
        };
        // provider -> (files, bytes, sats of rent)
        let mut by_provider: std::collections::BTreeMap<u32, (u64, u64, u64)> = Default::default();
        let charges: Vec<_> = files
            .iter()
            .map(|f| (f.hash, self.config.pricing.rent_per_day(f.size)))
            .collect();
        let charged = self.ledger.charge_files(&charges, day as u128).await?;

        let mut empty = vec![];
        for (f, result) in files.into_iter().zip(charged) {
            let charged = match result {
                Ok(c) => c,
                Err(e) => {
                    log::error!("charging {}: {e}", hex(&f.hash));
                    continue;
                }
            };
            report.charged_sats += charged.taken;
            let p = by_provider.entry(f.provider).or_default();
            *p = (p.0 + 1, p.1 + f.size, p.2 + charged.taken);
            if charged.left == 0 {
                empty.push(f);
            }
        }

        for f in empty {
            let lock = self.lock(&f.hash);
            let _guard = lock.lock().await;

            // it may have been paid for since it was charged
            match self.ledger.file(&f.hash).await {
                Ok(Some(after)) if !after.closed && after.balance == 0 => {
                    log::info!("{} ran out of sats", hex(&f.hash));
                    if let Err(e) = self.expire(&f).await {
                        log::error!("expiring {}: {e}", hex(&f.hash));
                    } else {
                        report.expired += 1;
                    }
                }
                Ok(_) => {}
                Err(e) => log::error!("{e}"),
            }
        }
        for (provider, (files, bytes, rent)) in by_provider {
            self.store.record_sweep(day, provider, files, bytes, rent)?;
        }
        Ok(report)
    }

    /// no more serving, no more credits, and the bytes go away. the caller holds the lock.
    async fn expire(&self, file: &FileInfo) -> Result<()> {
        self.ledger.close_file(&file.hash).await?;
        self.served.remove(&file.hash);

        let hash_hex = hex(&file.hash);
        let deleted = match self.providers.get(file.provider) {
            Some(storage) => storage.delete(&hash_hex).await.map_err(|e| e.to_string()),
            None => Err(format!("provider {} is not configured", file.provider)),
        };
        if let Err(e) = deleted {
            log::warn!("couldn't delete {hash_hex} yet, will retry: {e}");
            self.store.add_pending_delete(&hash_hex, file.provider)?;
        }
        Ok(())
    }

    /// try again to delete what we couldn't before
    pub async fn retry_deletes(&self) {
        let pending = match self.store.pending_deletes() {
            Ok(p) => p,
            Err(e) => return log::error!("{e}"),
        };
        for (hash_hex, provider) in pending {
            let Some(hash) = parse_hash(&hash_hex) else { continue };
            let lock = self.lock(&hash);
            let _guard = lock.lock().await;

            // it may have been uploaded again in the meantime
            let alive = matches!(self.ledger.file(&hash).await, Ok(Some(f)) if !f.closed);
            let done = alive
                || match self.providers.get(provider) {
                    Some(storage) => storage.delete(&hash_hex).await.is_ok(),
                    None => false,
                };
            if done && let Err(e) = self.store.remove_pending_delete(&hash_hex) {
                log::error!("{e}");
            }
        }
    }

    // the books

    fn check_provider(&self, provider: u32) -> Result<()> {
        if self.config.providers.iter().any(|p| p.id == provider) {
            Ok(())
        } else {
            Err(AppError::BadRequest(format!("there is no provider {provider}")))
        }
    }

    /// the operator paid a provider's bill. false if it was already recorded.
    pub async fn record_expense(
        &self,
        provider: u32,
        reference: &str,
        amount_minor: u64,
        date: u64,
        note: &str,
    ) -> Result<bool> {
        self.check_provider(provider)?;
        let reference = reference.trim();
        if reference.is_empty() {
            return Err(AppError::BadRequest("a reference is needed, like the number of the invoice".into()));
        }
        if amount_minor == 0 {
            return Err(AppError::BadRequest("the amount can't be zero".into()));
        }
        let recorded = self.ledger.record_expense(provider, reference, amount_minor, date).await?;
        if recorded {
            self.store.set_note(&ledger::reference_hex(reference), &label(reference, note))?;
        }
        Ok(recorded)
    }

    /// the operator sold sats for fiat. false if it was already recorded.
    pub async fn record_conversion(
        &self,
        reference: &str,
        sats: u64,
        fiat_minor: u64,
        note: &str,
    ) -> Result<bool> {
        let reference = reference.trim();
        if reference.is_empty() {
            return Err(AppError::BadRequest("a reference is needed, like the id of the trade".into()));
        }
        if sats == 0 || fiat_minor == 0 {
            return Err(AppError::BadRequest("the amounts can't be zero".into()));
        }
        let recorded = self.ledger.record_conversion(reference, sats, fiat_minor).await?;
        if recorded {
            self.store.set_note(&ledger::reference_hex(reference), &label(reference, note))?;
        }
        Ok(recorded)
    }

    /// send sats out of the wallet. the money leaves first and is recorded in the books right
    /// after, so a send can never be recorded without having happened. a reference that was
    /// used before is refused, which is what stops a retried send from going out twice.
    pub async fn send_sats(
        &self,
        from: WithdrawalFrom,
        address: &str,
        sats: u64,
        reference: &str,
        note: &str,
    ) -> Result<SendReport> {
        let reference = reference.trim();
        if reference.is_empty() {
            return Err(AppError::BadRequest("a reference is needed for every send".into()));
        }
        if sats == 0 {
            return Err(AppError::BadRequest("the amount can't be zero".into()));
        }
        if !self.payments.is_ready() {
            return Err(AppError::Unavailable("this server can't send yet: payments are not set up".into()));
        }
        if self.ledger.withdrawal(reference).await?.is_some() {
            return Err(AppError::BadRequest(
                "that exact send was recorded before; change the note to send the same amount again".into(),
            ));
        }

        let totals = self.ledger.totals().await?;
        let available = match from {
            WithdrawalFrom::Revenue => totals.earned.saturating_sub(totals.sold),
            WithdrawalFrom::Suspense => totals.unclaimed,
            WithdrawalFrom::User(pubkey) => self.ledger.user(&pubkey).await?.map(|u| u.balance).unwrap_or(0),
        };
        if sats > available {
            return Err(AppError::BadRequest(format!("there are only {available} sats available there")));
        }

        let txid = self.payments.send(address, sats).await?;
        let recorded = self.ledger.record_withdrawal(from, sats, reference, address).await?;
        if !recorded {
            log::error!("sent {sats} sats in {txid} but the withdrawal {reference} was already in the books");
        }
        if !note.trim().is_empty() {
            self.store.set_note(&ledger::reference_hex(reference), note.trim())?;
        }
        let wallet_balance = self.payments.wallet_balance().await.ok().flatten();
        log::info!("sent {sats} sats to {address} in {txid} ({reference})");
        Ok(SendReport { txid, wallet_balance })
    }

    /// give sats that no order claimed to an account. no money moves in the wallet.
    pub async fn grant_unclaimed(
        &self,
        pubkey: &[u8; 32],
        sats: u64,
        reference: &str,
        note: &str,
    ) -> Result<bool> {
        let reference = reference.trim();
        if reference.is_empty() {
            return Err(AppError::BadRequest("a reference is needed for every grant".into()));
        }
        if sats == 0 {
            return Err(AppError::BadRequest("the amount can't be zero".into()));
        }
        let unclaimed = self.ledger.totals().await?.unclaimed;
        if sats > unclaimed {
            return Err(AppError::BadRequest(format!("there are only {unclaimed} unclaimed sats")));
        }
        let recorded = self.ledger.record_grant(pubkey, sats, reference).await?;
        if recorded && !note.trim().is_empty() {
            self.store.set_note(&ledger::reference_hex(reference), note.trim())?;
        }
        Ok(recorded)
    }

    pub fn set_rate(&self, fiat_minor_per_btc: u64) -> Result<()> {
        if fiat_minor_per_btc == 0 {
            return Err(AppError::BadRequest("the rate can't be zero".into()));
        }
        Ok(self.store.set_rate(crate::store::Rate {
            fiat_minor_per_btc,
            set_at: now(),
        })?)
    }

    pub fn set_prices(&self, provider: u32, prices: crate::store::ProviderPrices) -> Result<()> {
        self.check_provider(provider)?;
        Ok(self.store.set_prices(provider, prices)?)
    }

    /// with mock payments only: pretend an order was paid
    pub async fn confirm_mock(&self, order_id: &str) -> Result<Order> {
        let order = (order_id.len() == 32)
            .then(|| self.store.order(order_id))
            .transpose()?
            .flatten()
            .ok_or_else(|| AppError::NotFound("no such order".into()))?;
        if !self.payments.simulate(&order.payment, order.amount_sats) {
            return Err(AppError::BadRequest("payments are real, they can't be confirmed by hand".into()));
        }
        self.check_order(order).await
    }

    // moderation

    /// who put this file here?
    pub async fn uploaders(&self, hash: &Hash) -> Result<Vec<String>> {
        Ok(self.ledger.uploaders(hash).await?.iter().map(|p| hex(p)).collect())
    }

    /// remove a file now and never accept it again. whatever sats it had are kept.
    pub async fn purge_file(&self, hash: &Hash) -> Result<()> {
        let hash_hex = hex(hash);
        self.store.ban_hash(&hash_hex, now())?;

        let lock = self.lock(hash);
        let _guard = lock.lock().await;

        let Some(file) = self.ledger.file(hash).await? else {
            return Ok(());
        };
        if !file.closed {
            self.ledger
                .charge_file(hash, file.balance, rand::random::<u128>() | 1)
                .await?;
        }
        self.expire(&file).await
    }

    /// ban a pubkey and optionally remove everything it has uploaded
    pub async fn ban_pubkey(&self, pubkey: &[u8; 32], purge: bool) -> Result<Vec<String>> {
        self.store.ban_pubkey(&hex(pubkey), now())?;
        let mut purged = vec![];
        if purge {
            for hash in self.ledger.files_by_uploader(pubkey).await? {
                self.purge_file(&hash).await?;
                purged.push(hex(&hash));
            }
        }
        Ok(purged)
    }
}

/// what we show next to an entry of the books
fn label(reference: &str, note: &str) -> String {
    match note.trim() {
        "" => reference.to_string(),
        note => format!("{reference} — {note}"),
    }
}

fn extension_for(mime: &str) -> &'static str {
    match mime.split(';').next().unwrap_or("").trim() {
        "image/png" => ".png",
        "image/jpeg" => ".jpg",
        "image/gif" => ".gif",
        "image/webp" => ".webp",
        "image/svg+xml" => ".svg",
        "image/avif" => ".avif",
        "video/mp4" => ".mp4",
        "video/webm" => ".webm",
        "audio/mpeg" => ".mp3",
        "audio/ogg" => ".ogg",
        "application/pdf" => ".pdf",
        "text/plain" => ".txt",
        _ => "",
    }
}
