//! the whole thing, through http, with the ledger, storage and payments in memory

use std::sync::Arc;

use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt;
use ritualistic::SecretKey;
use ritualistic::blossom::{self, Verb};
use serde_json::{Value, json};
use tower::ServiceExt;

use crate::app::App;
use crate::config::*;
use crate::ledger::MemoryLedger;
use crate::payments::MockPayments;
use crate::storage::{MemoryStorage, Providers};
use crate::store::Store;

const ADMIN: &str = "Bearer secret";

struct Env {
    app: Arc<App>,
    router: Router,
    storage: Arc<MemoryStorage>,
}

struct Reply {
    status: StatusCode,
    location: Option<String>,
    body: Value,
}

fn env_with(restrictions: Restrictions) -> Env {
    let config = Config {
        listen: "127.0.0.1:0".into(),
        public_url: "https://cdn.test".into(),
        data_dir: String::new(),
        admin_token: "secret".into(),
        restrictions,
        pricing: Pricing::default(),
        ledger: LedgerConfig::Memory,
        payments: PaymentsConfig::Mock,
        providers: vec![],
    };
    let storage = Arc::new(MemoryStorage::default());
    let app = App::new(
        config,
        Arc::new(MemoryLedger::new()),
        Providers::from_parts(vec![(1, 1, storage.clone())]),
        Arc::new(MockPayments::default()),
        Arc::new(Store::temporary().unwrap()),
    );
    Env {
        router: crate::http::router(app.clone()),
        app,
        storage,
    }
}

fn env() -> Env {
    env_with(Restrictions {
        max_file_size: 1000,
        min_account_payment_sats: 100,
        min_order_sats: 10,
        ..Default::default()
    })
}

impl Env {
    async fn call(&self, method: &str, uri: &str, headers: &[(&str, String)], body: Vec<u8>) -> Reply {
        let mut req = Request::builder().method(method).uri(uri);
        for (k, v) in headers {
            req = req.header(*k, v);
        }
        let res = self.router.clone().oneshot(req.body(Body::from(body)).unwrap()).await.unwrap();
        let status = res.status();
        let location = res.headers().get("location").map(|v| v.to_str().unwrap().to_string());
        let bytes = res.into_body().collect().await.unwrap().to_bytes();
        Reply {
            status,
            location,
            body: serde_json::from_slice(&bytes).unwrap_or(Value::Null),
        }
    }

    async fn json(&self, method: &str, uri: &str, headers: &[(&str, String)], body: Value) -> Reply {
        let mut headers = headers.to_vec();
        headers.push(("content-type", "application/json".into()));
        self.call(method, uri, &headers, body.to_string().into_bytes()).await
    }

    /// create an order for an account and pay it
    async fn fund(&self, sk: &SecretKey, sats: u64) -> Reply {
        let r = self
            .json(
                "POST",
                "/api/account/credit",
                &[("authorization", blossom::create_auth_header(sk, ACCOUNT, &[], None, "", 60))],
                json!({ "amount_sats": sats }),
            )
            .await;
        if r.status != StatusCode::CREATED {
            return r;
        }
        let id = r.body["id"].as_str().unwrap();
        self.json("POST", &format!("/admin/orders/{id}/confirm"), &[("authorization", ADMIN.into())], json!({}))
            .await
    }

    async fn balance(&self, sk: &SecretKey) -> u64 {
        let r = self
            .call("GET", "/api/account", &[("authorization", blossom::create_auth_header(sk, ACCOUNT, &[], None, "", 60))], vec![])
            .await;
        r.body["balance_sats"].as_u64().unwrap()
    }

    async fn upload(&self, sk: &SecretKey, data: &[u8]) -> Reply {
        let hash = blossom::sha256_hex(data);
        self.call(
            "PUT",
            "/upload",
            &[
                ("authorization", blossom::create_auth_header(sk, Verb::Upload, &[&hash], Some("cdn.test"), "upload", 60)),
                ("content-type", "text/plain".into()),
            ],
            data.to_vec(),
        )
        .await
    }

    async fn file(&self, hash: &str) -> Reply {
        self.call("GET", &format!("/api/files/{hash}"), &[], vec![]).await
    }

