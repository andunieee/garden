use std::collections::BTreeMap;
use std::sync::Arc;

use axum::Json;
use axum::Router;
use axum::extract::{DefaultBodyLimit, Path, State};
use axum::http::{HeaderMap, HeaderValue, StatusCode, Uri, header};
use axum::response::{Html, IntoResponse, Response};
use axum::routing::{get, post, put};
use bytes::Bytes;
use ritualistic::Timestamp;
use ritualistic::blossom::{self, Auth, Verb};
use serde::Deserialize;
use serde_json::json;
use tower_http::cors::{Any, CorsLayer};

use crate::app::{App, AppError, parse_hash, parse_pubkey};
use crate::store::{Order, OrderStatus};

type Shared = State<Arc<App>>;
type Result<T> = std::result::Result<T, AppError>;

/// the action authorization events for our own api (not part of blossom) are made for
const ACCOUNT: Verb = Verb::Custom("account");

impl IntoResponse for AppError {
    fn into_response(self) -> Response {
        let status = match self {
            AppError::BadRequest(_) => StatusCode::BAD_REQUEST,
            AppError::Unauthorized(_) => StatusCode::UNAUTHORIZED,
            AppError::PaymentRequired(_) => StatusCode::PAYMENT_REQUIRED,
            AppError::Forbidden(_) => StatusCode::FORBIDDEN,
            AppError::NotFound(_) => StatusCode::NOT_FOUND,
            AppError::TooLarge(_) => StatusCode::PAYLOAD_TOO_LARGE,
            AppError::UnsupportedType(_) => StatusCode::UNSUPPORTED_MEDIA_TYPE,
            AppError::Unavailable(_) => StatusCode::BAD_GATEWAY,
            AppError::Internal(_) => StatusCode::INTERNAL_SERVER_ERROR,
        };
        let reason = self.to_string();
        let mut res = (status, Json(json!({ "error": reason }))).into_response();
        // blossom clients read the reason from here
        if let Ok(v) = HeaderValue::from_str(&reason) {
            res.headers_mut().insert("x-reason", v);
        }
        res
    }
}

pub fn router(app: Arc<App>) -> Router {
    let max_upload = app.config.restrictions.max_file_size as usize;

    Router::new()
        .route("/", get(|| async { Html(include_str!("ui.html")) }))
        // blossom
        .route(
            "/upload",
            put(upload).head(upload_requirements).layer(DefaultBodyLimit::max(max_upload)),
        )
        .route("/{name}", get(blob))
        // our api
        .route("/api/info", get(info))
        .route("/api/account", get(account))
        .route("/api/account/credit", post(account_credit))
        .route("/api/topup", post(topup))
        .route("/api/orders/{id}", get(order))
        .route("/api/files/{hash}", get(file))
        // moderation
        .route("/admin/files/{hash}", get(admin_file))
        .route("/admin/ban", post(admin_ban))
        .route("/admin/bans", get(admin_bans))
        .route("/admin/sweep", post(admin_sweep))
        .route("/admin/orders/{id}/confirm", post(admin_confirm))
        .layer(
            CorsLayer::new()
                .allow_origin(Any)
                .allow_methods(Any)
                .allow_headers(Any)
                .expose_headers([
                    header::LOCATION,
                    header::HeaderName::from_static("x-reason"),
                ]),
        )
        .with_state(app)
}

fn header_str<'a>(headers: &'a HeaderMap, name: &str) -> Option<&'a str> {
    headers.get(name).and_then(|v| v.to_str().ok())
}

/// the verified authorization of a request
fn auth(app: &App, headers: &HeaderMap, verb: Verb) -> Result<Auth> {
    let value = header_str(headers, "authorization")
        .ok_or_else(|| AppError::Unauthorized("an Authorization header is required".into()))?;
    let auth = blossom::verify_auth_header(value, verb, Timestamp::now())
        .map_err(|e| AppError::Unauthorized(e.to_string()))?;

    if let Some(host) = url::Url::parse(&app.config.public_url).ok().and_then(|u| {
        u.host_str()
            .map(|h| u.port().map(|p| format!("{h}:{p}")).unwrap_or_else(|| h.to_string()))
    }) && !auth.is_for_server(&host)
    {
        return Err(AppError::Unauthorized(
            blossom::AuthError::WrongServer.to_string(),
        ));
    }
    Ok(auth)
}

