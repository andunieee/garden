//! admin settings, loaded from a toml file (see `garden.example.toml`)

use std::collections::BTreeSet;

use serde::Deserialize;

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    /// address to listen on
    #[serde(default = "default_listen")]
    pub listen: String,

    /// public base url of this server, used to build blob urls, e.g. https://cdn.example.com
    pub public_url: String,

    /// where the local state (pending orders, bans) is kept
    #[serde(default = "default_data_dir")]
    pub data_dir: String,

    /// bearer token for the /admin endpoints, if empty these are disabled
    #[serde(default)]
    pub admin_token: String,

    #[serde(default)]
    pub restrictions: Restrictions,

    #[serde(default)]
    pub pricing: Pricing,

    pub ledger: LedgerConfig,

    pub payments: PaymentsConfig,

    #[serde(default, rename = "provider")]
    pub providers: Vec<ProviderConfig>,
}

fn default_listen() -> String {
    "0.0.0.0:3000".into()
}

fn default_data_dir() -> String {
    "./data".into()
}

/// who can do what. this is the place to add new rules, they are all enforced by
/// [Restrictions::check_account] and [Restrictions::check_upload]
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct Restrictions {
    /// biggest file accepted, in bytes
    pub max_file_size: u64,

    /// if not empty only these mime types (e.g. "image/png", or "image/*") can be uploaded
    pub allowed_mime_types: Vec<String>,

    /// if not empty only these pubkeys (hex) can create accounts and upload
    pub allowed_pubkeys: BTreeSet<String>,

    /// these pubkeys (hex) can never create accounts nor upload
    pub denied_pubkeys: BTreeSet<String>,

    /// least amount that must be paid for an account to be created
    pub min_account_payment_sats: u64,

    /// least amount of any other order (account credit or file top-up)
    pub min_order_sats: u64,

    /// most files a single top-up order can pay for
    pub max_files_per_order: usize,

    /// how many hours we keep waiting for the payment of an order
    pub order_retention_hours: u64,
}