    async fn pay_for_files(&self, files: Value) -> Reply {
        let r = self.json("POST", "/api/topup", &[], json!({ "files": files })).await;
        if r.status != StatusCode::CREATED {
            return r;
        }
        let id = r.body["id"].as_str().unwrap();
        self.json("POST", &format!("/admin/orders/{id}/confirm"), &[("authorization", ADMIN.into())], json!({}))
            .await
    }
}

const ACCOUNT: Verb = Verb::Custom("account");

#[tokio::test]
async fn life_of_a_file() {
    let env = env();
    let alice = SecretKey::generate();
    let data = b"hello garden";
    let hash = blossom::sha256_hex(data);

    // nothing works before paying
    let r = env.upload(&alice, data).await;
    assert_eq!(r.status, StatusCode::PAYMENT_REQUIRED);
    assert_eq!(env.balance(&alice).await, 0);

    // too little
    assert_eq!(env.fund(&alice, 10).await.status, StatusCode::BAD_REQUEST);

    // the order is pending until the money arrives, and the account doesn't exist
    let r = env
        .json(
            "POST",
            "/api/account/credit",
            &[("authorization", blossom::create_auth_header(&alice, ACCOUNT, &[], None, "", 60))],
            json!({ "amount_sats": 1000 }),
        )
        .await;
    assert_eq!(r.status, StatusCode::CREATED);
    assert_eq!(r.body["status"], "pending");
    let order_id = r.body["id"].as_str().unwrap().to_string();
    assert!(r.body["payment"]["address"].as_str().unwrap().starts_with("mock:"));
    assert!(!env.app.account(&alice.pubkey().0).await.unwrap().exists);
    env.app.check_waiting_orders().await;
    assert!(!env.app.account(&alice.pubkey().0).await.unwrap().exists);

    // paid: the account appears with the money
    let r = env
        .json("POST", &format!("/admin/orders/{order_id}/confirm"), &[("authorization", ADMIN.into())], json!({}))
        .await;
    assert_eq!(r.body["status"], "paid");
    assert_eq!(env.balance(&alice).await, 1000);
    // looking at the order again doesn't credit again
    let r = env.call("GET", &format!("/api/orders/{order_id}"), &[], vec![]).await;
    assert_eq!(r.body["status"], "paid");
    assert_eq!(env.balance(&alice).await, 1000);

    // upload: costs enough for more than a month
    let r = env.upload(&alice, data).await;
    assert_eq!(r.status, StatusCode::OK, "{:?}", r.body);
    assert_eq!(r.body["sha256"], hash);
    assert_eq!(r.body["url"], format!("https://cdn.test/{hash}.txt"));
    assert_eq!(r.body["size"], data.len());
    let paid = 1000 - env.balance(&alice).await;
    assert_eq!(paid, 2 * 31);
    let f = env.file(&hash).await;
    assert_eq!(f.body["balance_sats"], paid);
    assert!(f.body["days_left"].as_u64().unwrap() >= 30);
    assert!(env.storage.blobs.lock().unwrap().contains_key(&hash));

    // serving redirects to the provider, with or without extension
    for path in [format!("/{hash}"), format!("/{hash}.txt")] {
        let r = env.call("GET", &path, &[], vec![]).await;
        assert_eq!(r.status, StatusCode::FOUND);
        assert_eq!(r.location.unwrap(), format!("https://storage.test/{hash}"));
    }
    assert_eq!(env.call("GET", &format!("/{}", "0".repeat(64)), &[], vec![]).await.status, StatusCode::NOT_FOUND);

    // the same file again is free
    assert_eq!(env.upload(&alice, data).await.status, StatusCode::OK);
    assert_eq!(1000 - env.balance(&alice).await, paid);

    // a stranger, with no account, adds sats to the file
    let before = f.body["balance_sats"].as_u64().unwrap();
    let r = env.pay_for_files(json!({ &hash: 40 })).await;
    assert_eq!(r.body["status"], "paid");
    assert_eq!(r.body["credits"][0]["result"], "credited");
    assert_eq!(env.file(&hash).await.body["balance_sats"], before + 40);

    // days go by, the rent is taken once a day
    let rent = 2;
    for day in 0..10 {
        env.app.sweep(day).await.unwrap();
        env.app.sweep(day).await.unwrap();
    }
    assert_eq!(env.file(&hash).await.body["balance_sats"], before + 40 - 10 * rent);

    // until it runs out
    let mut day = 10;
    while env.file(&hash).await.body["expired"] == false {
        env.app.sweep(day).await.unwrap();
        day += 1;
        assert!(day < 100);
    }
    assert_eq!(day, 10 + (before + 40 - 10 * rent) / rent);
    assert_eq!(env.file(&hash).await.body["balance_sats"], 0);
    assert_eq!(env.call("GET", &format!("/{hash}"), &[], vec![]).await.status, StatusCode::NOT_FOUND);
    assert!(!env.storage.blobs.lock().unwrap().contains_key(&hash));

    // too late for topups
    let r = env.json("POST", "/api/topup", &[], json!({ "files": { &hash: 50 } })).await;
    assert_eq!(r.status, StatusCode::NOT_FOUND);

    // but someone can upload it again
    env.app.served.clear();
    assert_eq!(env.upload(&alice, data).await.status, StatusCode::OK);
    assert_eq!(env.call("GET", &format!("/{hash}"), &[], vec![]).await.status, StatusCode::FOUND);
    assert_eq!(env.file(&hash).await.body["expired"], false);
}