fn pubkey_of(auth: &Auth) -> [u8; 32] {
    auth.pubkey().0
}

fn is_admin(app: &App, headers: &HeaderMap) -> Result<()> {
    let token = &app.config.admin_token;
    if token.is_empty() {
        return Err(AppError::NotFound("the admin api is disabled".into()));
    }
    let given = header_str(headers, "authorization")
        .and_then(|v| v.strip_prefix("Bearer "))
        .unwrap_or("");

    // compare without stopping at the first difference
    let same = given.len() == token.len()
        && given.bytes().zip(token.bytes()).fold(0u8, |acc, (a, b)| acc | (a ^ b)) == 0;
    if same {
        Ok(())
    } else {
        Err(AppError::Unauthorized("wrong admin token".into()))
    }
}

// blossom

async fn blob(State(app): Shared, uri: Uri) -> Result<Response> {
    let hash = blossom::hash_from_path(uri.path())
        .and_then(parse_hash)
        .ok_or_else(|| AppError::NotFound("not found".into()))?;

    match app.locate(&hash).await? {
        Some(url) => Ok((
            StatusCode::FOUND,
            [
                (header::LOCATION, url),
                // the target expires, so we can't be cached for long
                (header::CACHE_CONTROL, "public, max-age=60".to_string()),
            ],
        )
            .into_response()),
        None => Err(AppError::NotFound("not found".into())),
    }
}

async fn upload(State(app): Shared, headers: HeaderMap, body: Bytes) -> Result<Response> {
    let auth = auth(&app, &headers, Verb::Upload)?;
    let mime = header_str(&headers, "content-type").unwrap_or("");

    let descriptor = app
        .upload(&pubkey_of(&auth), mime, body, |hash| auth.covers(hash))
        .await?;
    Ok(Json(descriptor).into_response())
}

/// BUD-06: tell whether an upload would be accepted before the bytes are sent
async fn upload_requirements(State(app): Shared, headers: HeaderMap) -> Result<Response> {
    let size: u64 = header_str(&headers, "x-content-length")
        .and_then(|v| v.parse().ok())
        .ok_or_else(|| AppError::BadRequest("X-Content-Length is required".into()))?;
    let mime = header_str(&headers, "x-content-type").unwrap_or("application/octet-stream");
    if let Some(hash) = header_str(&headers, "x-sha256")
        && app.store.is_hash_banned(&hash.to_lowercase())?
    {
        return Err(AppError::Forbidden("this file is banned".into()));
    }

    // we don't know who is asking, so only the rules that apply to everybody
    app.config.restrictions.check_file(size, mime)?;
    Ok(StatusCode::OK.into_response())
}

// our api

async fn info(State(app): Shared) -> Json<serde_json::Value> {
    let c = &app.config;
    Json(json!({
        "url": c.public_url,
        "restrictions": {
            "max_file_size": c.restrictions.max_file_size,
            "allowed_mime_types": c.restrictions.allowed_mime_types,
            "min_account_payment_sats": c.restrictions.min_account_payment_sats,
            "min_order_sats": c.restrictions.min_order_sats,
            "max_files_per_order": c.restrictions.max_files_per_order,
            "restricted_pubkeys": !c.restrictions.allowed_pubkeys.is_empty(),
        },
        "pricing": {
            "base_sats_per_day": c.pricing.base_sats_per_day,
            "sats_per_mib_per_day": c.pricing.sats_per_mib_per_day,
            "min_days": c.pricing.min_days,
        },
    }))
}

async fn account(State(app): Shared, headers: HeaderMap) -> Result<Response> {
    let auth = auth(&app, &headers, ACCOUNT)?;
    Ok(Json(app.account(&pubkey_of(&auth)).await?).into_response())
}

#[derive(Deserialize)]
struct CreditRequest {
    amount_sats: u64,
}

async fn account_credit(
    State(app): Shared,
    headers: HeaderMap,
    Json(req): Json<CreditRequest>,
) -> Result<Response> {
    let auth = auth(&app, &headers, ACCOUNT)?;
    let order = app.account_order(&pubkey_of(&auth), req.amount_sats).await?;
    Ok((StatusCode::CREATED, Json(order)).into_response())
}

