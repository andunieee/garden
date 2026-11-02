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

use crate::app::{App, parse_hash};
use crate::config::*;
use crate::ledger::{OrangeLedger, WithdrawalFrom};
use crate::payments::{MockPayments, Payments, Switch};
use crate::storage::{MemoryStorage, Providers};
use crate::store::Store;


struct Env {
    app: Arc<App>,
    router: Router,
    storage: Arc<MemoryStorage>,
    payments: Arc<MockPayments>,
}

/// the hyperb identity of the operator in these tests
fn operator_secret() -> ed25519_dalek::SigningKey {
    ed25519_dalek::SigningKey::from_bytes(&[7; 32])
}

fn operator_key() -> String {
    lowercase_hex::encode(operator_secret().verifying_key().to_bytes())
}

fn provider(id: u32) -> ProviderConfig {
    ProviderConfig {
        id,
        kind: ProviderKind::B2,
        enabled: true,
        weight: 1,
        bucket: "b".into(),
        region: String::new(),
        endpoint: "http://localhost".into(),
        access_key_id: String::new(),
        secret_access_key: String::new(),
        public_url: String::new(),
    }
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
        operators: vec![operator_key()],
        fiat: Fiat::default(),
        restrictions,
        pricing: Pricing::default(),
        ledger: LedgerConfig::Memory,
        payments: Some(PaymentsConfig::Mock),
        providers: vec![provider(1)],
        path: None,
    };
    let storage = Arc::new(MemoryStorage::default());
    let payments = Arc::new(MockPayments::default());
    let app = App::new(
        config,
        Arc::new(OrangeLedger::in_memory().unwrap()),
        Providers::from_parts(vec![(1, 1, storage.clone())]),
        Arc::new(Switch::new(Some(payments.clone()))),
        Arc::new(Store::temporary().unwrap()),
    );
    Env {
        router: crate::http::router(app.clone()),
        app,
        storage,
        payments,
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
        self.confirm(&id).await
    }

    /// the money for an order arrives
    async fn confirm(&self, id: &str) -> Reply {
        self.app.confirm_mock(id).await.unwrap();
        self.call("GET", &format!("/api/orders/{id}"), &[], vec![]).await
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
        self.confirm(&id).await
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
    let r = env.confirm(&order_id).await;
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
    let r = env.confirm(&id).await;
    assert_eq!(r.body["status"], "paid");
    assert_eq!(r.body["credits"][0]["result"], "gone");
    assert_eq!(env.file(&hash).await.body["balance_sats"], 0);
    // the money did arrive, so it's in the books as unclaimed
    assert_eq!(env.app.ledger.totals().await.unwrap().unclaimed, 50);
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

    // we can tell who uploaded
    assert_eq!(env.app.uploaders(&parse_hash(&h(bad1)).unwrap()).await.unwrap(), vec![evil.pubkey().to_hex()]);

    let purged = env.app.ban_pubkey(&evil.pubkey().0, true).await.unwrap();
    assert_eq!(purged.len(), 2);

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
    assert_eq!(env.app.store.banned_pubkeys().unwrap(), vec![evil.pubkey().to_hex()]);
    assert_eq!(env.app.store.banned_hashes().unwrap().len(), 2);
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

#[tokio::test]
async fn what_is_paid_and_not_used_goes_to_the_suspense_account() {
    let env = env();
    let alice = SecretKey::generate();
    let r = env
        .json(
            "POST",
            "/api/account/credit",
            &[("authorization", blossom::create_auth_header(&alice, ACCOUNT, &[], None, "", 60))],
            json!({ "amount_sats": 1000 }),
        )
        .await;
    let id = r.body["id"].as_str().unwrap().to_string();
    let order = env.app.store.order(&id).unwrap().unwrap();

    // less than asked is not enough
    env.payments.simulate(&order.payment, 999);
    assert_eq!(env.call("GET", &format!("/api/orders/{id}"), &[], vec![]).await.body["status"], "pending");
    assert!(!env.app.account(&alice.pubkey().0).await.unwrap().exists);

    // and more than asked is credited as asked, with the rest kept apart
    env.payments.simulate(&order.payment, 301);
    let r = env.call("GET", &format!("/api/orders/{id}"), &[], vec![]).await;
    assert_eq!((r.body["status"].as_str(), r.body["received_sats"].as_u64()), (Some("paid"), Some(1300)));
    assert_eq!(env.balance(&alice).await, 1000);
    let t = env.app.ledger.totals().await.unwrap();
    assert_eq!((t.received, t.unclaimed), (1300, 300));
}

#[tokio::test]
async fn the_wallet_sends_and_the_books_follow() {
    let env = env();
    let sk = SecretKey::generate();
    assert_eq!(env.fund(&sk, 2000).await.status, StatusCode::OK);

    // 500 sats arrived without an order claiming them
    let extra = crate::payments::PaymentRequest {
        method: "mock".into(),
        address: "mock:extra".into(),
        uri: "mock:extra".into(),
    };
    assert!(env.payments.simulate(&extra, 500));
    env.app.ledger.record_unclaimed(500, 1).await.unwrap();

    // the operator sends some of them out
    let report = env
        .app
        .send_sats(WithdrawalFrom::Suspense, "tark1test", 100, "send-1", "rent")
        .await
        .unwrap();
    assert_eq!(report.txid, "mock-send-100");
    assert_eq!(report.wallet_balance, Some(2400));

    // the same reference can't be sent twice, and the source can't go below zero
    assert!(env
        .app
        .send_sats(WithdrawalFrom::Suspense, "tark1test", 100, "send-1", "rent")
        .await
        .is_err());
    assert!(env
        .app
        .send_sats(WithdrawalFrom::Suspense, "tark1test", 1000, "send-2", "")
        .await
        .is_err());

    let t = env.app.ledger.totals().await.unwrap();
    assert_eq!((t.received, t.unclaimed, t.withdrawn), (2500, 400, 100));
    let a = env.app.audit(1).await.unwrap();
    assert_eq!(a.integrity.difference, 0);
    assert_eq!(a.wallet.unwrap().difference, 0);

    // unclaimed sats can be given to an account instead: no wallet movement
    let bob = SecretKey::generate();
    assert!(env
        .app
        .grant_unclaimed(&bob.pubkey().0, 50, "grant-1", "refund")
        .await
        .unwrap());
    assert!(!env
        .app
        .grant_unclaimed(&bob.pubkey().0, 50, "grant-1", "refund")
        .await
        .unwrap());
    assert_eq!(env.balance(&bob).await, 50);
    assert_eq!(env.app.ledger.totals().await.unwrap().unclaimed, 350);
    let a = env.app.audit(1).await.unwrap();
    assert_eq!(a.integrity.difference, 0);
    assert_eq!(a.wallet.unwrap().difference, 0);
}

#[tokio::test]
async fn the_operator_wallet_sends_and_grants() {
    let env = env();
    let mut op = operator(&env).await;
    let sk = SecretKey::generate();
    assert_eq!(env.fund(&sk, 2000).await.status, StatusCode::OK);

    // a send asks first, then moves the money out of the account
    let wallet = json!({
        "from": "user",
        "account": sk.pubkey().to_hex(),
        "amount": "500",
        "address": "tark1test",
        "note": "a payout",
    });
    let out = op.handle(&hyperb_action("ask_send", json!({"wallet": wallet}), None)).await;
    assert!(out.contains("Send 500 sats"), "{out}");
    assert!(out.contains("\"action\":\"send\""), "{out}");

    let out = op
        .handle(&hyperb_action(
            "confirmed",
            json!({"wallet": wallet, "confirm": {"open": true, "action": "send", "arg": ""}}),
            None,
        ))
        .await;
    assert!(out.contains("Sent 500 sats"), "{out}");
    assert_eq!(env.balance(&sk).await, 1500);
    assert_eq!(env.app.ledger.totals().await.unwrap().withdrawn, 500);

    // and unclaimed sats can be given to another account without touching the wallet
    env.app.ledger.record_unclaimed(100, 1).await.unwrap();
    let bob = SecretKey::generate();
    let wallet = json!({
        "from": "revenue",
        "grant_account": bob.pubkey().to_hex(),
        "grant_amount": "100",
        "grant_note": "refund",
    });
    let out = op.handle(&hyperb_action("ask_grant", json!({"wallet": wallet}), None)).await;
    assert!(out.contains("Give 100 sats"), "{out}");
    let out = op
        .handle(&hyperb_action(
            "confirmed",
            json!({"wallet": wallet, "confirm": {"open": true, "action": "grant", "arg": ""}}),
            None,
        ))
        .await;
    assert!(out.contains("Gave 100 sats"), "{out}");
    assert_eq!(env.balance(&bob).await, 100);
    assert_eq!(env.app.ledger.totals().await.unwrap().unclaimed, 0);
}

#[tokio::test]
async fn the_audit_follows_the_money() {
    let env = env();
    let alice = SecretKey::generate();
    env.fund(&alice, 1000).await;
    env.upload(&alice, b"audit me").await;
    let today = crate::app::now() / 86400;
    env.app.sweep(today).await.unwrap();

    let a = env.app.audit(30).await.unwrap();
    assert_eq!(a.integrity.difference, 0);
    assert_eq!((a.integrity.received, a.integrity.earned), (1000, 2));
    assert_eq!(a.wallet.as_ref().unwrap().difference, 0);
    assert_eq!(a.providers.len(), 1);
    assert_eq!((a.providers[0].earned_sats, a.providers[0].paid_minor), (2, 0));
    assert_eq!(a.obligations.open_files, 1);
    assert_eq!(a.obligations.in_accounts, 1000 - 62);

    // the operator takes the 2 sats out and sells them, but forgets to say so
    *env.payments.spent.lock().unwrap() += 2;
    let a = env.app.audit(30).await.unwrap();
    assert_eq!(a.wallet.as_ref().unwrap().difference, -2);
    assert!(a.warnings.iter().any(|w| w.level == crate::audit::Level::Failure && w.text.contains("2 sats less")));

    // now they say so
    assert!(env.app.record_conversion("trade-1", 2, 10, "market").await.unwrap());
    assert!(!env.app.record_conversion("trade-1", 2, 10, "market").await.unwrap());
    // the same reference with other numbers is a mistake, not a repeat
    assert!(env.app.record_conversion("trade-1", 3, 10, "").await.is_err());
    let a = env.app.audit(30).await.unwrap();
    assert_eq!(a.wallet.as_ref().unwrap().difference, 0);
    assert_eq!((a.position.sold_sats, a.position.unsold_sats), (2, 0));
    assert_eq!(a.position.rate_source, Some("realized"));

    // a bill
    let date = today * 86400;
    assert!(env.app.record_expense(1, "inv-1", 300, date, "storage").await.unwrap());
    assert!(!env.app.record_expense(1, "inv-1", 300, date, "storage").await.unwrap());
    assert!(env.app.record_expense(9, "inv-1", 300, date, "").await.is_err(), "no such provider");
    assert!(env.app.record_expense(1, " ", 300, date, "").await.is_err(), "no reference");
    assert!(env.app.record_expense(1, "inv-2", 0, date, "").await.is_err(), "nothing paid");
    let a = env.app.audit(30).await.unwrap();
    assert_eq!(a.providers[0].paid_minor, 300);
    assert_eq!(a.position.fiat_on_hand_minor, 10 - 300);
    // a bill from before the period isn't in it
    env.app.record_expense(1, "inv-old", 999, (today - 200) * 86400, "").await.unwrap();
    assert_eq!(env.app.audit(30).await.unwrap().providers[0].paid_minor, 300);
    assert_eq!(env.app.audit(365).await.unwrap().providers[0].paid_minor, 1299);
}

// the operator interface

fn hyperb_action(name: &str, signals: serde_json::Value, payload: Option<serde_json::Value>) -> String {
    let mut a = json!({ "name": name, "signals": signals });
    if let Some(p) = payload {
        a["payload"] = p;
    }
    format!("action {a}\n")
}

async fn operator(env: &Env) -> crate::operator::OperatorSession {
    let mut session = crate::operator::OperatorSession::new(env.app.clone());
    let out = session.handle("hello {}\n").await;
    assert!(out.contains("\"id\":\"audit_run\""), "{out}");
    assert!(out.contains("Nothing entered yet."));
    session
}

#[tokio::test]
async fn the_operator_enters_the_books() {
    let env = env();
    let mut op = operator(&env).await;

    let expense = |provider: &str, amount: &str, date: &str, reference: &str| {
        hyperb_action(
            "expense",
            json!({ "expense": { "provider": provider, "amount": amount, "date": date, "reference": reference, "note": "march" } }),
            None,
        )
    };

    let out = op.handle(&expense("1", "12.34", "2026-03-01", "inv-9")).await;
    assert!(out.contains("Recorded."), "{out}");
    assert!(out.contains("2026-03-01 · 12.34 USD paid to 1 · b2 · inv-9 — march"), "{out}");
    let e = env.app.ledger.expenses(1).await.unwrap();
    assert_eq!((e[0].amount_minor, e[0].date), (1234, crate::operator::parse_date("2026-03-01").unwrap()));

    assert!(op.handle(&expense("1", "12.34", "2026-03-01", "inv-9")).await.contains("recorded before"));
    assert_eq!(env.app.ledger.expenses(1).await.unwrap().len(), 1);

    // mistakes are explained and change nothing
    assert!(op.handle(&expense("1", "abc", "2026-03-01", "x")).await.contains("The amount isn't a number."));
    assert!(op.handle(&expense("1", "1", "2026-02-31", "x")).await.contains("The date should look like"));
    assert!(op.handle(&expense("1", "1", "2026-03-01", "")).await.contains("a reference is needed"));
    assert!(op.handle(&expense("7", "1", "2026-03-01", "x")).await.contains("there is no provider 7"));
    assert_eq!(env.app.ledger.expenses(1).await.unwrap().len(), 1);

    let out = op
        .handle(&hyperb_action("sale", json!({ "sale": { "sats": "1,000", "fiat": "5.00", "reference": "t-1", "note": "" } }), None))
        .await;
    assert!(out.contains("Recorded."), "{out}");
    assert!(out.contains("Sold 1,000 sats for 5.00 USD · t-1"), "{out}");
    let c = env.app.ledger.conversions().await.unwrap();
    assert_eq!((c[0].sats, c[0].fiat_minor), (1000, 500));

    op.handle(&hyperb_action("set_rate", json!({ "rate": { "value": "60,000" } }), None)).await;
    assert_eq!(env.app.store.rate().unwrap().unwrap().fiat_minor_per_btc, 6_000_000);

    // $0.006 per GB per month is 0.6 cents
    let out = op
        .handle(&hyperb_action("set_prices", json!({ "prices": { "provider": "1", "storage": "0.006", "egress": "" } }), None))
        .await;
    assert!(out.contains("Saved."), "{out}");
    let prices = env.app.store.prices(1).unwrap().unwrap();
    assert!((prices.storage_per_gb_month_minor - 0.6).abs() < 1e-9);
    assert_eq!(prices.egress_per_gb_minor, 0.0);
    // and picking the provider again shows them
    let out = op.handle(&hyperb_action("load_prices", json!({ "prices": { "provider": "1" } }), None)).await;
    assert!(out.contains("\"storage\":\"0.006\""), "{out}");

    // the audit uses it all
    let out = op.handle(&hyperb_action("audit", json!({ "audit": { "days": "30" } }), None)).await;
    assert!(out.contains("The ledger adds up"), "{out}");
    assert!(out.contains("60,000.00 USD for 1 BTC (operator)"), "{out}");
}

#[tokio::test]
async fn payments_are_set_up_in_the_operator_interface() {
    let mut env = env();
    let mut config = env.app.config.clone();
    config.payments = None;
    env.app = App::new(
        config,
        Arc::new(OrangeLedger::in_memory().unwrap()),
        Providers::from_parts(vec![(1, 1, env.storage.clone())]),
        Arc::new(Switch::default()),
        Arc::new(Store::temporary().unwrap()),
    );
    env.router = crate::http::router(env.app.clone());

    // nobody can pay until they are
    let sk = SecretKey::generate();
    assert_eq!(env.fund(&sk, 1000).await.status, StatusCode::BAD_GATEWAY);
    assert_eq!(env.pay_for_files(json!({})).await.status, StatusCode::BAD_REQUEST);

    let mut op = operator(&env).await;
    let out = op.handle("hello {}\n").await;
    assert!(out.contains("Payments are not set up"), "{out}");
    assert!(out.contains("\"id\":\"pay_go\""), "{out}");

    // mistakes are caught before anything is tried
    let form = |server: &str, url: &str, wallet: &str, seed: &str| {
        hyperb_action(
            "ask_payments",
            json!({ "payments": { "server": server, "url": url, "wallet": wallet, "seed": seed } }),
            None,
        )
    };
    assert!(op.handle(&form("custom", "ftp://x", "new", "")).await.contains("has to start with https://"));
    assert!(op.handle(&form("mutinynet", "", "seed", "abc")).await.contains("A seed is 64 hex characters."));
    assert!(op.handle(&form("mutinynet", "", "keep", "")).await.contains("There is no wallet yet."));
    // a server that isn't there changes nothing
    let out = op.handle(&form("custom", "http://127.0.0.1:9", "new", "")).await;
    assert!(out.contains("\"kind\":\"failure\""), "{out}");
    assert!(env.app.store.arkade().unwrap().is_none());
    assert!(!env.app.payments.is_ready());
}

/// `cargo test -- --ignored mutinynet`
#[tokio::test]
#[ignore = "talks to arkade's mutinynet server"]
async fn payments_on_mutinynet_from_the_operator_interface() {
    let mut env = env();
    let mut config = env.app.config.clone();
    config.payments = None;
    env.app = App::new(
        config,
        Arc::new(OrangeLedger::in_memory().unwrap()),
        Providers::from_parts(vec![(1, 1, env.storage.clone())]),
        Arc::new(Switch::default()),
        Arc::new(Store::temporary().unwrap()),
    );
    env.router = crate::http::router(env.app.clone());
    let mut op = operator(&env).await;

    let form = json!({ "payments": { "server": "mutinynet", "url": "", "wallet": "new", "seed": "" } });
    let out = op.handle(&hyperb_action("ask_payments", form, None)).await;
    assert!(out.contains("Connected, on signet: test money."), "{out}");
    assert!(out.contains("Taking payments (signet, test money); the wallet holds 0 sats."), "{out}");

    let sk = SecretKey::generate();
    let r = env
        .json(
            "POST",
            "/api/account/credit",
            &[("authorization", blossom::create_auth_header(&sk, ACCOUNT, &[], None, "", 60))],
            json!({ "amount_sats": 1000 }),
        )
        .await;
    assert_eq!(r.status, StatusCode::CREATED, "{}", r.body);
    assert!(r.body["payment"]["address"].as_str().unwrap().starts_with("tark1"), "{}", r.body);

    // the seed is shown when asked for, and the wallet can't be changed while the order waits
    let out = op.handle(&hyperb_action("confirmed", json!({ "confirm": { "action": "reveal", "arg": "" } }), None)).await;
    assert!(out.contains(&env.app.store.arkade().unwrap().unwrap().seed), "{out}");
    let form = json!({ "payments": { "server": "mutinynet", "url": "", "wallet": "new", "seed": "" } });
    let out = op.handle(&hyperb_action("confirmed", json!({ "confirm": { "action": "payments", "arg": "" }, "payments": form["payments"] }), None)).await;
    assert!(out.contains("1 orders are waiting"), "{out}");
}

#[tokio::test]
async fn payments_in_the_config_file_are_not_changed_from_the_interface() {
    let env = env();
    let mut op = operator(&env).await;
    let out = op.handle("hello {}\n").await;
    assert!(out.contains("kind = \\\"mock\\\""), "{out}");
    assert!(!out.contains("\"id\":\"pay_go\""), "{out}");
    let settings = crate::store::ArkadeSettings { server_url: "https://x".into(), seed: "11".repeat(32) };
    assert!(matches!(env.app.set_up_arkade(settings).await, Err(crate::app::AppError::BadRequest(_))));
}

#[tokio::test]
async fn the_operator_moderates_and_runs_the_rent() {
    let env = env();
    let (evil, good) = (SecretKey::generate(), SecretKey::generate());
    env.fund(&evil, 1000).await;
    env.fund(&good, 1000).await;
    env.upload(&evil, b"evil bytes").await;
    env.upload(&good, b"good bytes").await;
    let hash = blossom::sha256_hex(b"evil bytes");
    let mut op = operator(&env).await;

    // who did it
    let out = op.handle(&hyperb_action("uploaders", json!({ "mod": { "hash": hash } }), None)).await;
    assert!(out.contains(&format!("uploaded by {}", evil.pubkey().to_hex())), "{out}");
    assert!(op.handle(&hyperb_action("uploaders", json!({ "mod": { "hash": "nope" } }), None)).await.contains("isn't a sha256"));
    let out = op.handle(&hyperb_action("files_of", json!({ "mod": { "pubkey": evil.pubkey().to_hex() } }), None)).await;
    assert!(out.contains("1 files by") && out.contains(&hash), "{out}");

    // asking doesn't do anything yet
    let ban = json!({ "mod": { "pubkey": evil.pubkey().to_hex(), "purge": true } });
    let out = op.handle(&hyperb_action("ask_ban", ban, None)).await;
    assert!(out.contains("\"open\":true") && out.contains("\"action\":\"ban_and_purge\""), "{out}");
    assert!(!env.app.store.is_pubkey_banned(&evil.pubkey().to_hex()).unwrap());
    assert_eq!(env.call("GET", &format!("/{hash}"), &[], vec![]).await.status, StatusCode::FOUND);

    // saying yes does
    let yes = json!({ "confirm": { "action": "ban_and_purge", "arg": evil.pubkey().to_hex() } });
    let out = op.handle(&hyperb_action("confirmed", yes, None)).await;
    assert!(out.contains("Banned, and 1 files removed."), "{out}");
    assert!(out.contains(&format!("account {}", evil.pubkey().to_hex())), "the list of bans is updated");
    env.app.served.clear();
    assert_eq!(env.call("GET", &format!("/{hash}"), &[], vec![]).await.status, StatusCode::NOT_FOUND);
    assert_eq!(env.upload(&evil, b"more").await.status, StatusCode::FORBIDDEN);

    // one file
    let hash = blossom::sha256_hex(b"good bytes");
    let out = op.handle(&hyperb_action("ask_purge", json!({ "mod": { "hash": hash } }), None)).await;
    assert!(out.contains("\"action\":\"purge\""));
    let out = op
        .handle(&hyperb_action("confirmed", json!({ "confirm": { "action": "purge", "arg": hash } }), None))
        .await;
    assert!(out.contains("Removed."), "{out}");
    assert!(env.app.store.is_hash_banned(&hash).unwrap());

    // nothing happens for an action nobody asked about
    let out = op.handle(&hyperb_action("confirmed", json!({ "confirm": { "action": "", "arg": "" } }), None)).await;
    assert!(!out.contains("Removed"));

    // rent
    env.upload(&good, b"rent payer").await;
    let out = op.handle(&hyperb_action("sweep", json!({}), None)).await;
    assert!(out.contains("1 files were charged 2 sats"), "{out}");

    // orders waiting, and marking them as paid since payments are mock here
    let hash = blossom::sha256_hex(b"rent payer");
    let order = env.app.files_order([(hash.clone(), 77)].into()).await.unwrap();
    let out = op.handle(&hyperb_action("orders", json!({}), None)).await;
    assert!(out.contains("1 files") && out.contains("77 sats") && out.contains("Mark as paid"), "{out}");
    let out = op.handle(&hyperb_action("mock_paid", json!({}), Some(json!(order.id)))).await;
    assert!(out.contains("Marked as paid."), "{out}");
    assert!(!out.contains("Mark as paid"), "it's not waiting any more");
    assert_eq!(env.app.file_view(&parse_hash(&hash).unwrap()).await.unwrap().balance_sats, 62 - 2 + 77);
}

#[test]
fn dates() {
    use crate::operator::{format_date, parse_date};
    assert_eq!(parse_date("1970-01-01"), None, "too old to be a bill");
    assert_eq!(parse_date("2000-03-01"), Some(951_868_800));
    assert_eq!(format_date(951_868_800), "2000-03-01");
    assert_eq!(parse_date("2024-02-29").map(format_date).as_deref(), Some("2024-02-29"));
    assert_eq!(parse_date("2025-02-29"), None);
    assert_eq!(parse_date("2025-13-01"), None);
    assert_eq!(parse_date("2025-1-1").map(format_date).as_deref(), Some("2025-01-01"));
    assert_eq!(parse_date("tomorrow"), None);
    assert_eq!(format_date(1_700_000_000), "2023-11-14");
}

// the hyperb session, authenticated by the key of the operator

mod hyperb {
    use super::*;
    use ed25519_dalek::Signer;
    use http_body_util::BodyExt;

    async fn send(
        env: &Env,
        method: &str,
        headers: &[(&str, String)],
        body: &str,
    ) -> axum::http::Response<Body> {
        let mut req = Request::builder().method(method).uri("/operator");
        for (k, v) in headers {
            req = req.header(*k, v);
        }
        env.router.clone().oneshot(req.body(Body::from(body.to_string())).unwrap()).await.unwrap()
    }

    fn sign(key: &ed25519_dalek::SigningKey, session: &str) -> String {
        lowercase_hex::encode(key.sign(session.as_bytes()).to_bytes())
    }

    async fn hello(env: &Env, key: &ed25519_dalek::SigningKey) -> axum::http::Response<Body> {
        let k = lowercase_hex::encode(key.verifying_key().to_bytes());
        send(env, "POST", &[("hyperb-key", k)], "hello {}\n").await
    }

    fn session_of(res: &axum::http::Response<Body>) -> String {
        res.headers()["hyperb-session"].to_str().unwrap().to_string()
    }

    fn signed(key: &ed25519_dalek::SigningKey, session: &str) -> Vec<(&'static str, String)> {
        vec![
            ("hyperb-key", lowercase_hex::encode(key.verifying_key().to_bytes())),
            ("hyperb-session", session.to_string()),
            ("hyperb-signature", sign(key, session)),
        ]
    }

    /// reads the stream until `needle` shows up
    async fn read_until(body: &mut Body, needle: &str) -> String {
        let mut seen = String::new();
        loop {
            let frame = tokio::time::timeout(std::time::Duration::from_secs(5), body.frame())
                .await
                .unwrap_or_else(|_| panic!("timed out waiting for {needle:?}, got {seen}"));
            let frame = frame.unwrap_or_else(|| panic!("stream ended waiting for {needle:?}, got {seen}")).unwrap();
            if let Some(data) = frame.data_ref() {
                seen.push_str(&String::from_utf8_lossy(data));
            }
            if seen.contains(needle) {
                return seen;
            }
        }
    }

    #[tokio::test]
    async fn only_the_operator_gets_in() {
        let env = env();
        let stranger = ed25519_dalek::SigningKey::from_bytes(&[8; 32]);
        assert_eq!(hello(&env, &stranger).await.status(), StatusCode::FORBIDDEN);
        assert_eq!(send(&env, "POST", &[], "hello {}\n").await.status(), StatusCode::BAD_REQUEST);
        assert_eq!(send(&env, "POST", &[("hyperb-key", "xyz".into())], "hello {}\n").await.status(), StatusCode::BAD_REQUEST);

        let operator = operator_secret();
        let res = hello(&env, &operator).await;
        assert_eq!(res.status(), StatusCode::OK);
        let session = session_of(&res);

        let sse = |headers: Vec<(&'static str, String)>| {
            let env = &env;
            async move {
                let mut h = headers;
                h.push(("accept", "text/event-stream".into()));
                send(env, "GET", &h, "").await.status()
            }
        };
        // knowing the session id and the key is not enough
        let wrong = signed(&stranger, &session);
        assert_eq!(sse(vec![("hyperb-key", lowercase_hex::encode(operator_secret().verifying_key().to_bytes())), ("hyperb-session", session.clone()), ("hyperb-signature", wrong[2].1.clone())]).await, StatusCode::UNAUTHORIZED);
        assert_eq!(sse(wrong).await, StatusCode::UNAUTHORIZED);
        assert_eq!(sse(vec![("hyperb-session", session.clone())]).await, StatusCode::UNAUTHORIZED);
        assert_eq!(sse(signed(&operator, "unknown")).await, StatusCode::UNAUTHORIZED);

        // lines can't be injected either
        let mut forged = signed(&stranger, &session);
        forged[0].1 = lowercase_hex::encode(operator.verifying_key().to_bytes());
        assert_eq!(send(&env, "POST", &forged, "action {\"name\":\"sweep\"}\n").await.status(), StatusCode::UNAUTHORIZED);

        // a page for people who wander in with a web browser
        assert_eq!(send(&env, "GET", &[], "").await.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn a_whole_session() {
        let env = env();
        let operator = operator_secret();
        let res = hello(&env, &operator).await;
        let session = session_of(&res);

        let mut h = signed(&operator, &session);
        h.push(("accept", "text/event-stream".into()));
        let res = send(&env, "GET", &h, "").await;
        assert_eq!(res.status(), StatusCode::OK);
        let mut stream = res.into_body();

        // the interface arrives
        let seen = read_until(&mut stream, "audit_run").await;
        assert!(seen.contains("event: element") && seen.contains("event: signal"), "{seen}");
        // a second stream for the same session is refused
        assert_eq!(send(&env, "GET", &h, "").await.status(), StatusCode::CONFLICT);

        // and answers to what the operator does
        let action = hyperb_action("audit", json!({ "audit": { "days": "7" } }), None);
        assert_eq!(send(&env, "POST", &signed(&operator, &session), &action).await.status(), StatusCode::NO_CONTENT);
        read_until(&mut stream, "The ledger adds up").await;

        drop(stream);
    }

    #[tokio::test]
    async fn the_first_operator_comes_with_the_setup_link() {
        let dir = std::env::temp_dir().join(format!("garden-setup-{:016x}", rand::random::<u64>()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("garden.toml");
        std::fs::write(&path, "# the comments stay\nlisten = \"127.0.0.1:3000\"\noperators = []\n").unwrap();

        let mut env = env();
        let mut config = env.app.config.clone();
        config.operators.clear();
        config.public_url = "https://cdn.example.com".into();
        config.path = Some(path.clone());
        env.app = App::new(
            config,
            Arc::new(OrangeLedger::in_memory().unwrap()),
            Providers::from_parts(vec![]),
            Arc::new(Switch::new(Some(Arc::new(MockPayments::default())))),
            Arc::new(Store::temporary().unwrap()),
        );
        env.router = crate::http::router(env.app.clone());

        // nobody gets in without the link
        let (first, second) = (operator_secret(), ed25519_dalek::SigningKey::from_bytes(&[8; 32]));
        assert_eq!(hello(&env, &first).await.status(), StatusCode::NOT_FOUND);
        let link = env.app.setup_link().unwrap();
        assert_eq!(env.app.setup_link().unwrap(), link, "it stays the same until it is used");
        let (address, token) = link.split_once("#setup=").unwrap();
        assert_eq!(address, "hyperb://cdn.example.com/operator");

        let key = |k: &ed25519_dalek::SigningKey| lowercase_hex::encode(k.verifying_key().to_bytes());
        let with = |token: &str| format!("hello {}\n", json!({ "params": { "setup": token } }));
        assert_eq!(send(&env, "POST", &[("hyperb-key", key(&first))], &with("nope")).await.status(), StatusCode::FORBIDDEN);

        // the first one to bring it is the operator, and is written down
        let res = send(&env, "POST", &[("hyperb-key", key(&first))], &with(token)).await;
        assert_eq!(res.status(), StatusCode::OK);
        let mut h = signed(&first, &session_of(&res));
        h.push(("accept", "text/event-stream".into()));
        let mut stream = send(&env, "GET", &h, "").await.into_body();
        read_until(&mut stream, "You are the operator of this server now.").await;
        let saved = std::fs::read_to_string(&path).unwrap();
        assert!(saved.starts_with("# the comments stay\n"), "{saved}");
        let saved: toml::Table = toml::from_str(&saved).unwrap();
        assert_eq!(saved["operators"].as_array().unwrap()[0].as_str().unwrap(), key(&first));

        // and it doesn't work again
        assert!(env.app.setup_link().is_none());
        assert_eq!(send(&env, "POST", &[("hyperb-key", key(&second))], &with(token)).await.status(), StatusCode::FORBIDDEN);
        assert_eq!(hello(&env, &first).await.status(), StatusCode::OK);
        drop(stream);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn setup_links_use_the_scheme_hyperb_picks() {
        let mut env = env();
        for (public, link) in [
            ("http://127.0.0.1:3000", "hyperb://127.0.0.1:3000/operator"),
            ("http://localhost:3000", "hyperb://localhost:3000/operator"),
            ("http://cdn.lan", "hyperb+http://cdn.lan/operator"),
            ("https://cdn.example.com:8443", "hyperb://cdn.example.com:8443/operator"),
        ] {
            let mut config = env.app.config.clone();
            config.operators.clear();
            config.public_url = public.into();
            env.app = App::new(
                config,
                Arc::new(OrangeLedger::in_memory().unwrap()),
                Providers::from_parts(vec![]),
                Arc::new(Switch::default()),
                Arc::new(Store::temporary().unwrap()),
            );
            assert!(env.app.setup_link().unwrap().starts_with(&format!("{link}#setup=")), "{public}");
        }
    }
}