#[tokio::test]
async fn one_order_many_files() {
    let env = env();
    let alice = SecretKey::generate();
    env.fund(&alice, 1000).await;
    let (a, b) = (blossom::sha256_hex(b"aaa"), blossom::sha256_hex(b"bbb"));
    env.upload(&alice, b"aaa").await;
    env.upload(&alice, b"bbb").await;
    let (fa, fb) = (
        env.file(&a).await.body["balance_sats"].as_u64().unwrap(),
        env.file(&b).await.body["balance_sats"].as_u64().unwrap(),
    );

    let r = env.pay_for_files(json!({ &a: 30, &b: 70 })).await;
    assert_eq!(r.body["amount_sats"], 100);
    assert_eq!(r.body["status"], "paid");
    assert_eq!(env.file(&a).await.body["balance_sats"], fa + 30);
    assert_eq!(env.file(&b).await.body["balance_sats"], fb + 70);

    // all or nothing at order creation
    let missing = "f".repeat(64);
    let r = env.json("POST", "/api/topup", &[], json!({ "files": { &a: 30, &missing: 70 } })).await;
    assert_eq!(r.status, StatusCode::NOT_FOUND);
    let r = env.json("POST", "/api/topup", &[], json!({ "files": { "nope": 30 } })).await;
    assert_eq!(r.status, StatusCode::BAD_REQUEST);
    let r = env.json("POST", "/api/topup", &[], json!({ "files": {} })).await;
    assert_eq!(r.status, StatusCode::BAD_REQUEST);
    let r = env.json("POST", "/api/topup", &[], json!({ "files": { &a: 1 } })).await;
    assert_eq!(r.status, StatusCode::BAD_REQUEST, "below the minimum");
}

#[tokio::test]
async fn a_file_that_dies_with_a_payment_in_flight() {
    let env = env();
    let alice = SecretKey::generate();
    env.fund(&alice, 1000).await;
    let hash = blossom::sha256_hex(b"short lived");
    env.upload(&alice, b"short lived").await;

    // the order is made while the file is alive
    let r = env.json("POST", "/api/topup", &[], json!({ "files": { &hash: 50 } })).await;
    let id = r.body["id"].as_str().unwrap().to_string();

    // and the file dies before it's paid
    let mut day = 0;
    while env.file(&hash).await.body["expired"] == false {
        env.app.sweep(day).await.unwrap();
        day += 1;
    }
    let r = env
        .json("POST", &format!("/admin/orders/{id}/confirm"), &[("authorization", ADMIN.into())], json!({}))
        .await;
    assert_eq!(r.body["status"], "paid");
    assert_eq!(r.body["credits"][0]["result"], "gone");
    assert_eq!(env.file(&hash).await.body["balance_sats"], 0);
}

