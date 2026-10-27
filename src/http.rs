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

use crate::app::{App, AppError, parse_hash};
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
        // everything the operator does happens here, in hyperb
        .merge(crate::operator::transport::routes())
        .layer(
            CorsLayer::new()
                .allow_origin(Any)
                .allow_methods(Any)
                .allow_headers(Any)
                .expose_headers([
                    header::LOCATION,
                    header::HeaderName::from_static("x-reason"),
                    header::HeaderName::from_static("hyperb-session"),
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
