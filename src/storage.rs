//! file hosting providers. blobs are stored under their sha256 in hex.
//!
//! b2, s3 and r2 all speak the s3 api, so one implementation covers the three.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use bytes::Bytes;
use rusty_s3::{Bucket, Credentials, S3Action, UrlStyle};

use crate::config::{ProviderConfig, ProviderKind};

#[derive(thiserror::Error, Debug)]
pub enum StorageError {
    #[error("provider failure: {0}")]
    Provider(String),
}

pub type Result<T> = std::result::Result<T, StorageError>;

fn err(e: impl std::fmt::Display) -> StorageError {
    StorageError::Provider(e.to_string())
}

#[async_trait::async_trait]
pub trait Storage: Send + Sync {
    async fn put(&self, key: &str, content_type: &str, body: Bytes) -> Result<()>;

    /// deleting something that isn't there is not an error
    async fn delete(&self, key: &str) -> Result<()>;

    /// where to send people that want the blob
    async fn url(&self, key: &str) -> Result<String>;
}

const SIGNATURE_VALIDITY: Duration = Duration::from_secs(3600);

pub struct S3Storage {
    bucket: Bucket,
    credentials: Credentials,
    public_url: Option<String>,
    http: reqwest::Client,
}

impl S3Storage {
    pub fn new(config: &ProviderConfig) -> Result<Self> {
        let (endpoint, style, default_region) = match config.kind {
            ProviderKind::S3 if config.endpoint.is_empty() => {
                let region = if config.region.is_empty() { "us-east-1" } else { &config.region };
                (format!("https://s3.{region}.amazonaws.com"), UrlStyle::VirtualHost, "us-east-1")
            }
            // b2 and r2 want path style, and so does any custom s3 endpoint
            _ => (config.endpoint.clone(), UrlStyle::Path, "auto"),
        };
        let region = if config.region.is_empty() { default_region } else { &config.region };

        Ok(Self {
            bucket: Bucket::new(
                endpoint.parse().map_err(err)?,
                style,
                config.bucket.clone(),
                region.to_string(),
            )
            .map_err(err)?,
            credentials: Credentials::new(&config.access_key_id, &config.secret_access_key),
            public_url: (!config.public_url.is_empty())
                .then(|| config.public_url.trim_end_matches('/').to_string()),
            http: reqwest::Client::builder()
                .timeout(Duration::from_secs(600))
                .build()
                .map_err(err)?,
        })
    }
}

#[async_trait::async_trait]
impl Storage for S3Storage {
    async fn put(&self, key: &str, content_type: &str, body: Bytes) -> Result<()> {
        let mut action = self.bucket.put_object(Some(&self.credentials), key);
        action.headers_mut().insert("content-type", content_type);
        let url = action.sign(SIGNATURE_VALIDITY);

        let res = self
            .http
            .put(url)
            .header("content-type", content_type)
            .body(body)
            .send()
            .await
            .map_err(err)?;
        if !res.status().is_success() {
            let status = res.status();
            return Err(err(format!("put {key}: {status} {}", res.text().await.unwrap_or_default())));
        }
        Ok(())
    }

    async fn delete(&self, key: &str) -> Result<()> {
        let url = self
            .bucket
            .delete_object(Some(&self.credentials), key)
            .sign(SIGNATURE_VALIDITY);
        let res = self.http.delete(url).send().await.map_err(err)?;
        if !res.status().is_success() && res.status() != reqwest::StatusCode::NOT_FOUND {
            return Err(err(format!("delete {key}: {}", res.status())));
        }
        Ok(())
    }

    async fn url(&self, key: &str) -> Result<String> {
        if let Some(base) = &self.public_url {
            return Ok(format!("{base}/{key}"));
        }
        Ok(self
            .bucket
            .get_object(Some(&self.credentials), key)
            .sign(SIGNATURE_VALIDITY)
            .to_string())
    }
}

/// keeps blobs in memory, for tests
#[cfg(test)]
#[derive(Default)]
pub struct MemoryStorage {
    pub blobs: std::sync::Mutex<HashMap<String, (String, Bytes)>>,
    pub fail_deletes: std::sync::atomic::AtomicBool,
}

#[cfg(test)]
#[async_trait::async_trait]
impl Storage for MemoryStorage {
    async fn put(&self, key: &str, content_type: &str, body: Bytes) -> Result<()> {
        self.blobs
            .lock()
            .unwrap()
            .insert(key.to_string(), (content_type.to_string(), body));
        Ok(())
    }

    async fn delete(&self, key: &str) -> Result<()> {
        if self.fail_deletes.load(std::sync::atomic::Ordering::SeqCst) {
            return Err(err("provider is down"));
        }
        self.blobs.lock().unwrap().remove(key);
        Ok(())
    }

    async fn url(&self, key: &str) -> Result<String> {
        Ok(format!("https://storage.test/{key}"))
    }
}

/// the configured providers, by id
#[derive(Clone)]
pub struct Providers {
    by_id: Arc<HashMap<u32, Arc<dyn Storage>>>,
    /// enabled providers and their weights
    candidates: Arc<Vec<(u32, u32)>>,
}

impl Providers {
    pub fn from_config(configs: &[ProviderConfig]) -> Result<Self> {
        let mut by_id: HashMap<u32, Arc<dyn Storage>> = HashMap::new();
        let mut candidates = vec![];
        for c in configs {
            by_id.insert(c.id, Arc::new(S3Storage::new(c)?));
            if c.enabled && c.weight > 0 {
                candidates.push((c.id, c.weight));
            }
        }
        Ok(Self {
            by_id: Arc::new(by_id),
            candidates: Arc::new(candidates),
        })
    }

    #[cfg(test)]
    pub fn from_parts(providers: Vec<(u32, u32, Arc<dyn Storage>)>) -> Self {
        Self {
            candidates: Arc::new(providers.iter().map(|(id, w, _)| (*id, *w)).collect()),
            by_id: Arc::new(providers.into_iter().map(|(id, _, s)| (id, s)).collect()),
        }
    }

    pub fn get(&self, id: u32) -> Option<Arc<dyn Storage>> {
        self.by_id.get(&id).cloned()
    }

    /// choose where a new file goes
    pub fn pick(&self) -> Option<u32> {
        let total: u32 = self.candidates.iter().map(|(_, w)| w).sum();
        if total == 0 {
            return None;
        }
        let mut n = rand::random_range(0..total);
        for (id, w) in self.candidates.iter() {
            if n < *w {
                return Some(*id);
            }
            n -= w;
        }
        None
    }
}