#[tokio::test]
async fn authorization_is_checked() {
    let env = env();
    let alice = SecretKey::generate();
    env.fund(&alice, 1000).await;
    let data = b"data";
    let hash = blossom::sha256_hex(data);
    let up = |auth: String| {
        let env = &env;
        async move {
            env.call("PUT", "/upload", &[("authorization", auth)], data.to_vec()).await.status
        }
    };

    assert_eq!(env.call("PUT", "/upload", &[], data.to_vec()).await.status, StatusCode::UNAUTHORIZED);
    // another action
    assert_eq!(up(blossom::create_auth_header(&alice, Verb::Delete, &[&hash], None, "", 60)).await, StatusCode::UNAUTHORIZED);
    // another file
    assert_eq!(up(blossom::create_auth_header(&alice, Verb::Upload, &[&"1".repeat(64)], None, "", 60)).await, StatusCode::UNAUTHORIZED);
    // another server
    assert_eq!(up(blossom::create_auth_header(&alice, Verb::Upload, &[&hash], Some("evil.test"), "", 60)).await, StatusCode::UNAUTHORIZED);
    assert_eq!(env.file(&hash).await.status, StatusCode::NOT_FOUND);

    // none of the above cost anything, and the good one works with no x or server tags too
    assert_eq!(env.balance(&alice).await, 1000);
    assert_eq!(up(blossom::create_auth_header(&alice, Verb::Upload, &[], None, "", 60)).await, StatusCode::OK);

    // the account api doesn't take upload authorizations
    let r = env
        .call("GET", "/api/account", &[("authorization", blossom::create_auth_header(&alice, Verb::Upload, &[], None, "", 60))], vec![])
        .await;
    assert_eq!(r.status, StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn restrictions() {
    let carol = SecretKey::generate();
    let env = env_with(Restrictions {
        max_file_size: 10,
        min_account_payment_sats: 100,
        allowed_mime_types: vec!["text/*".into()],
        denied_pubkeys: [carol.pubkey().to_hex()].into(),
        ..Default::default()
    });

    // denied pubkeys can't even pay
    assert_eq!(env.fund(&carol, 1000).await.status, StatusCode::FORBIDDEN);

    let alice = SecretKey::generate();
    env.fund(&alice, 1000).await;
    assert_eq!(env.upload(&alice, b"0123456789").await.status, StatusCode::OK);
    assert_eq!(env.upload(&alice, b"0123456789a").await.status, StatusCode::PAYLOAD_TOO_LARGE);
    assert_eq!(env.upload(&alice, b"").await.status, StatusCode::BAD_REQUEST);

    let hash = blossom::sha256_hex(b"png");
    let r = env
        .call(
            "PUT",
            "/upload",
            &[
                ("authorization", blossom::create_auth_header(&alice, Verb::Upload, &[&hash], None, "", 60)),
                ("content-type", "image/png".into()),
            ],
            b"png".to_vec(),
        )
        .await;
    assert_eq!(r.status, StatusCode::UNSUPPORTED_MEDIA_TYPE);

    // the preflight says the same without sending the file
    let head = |len: &str, mime: &str| {
        let env = &env;
        let (len, mime) = (len.to_string(), mime.to_string());
        async move {
            env.call("HEAD", "/upload", &[("x-content-length", len), ("x-content-type", mime)], vec![]).await.status
        }
    };
    assert_eq!(head("5", "text/plain").await, StatusCode::OK);
    assert_eq!(head("500", "text/plain").await, StatusCode::PAYLOAD_TOO_LARGE);
    assert_eq!(head("5", "image/png").await, StatusCode::UNSUPPORTED_MEDIA_TYPE);
}

#[tokio::test]
async fn banning_an_uploader_removes_what_they_uploaded() {
    let env = env();
    let (evil, good) = (SecretKey::generate(), SecretKey::generate());
    env.fund(&evil, 1000).await;
    env.fund(&good, 1000).await;
    let (bad1, bad2, fine) = (b"bad one".as_slice(), b"bad two".as_slice(), b"fine".as_slice());
    for (sk, data) in [(&evil, bad1), (&evil, bad2), (&good, fine)] {
        assert_eq!(env.upload(sk, data).await.status, StatusCode::OK);
    }
    let h = |d: &[u8]| blossom::sha256_hex(d);

    // we can tell who uploaded, and nobody else can
    let admin = [("authorization", ADMIN.to_string())];
    let r = env.call("GET", &format!("/admin/files/{}", h(bad1)), &admin, vec![]).await;
    assert_eq!(r.body["uploaders"], json!([evil.pubkey().to_hex()]));
    let r = env.call("GET", &format!("/admin/files/{}", h(bad1)), &[("authorization", "Bearer nope".into())], vec![]).await;
    assert_eq!(r.status, StatusCode::UNAUTHORIZED);
    let r = env.call("GET", &format!("/admin/files/{}", h(bad1)), &[], vec![]).await;
    assert_eq!(r.status, StatusCode::UNAUTHORIZED);

    let r = env
        .json("POST", "/admin/ban", &admin, json!({ "pubkey": evil.pubkey().to_hex(), "purge": true }))
        .await;
    assert_eq!(r.status, StatusCode::OK);
    assert_eq!(r.body["purged"].as_array().unwrap().len(), 2);

    for d in [bad1, bad2] {
        env.app.served.clear();
        assert_eq!(env.call("GET", &format!("/{}", h(d)), &[], vec![]).await.status, StatusCode::NOT_FOUND);
        assert!(!env.storage.blobs.lock().unwrap().contains_key(&h(d)));
        // and it can't come back, not even from somebody else
        assert_eq!(env.upload(&good, d).await.status, StatusCode::FORBIDDEN);
    }
    assert_eq!(env.call("GET", &format!("/{}", h(fine)), &[], vec![]).await.status, StatusCode::FOUND);

    // the account is blocked
    assert_eq!(env.upload(&evil, b"more evil").await.status, StatusCode::FORBIDDEN);
    assert_eq!(env.fund(&evil, 1000).await.status, StatusCode::FORBIDDEN);
    let r = env.call("GET", "/admin/bans", &admin, vec![]).await;
    assert_eq!(r.body["pubkeys"], json!([evil.pubkey().to_hex()]));
    assert_eq!(r.body["hashes"].as_array().unwrap().len(), 2);
}

#[tokio::test]
async fn deletions_that_fail_are_retried() {
    let env = env();
    let alice = SecretKey::generate();
    env.fund(&alice, 1000).await;
    let hash = blossom::sha256_hex(b"stubborn");
    env.upload(&alice, b"stubborn").await;

    env.storage.fail_deletes.store(true, std::sync::atomic::Ordering::SeqCst);
    let mut day = 0;
    while env.file(&hash).await.body["expired"] == false {
        env.app.sweep(day).await.unwrap();
        day += 1;
    }
    // it's gone as far as anyone can tell, but the bytes are still there
    assert_eq!(env.call("GET", &format!("/{hash}"), &[], vec![]).await.status, StatusCode::NOT_FOUND);
    assert!(env.storage.blobs.lock().unwrap().contains_key(&hash));
    env.app.retry_deletes().await;
    assert!(env.storage.blobs.lock().unwrap().contains_key(&hash));

    env.storage.fail_deletes.store(false, std::sync::atomic::Ordering::SeqCst);
    env.app.retry_deletes().await;
    assert!(!env.storage.blobs.lock().unwrap().contains_key(&hash));
    assert!(env.app.store.pending_deletes().unwrap().is_empty());
}

#[tokio::test]
async fn a_payment_for_a_file_that_is_uploaded_again_is_not_deleted() {
    let env = env();
    let alice = SecretKey::generate();
    env.fund(&alice, 1000).await;
    let hash = blossom::sha256_hex(b"phoenix");
    env.upload(&alice, b"phoenix").await;

    env.storage.fail_deletes.store(true, std::sync::atomic::Ordering::SeqCst);
    let mut day = 0;
    while env.file(&hash).await.body["expired"] == false {
        env.app.sweep(day).await.unwrap();
        day += 1;
    }
    env.storage.fail_deletes.store(false, std::sync::atomic::Ordering::SeqCst);

    assert_eq!(env.upload(&alice, b"phoenix").await.status, StatusCode::OK);
    env.app.retry_deletes().await;
    assert!(env.storage.blobs.lock().unwrap().contains_key(&hash));
    assert_eq!(env.file(&hash).await.body["expired"], false);
}

#[tokio::test]
async fn provider_choice_follows_weights() {
    let (a, b) = (Arc::new(MemoryStorage::default()), Arc::new(MemoryStorage::default()));
    let providers = Providers::from_parts(vec![(1, 0, a.clone()), (2, 3, b.clone())]);
    for _ in 0..50 {
        assert_eq!(providers.pick(), Some(2));
    }
    assert_eq!(Providers::from_parts(vec![]).pick(), None);
}