impl Default for Restrictions {
    fn default() -> Self {
        Self {
            max_file_size: 100 * 1024 * 1024,
            allowed_mime_types: vec![],
            allowed_pubkeys: BTreeSet::new(),
            denied_pubkeys: BTreeSet::new(),
            min_account_payment_sats: 1000,
            min_order_sats: 100,
            max_files_per_order: 100,
            order_retention_hours: 24 * 7,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum Denied {
    #[error("this pubkey is not allowed")]
    Pubkey,
    #[error("files of {0} bytes are not accepted, the limit is {1}")]
    TooBig(u64, u64),
    #[error("files of type {0} are not accepted")]
    MimeType(String),
}

impl Restrictions {
    /// can this pubkey have an account (and upload)?
    pub fn check_account(&self, pubkey_hex: &str) -> Result<(), Denied> {
        if self.denied_pubkeys.contains(pubkey_hex) {
            return Err(Denied::Pubkey);
        }
        if !self.allowed_pubkeys.is_empty() && !self.allowed_pubkeys.contains(pubkey_hex) {
            return Err(Denied::Pubkey);
        }
        Ok(())
    }

    /// can this pubkey upload this?
    pub fn check_upload(&self, pubkey_hex: &str, size: u64, mime: &str) -> Result<(), Denied> {
        self.check_account(pubkey_hex)?;
        self.check_file(size, mime)
    }

    /// the rules that depend only on the file
    pub fn check_file(&self, size: u64, mime: &str) -> Result<(), Denied> {
        if size > self.max_file_size {
            return Err(Denied::TooBig(size, self.max_file_size));
        }
        if !self.allowed_mime_types.is_empty() {
            let mime = mime.split(';').next().unwrap_or("").trim().to_lowercase();
            let ok = self.allowed_mime_types.iter().any(|allowed| {
                let allowed = allowed.to_lowercase();
                match allowed.strip_suffix("/*") {
                    Some(prefix) => mime.split('/').next() == Some(prefix),
                    None => allowed == mime,
                }
            });
            if !ok {
                return Err(Denied::MimeType(mime));
            }
        }
        Ok(())
    }
}

/// how much files cost to keep. the daily rent of a file is
/// `base_sats_per_day + ceil(size_in_mib * sats_per_mib_per_day)`
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct Pricing {
    pub base_sats_per_day: u64,
    pub sats_per_mib_per_day: f64,

    /// a new file is funded with enough sats to last at least this many days. can't be less
    /// than 30.
    pub min_days: u64,
}

impl Default for Pricing {
    fn default() -> Self {
        Self {
            base_sats_per_day: 1,
            sats_per_mib_per_day: 0.1,
            min_days: 30,
        }
    }
}

impl Pricing {
    pub fn rent_per_day(&self, size: u64) -> u64 {
        let mib = size as f64 / (1024.0 * 1024.0);
        (self.base_sats_per_day + (mib * self.sats_per_mib_per_day).ceil() as u64).max(1)
    }

    /// what has to be put in a new file account. the first day is charged right away (well,
    /// at the next daily sweep) so one extra day is added to make sure the file really lasts
    /// the promised time.
    pub fn initial_funding(&self, size: u64) -> u64 {
        self.rent_per_day(size) * (self.min_days.max(30) + 1)
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(tag = "kind", rename_all = "lowercase", deny_unknown_fields)]
pub enum LedgerConfig {
    /// a tigerbeetle cluster
    Tigerbeetle {
        #[serde(default)]
        cluster_id: u64,
        /// comma separated replica addresses
        addresses: String,
    },
    /// volatile, for development only
    Memory,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(tag = "kind", rename_all = "lowercase", deny_unknown_fields)]
pub enum PaymentsConfig {
    Arkade {
        /// url of the arkd server, e.g. https://arkade.computer
        server_url: String,
        /// hex of the 32 byte seed of the wallet that receives the payments (keep it safe!)
        /// can also be given in the GARDEN_ARKADE_SEED environment variable
        #[serde(default)]
        seed: String,
        /// how often we look for payments, in seconds
        #[serde(default = "default_poll_secs")]
        poll_secs: u64,
    },
    /// payments are confirmed manually through the admin api, for development only
    Mock,
}

fn default_poll_secs() -> u64 {
    5
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ProviderKind {
    S3,
    /// backblaze b2, through its s3 compatible api
    B2,
    /// cloudflare r2, through its s3 compatible api
    R2,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProviderConfig {
    /// a number that identifies this provider forever (it gets written to the ledger next to
    /// each file), never reuse or change it
    pub id: u32,
    pub kind: ProviderKind,
    #[serde(default = "yes")]
    pub enabled: bool,
    /// chance of being picked for a new upload relative to the other providers
    #[serde(default = "one")]
    pub weight: u32,
    pub bucket: String,
    #[serde(default)]
    pub region: String,
    /// required for b2 (https://s3.<region>.backblazeb2.com) and r2
    /// (https://<account id>.r2.cloudflarestorage.com)
    #[serde(default)]
    pub endpoint: String,
    pub access_key_id: String,
    pub secret_access_key: String,
    /// if the bucket is public, the base url the objects are available at. otherwise we
    /// redirect to short-lived presigned urls
    #[serde(default)]
    pub public_url: String,
}

fn yes() -> bool {
    true
}

fn one() -> u32 {
    1
}

impl Config {
    pub fn load(path: &str) -> Result<Config, String> {
        let text = std::fs::read_to_string(path).map_err(|e| format!("reading {path}: {e}"))?;
        let mut config: Config = toml::from_str(&text).map_err(|e| format!("{path}: {e}"))?;
        config.public_url = config.public_url.trim_end_matches('/').to_string();

        if let PaymentsConfig::Arkade { seed, .. } = &mut config.payments {
            if let Ok(s) = std::env::var("GARDEN_ARKADE_SEED") {
                *seed = s;
            }
        }
        config.validate()?;
        Ok(config)
    }

    pub fn validate(&self) -> Result<(), String> {
        if self.pricing.min_days < 30 {
            return Err("pricing.min_days can't be less than 30".into());
        }
        let mut ids = BTreeSet::new();
        for p in &self.providers {
            if !ids.insert(p.id) {
                return Err(format!("duplicate provider id {}", p.id));
            }
            if p.id == 0 {
                return Err("provider ids start at 1".into());
            }
            if matches!(p.kind, ProviderKind::B2 | ProviderKind::R2) && p.endpoint.is_empty() {
                return Err(format!("provider {} needs an endpoint", p.id));
            }
        }
        if !self.providers.iter().any(|p| p.enabled && p.weight > 0) {
            return Err("at least one enabled provider is needed".into());
        }
        if let PaymentsConfig::Arkade { seed, .. } = &self.payments {
            if seed.len() != 64 || lowercase_hex::decode(seed).is_err() {
                return Err("payments.seed must be 32 bytes in hex".into());
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mime_rules() {
        let r = Restrictions {
            allowed_mime_types: vec!["image/*".into(), "application/pdf".into()],
            ..Default::default()
        };
        assert!(r.check_upload("aa", 10, "image/png").is_ok());
        assert!(r.check_upload("aa", 10, "application/pdf; charset=x").is_ok());
        assert_eq!(
            r.check_upload("aa", 10, "video/mp4"),
            Err(Denied::MimeType("video/mp4".into()))
        );
        assert!(matches!(
            r.check_upload("aa", u64::MAX, "image/png"),
            Err(Denied::TooBig(..))
        ));
    }

    #[test]
    fn pubkey_rules() {
        let mut r = Restrictions::default();
        r.denied_pubkeys.insert("bad".into());
        assert!(r.check_account("good").is_ok());
        assert!(r.check_account("bad").is_err());

        r.allowed_pubkeys.insert("good".into());
        assert!(r.check_account("good").is_ok());
        assert!(r.check_account("other").is_err());
    }

    #[test]
    fn pricing() {
        let p = Pricing::default();
        assert_eq!(p.rent_per_day(0), 1);
        assert_eq!(p.rent_per_day(1024), 2);
        assert_eq!(p.rent_per_day(10 * 1024 * 1024), 2);
        assert_eq!(p.rent_per_day(100 * 1024 * 1024), 11);
        // always at least a month, plus the day that is charged straight away
        assert_eq!(p.initial_funding(1024), 2 * 31);
    }

    #[test]
    fn example_parses() {
        let text = include_str!("../garden.example.toml");
        let config: Config = toml::from_str(text).unwrap();
        config.validate().unwrap();
    }
}