#[derive(Deserialize)]
struct TopupRequest {
    /// sha256 -> sats
    files: BTreeMap<String, u64>,
}

/// no account, no authorization: anyone can pay for any file
async fn topup(State(app): Shared, Json(req): Json<TopupRequest>) -> Result<Response> {
    let order = app.files_order(req.files).await?;
    Ok((StatusCode::CREATED, Json(order)).into_response())
}

async fn order(State(app): Shared, Path(id): Path<String>) -> Result<Json<Order>> {
    let order = (id.len() == 32)
        .then(|| app.store.order(&id))
        .transpose()?
        .flatten()
        .ok_or_else(|| AppError::NotFound("no such order".into()))?;

    if order.status == OrderStatus::Pending {
        // no need to wait for the next round of checks
        return Ok(Json(app.check_order(order).await?));
    }
    Ok(Json(order))
}

async fn file(State(app): Shared, Path(hash): Path<String>) -> Result<Response> {
    let hash = parse_hash(&hash.to_lowercase()).ok_or_else(|| AppError::BadRequest("not a sha256".into()))?;
    Ok(Json(app.file_view(&hash).await?).into_response())
}

// moderation

async fn admin_file(State(app): Shared, headers: HeaderMap, Path(hash): Path<String>) -> Result<Response> {
    is_admin(&app, &headers)?;
    let hash = parse_hash(&hash.to_lowercase()).ok_or_else(|| AppError::BadRequest("not a sha256".into()))?;
    Ok(Json(json!({
        "file": app.file_view(&hash).await.ok(),
        "uploaders": app.uploaders(&hash).await?,
        "banned": app.store.is_hash_banned(&lowercase_hex::encode(hash))?,
    }))
    .into_response())
}

#[derive(Deserialize)]
struct BanRequest {
    pubkey: Option<String>,
    sha256: Option<String>,
    /// with a pubkey: also remove everything it uploaded
    #[serde(default)]
    purge: bool,
}

async fn admin_ban(State(app): Shared, headers: HeaderMap, Json(req): Json<BanRequest>) -> Result<Response> {
    is_admin(&app, &headers)?;
    let mut purged = vec![];
    if let Some(pk) = &req.pubkey {
        let pk = parse_pubkey(pk).ok_or_else(|| AppError::BadRequest("not a pubkey in hex".into()))?;
        purged = app.ban_pubkey(&pk, req.purge).await?;
    }
    if let Some(h) = &req.sha256 {
        let h = parse_hash(&h.to_lowercase()).ok_or_else(|| AppError::BadRequest("not a sha256".into()))?;
        app.purge_file(&h).await?;
        purged.push(lowercase_hex::encode(h));
    }
    if req.pubkey.is_none() && req.sha256.is_none() {
        return Err(AppError::BadRequest("give a pubkey or a sha256".into()));
    }
    Ok(Json(json!({ "purged": purged })).into_response())
}

async fn admin_bans(State(app): Shared, headers: HeaderMap) -> Result<Response> {
    is_admin(&app, &headers)?;
    Ok(Json(json!({
        "pubkeys": app.store.banned_pubkeys()?,
        "hashes": app.store.banned_hashes()?,
    }))
    .into_response())
}

/// run the daily charge now. harmless to repeat, each file pays once per day.
async fn admin_sweep(State(app): Shared, headers: HeaderMap) -> Result<Response> {
    is_admin(&app, &headers)?;
    Ok(Json(app.sweep(crate::app::now() / 86400).await?).into_response())
}

/// with mock payments only: pretend the order was paid
async fn admin_confirm(State(app): Shared, headers: HeaderMap, Path(id): Path<String>) -> Result<Response> {
    is_admin(&app, &headers)?;
    let order = (id.len() == 32)
        .then(|| app.store.order(&id))
        .transpose()?
        .flatten()
        .ok_or_else(|| AppError::NotFound("no such order".into()))?;
    if !app.payments.simulate(&order.payment, order.amount_sats) {
        return Err(AppError::BadRequest("payments are real, they can't be confirmed by hand".into()));
    }
    Ok(Json(app.check_order(order).await?).into_response())
}
