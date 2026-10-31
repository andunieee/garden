//! the hyperb http session (see `SPEC.md` of hyperb, section 2):
//!
//! 1. `POST /operator` with the `hello` line as the body and the renderer's public key in
//!    `hyperb-key`. we answer 200 with a session id in `hyperb-session`.
//! 2. `GET /operator` with `Accept: text/event-stream`, `hyperb-key`, `hyperb-session` and
//!    `hyperb-signature`, the renderer's ed25519 signature of the session id. everything we say
//!    goes down this stream, one event per line, named by its tag.
//! 3. every other line from the renderer is a `POST` with the same three headers.
//!
//! only the keys listed in the `operators` setting get a session, and each request has to be
//! signed by the key that opened it.
//!
//! while there is no operator, the server logs a setup link with a secret in its fragment,
//! which hyperb sends in the `hello` (`{"params":{"setup":"..."}}`). the key that brings it
//! first becomes the operator and is added to the config file. this is how a headless server
//! is taken over the first time.

use std::collections::HashMap;
use std::convert::Infallible;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::Router;
use axum::extract::State;
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::sse::{self, KeepAlive, Sse};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use ed25519_dalek::{Signature, Verifier, VerifyingKey};
use futures_util::stream::{self, Stream, StreamExt};
use tokio::sync::mpsc;

use super::OperatorSession;
use crate::app::App;

pub const KEY: &str = "hyperb-key";
pub const SESSION: &str = "hyperb-session";
pub const SIGNATURE: &str = "hyperb-signature";

/// how long a session waits for its event stream to be opened
const STREAM_TIMEOUT: Duration = Duration::from_secs(30);
/// there is one operator, or a few. this is only to stop a flood.
const MAX_SESSIONS: usize = 16;

#[derive(Default)]
pub struct Sessions(Mutex<HashMap<String, Session>>);

struct Session {
    key: String,
    cmds: mpsc::UnboundedSender<Cmd>,
    streaming: bool,
}

enum Cmd {
    /// lines from the renderer
    Input(String),
    /// the event stream opened: send everything there
    Stream(mpsc::UnboundedSender<String>),
}

pub fn routes() -> Router<Arc<App>> {
    Router::new().route("/operator", get(stream).post(post))
}

fn header_str<'a>(headers: &'a HeaderMap, name: &str) -> Option<&'a str> {
    headers.get(name)?.to_str().ok()
}

fn key_bytes(hex: &str) -> Option<[u8; 32]> {
    lowercase_hex::decode(hex).ok()?.try_into().ok()
}

/// is `signature` (hex) the signature of `message` by `key` (hex)?
pub fn verify(key: &str, message: &[u8], signature: &str) -> bool {
    let Some(key) = key_bytes(key).and_then(|k| VerifyingKey::from_bytes(&k).ok()) else {
        return false;
    };
    let Some(sig) = lowercase_hex::decode(signature).ok().and_then(|s| <[u8; 64]>::try_from(s).ok()) else {
        return false;
    };
    key.verify(message, &Signature::from_bytes(&sig)).is_ok()
}

/// the secret of the setup link, if the hello brings one
fn setup_param(hello: &str) -> Option<String> {
    match hypereffect::RendererMessage::parse(hello.lines().next()?) {
        Ok(hypereffect::RendererMessage::Hello { mut params }) => params.remove("setup"),
        _ => None,
    }
}

/// a hello, or a line for a session
async fn post(State(app): State<Arc<App>>, headers: HeaderMap, body: String) -> Response {
    if headers.contains_key(SESSION) {
        let map = app.sessions.0.lock().unwrap();
        return match authorize(&map, &headers) {
            Ok(s) if s.cmds.send(Cmd::Input(body)).is_ok() => StatusCode::NO_CONTENT.into_response(),
            Ok(_) => (StatusCode::GONE, "session ended").into_response(),
            Err(r) => r.into_response(),
        };
    }

    let Some(key) = header_str(&headers, KEY).map(str::to_lowercase).filter(|k| key_bytes(k).is_some()) else {
        return (StatusCode::BAD_REQUEST, format!("missing or invalid {KEY}")).into_response();
    };
    let mut welcome = None;
    if !app.is_operator(&key) {
        match setup_param(&body) {
            Some(token) if !app.has_operators() => match app.claim_operator(&key, &token) {
                Ok(true) => welcome = Some(("success", "You are the operator of this server now.".to_string())),
                Ok(false) => {
                    return (StatusCode::FORBIDDEN, "this setup link is not valid: it changes every time the server starts")
                        .into_response();
                }
                Err(e) => {
                    log::error!("{e}");
                    welcome = Some((
                        "warning",
                        format!("You are the operator until the server restarts, but {e}. Add your key to `operators` by hand."),
                    ));
                }
            },
            _ if !app.has_operators() => {
                return (StatusCode::NOT_FOUND, "this server has no operator yet: open the setup link from its log").into_response();
            }
            _ => {
                log::warn!("the key {key} tried to open the operator interface, add it to `operators` if it's yours");
                return (StatusCode::FORBIDDEN, "this key is not an operator").into_response();
            }
        }
    }

    let id = lowercase_hex::encode(rand::random::<[u8; 32]>());
    let (cmds, rx) = mpsc::unbounded_channel();
    {
        let mut map = app.sessions.0.lock().unwrap();
        if map.len() >= MAX_SESSIONS {
            return (StatusCode::SERVICE_UNAVAILABLE, "too many sessions").into_response();
        }
        map.insert(
            id.clone(),
            Session {
                key: key.clone(),
                cmds,
                streaming: false,
            },
        );
    }
    log::info!("operator session for {key}");

    let (task_app, sid) = (app.clone(), id.clone());
    tokio::spawn(async move {
        run(task_app.clone(), body, rx, welcome).await;
        task_app.sessions.0.lock().unwrap().remove(&sid);
    });
    ([(SESSION, id)], StatusCode::OK).into_response()
}

/// opens the event stream of a session
async fn stream(State(app): State<Arc<App>>, headers: HeaderMap) -> Response {
    let wants_events = header_str(&headers, header::ACCEPT.as_str()).is_some_and(|a| a.contains("text/event-stream"));
    if !wants_events {
        return (
            StatusCode::OK,
            "This is the operator interface of a garden server. Open it with hyperb.",
        )
            .into_response();
    }
    let mut map = app.sessions.0.lock().unwrap();
    let id = header_str(&headers, SESSION).unwrap_or_default().to_string();
    let session = match authorize(&map, &headers) {
        Ok(_) => map.get_mut(&id).expect("authorize found it"),
        Err(r) => return r.into_response(),
    };
    if session.streaming {
        return (StatusCode::CONFLICT, "the session already has a stream").into_response();
    }
    let (tx, rx) = mpsc::unbounded_channel();
    if session.cmds.send(Cmd::Stream(tx)).is_err() {
        return (StatusCode::GONE, "session ended").into_response();
    }
    session.streaming = true;
    Sse::new(events(rx)).keep_alive(KeepAlive::default()).into_response()
}

/// one SSE event per line, named by its tag
fn events(rx: mpsc::UnboundedReceiver<String>) -> impl Stream<Item = Result<sse::Event, Infallible>> {
    stream::unfold(rx, |mut rx| async move { Some((rx.recv().await?, rx)) }).flat_map(|chunk| {
        let events: Vec<_> = chunk
            .lines()
            .filter_map(|l| l.split_once(' ').filter(|(tag, _)| !tag.is_empty()))
            .map(|(tag, json)| Ok(sse::Event::default().event(tag).data(json)))
            .collect();
        stream::iter(events)
    })
}

/// the session the headers name, if they are signed by its key
fn authorize<'a>(map: &'a HashMap<String, Session>, headers: &HeaderMap) -> Result<&'a Session, (StatusCode, &'static str)> {
    let deny = |why| Err((StatusCode::UNAUTHORIZED, why));
    let id = header_str(headers, SESSION).unwrap_or_default();
    let Some(s) = map.get(id) else {
        return deny("unknown session");
    };
    let key = header_str(headers, KEY).map(str::to_lowercase);
    if key.as_deref() != Some(s.key.as_str()) {
        return deny("the session belongs to another key");
    }
    if !verify(&s.key, id.as_bytes(), header_str(headers, SIGNATURE).unwrap_or_default()) {
        return deny("bad signature");
    }
    Ok(s)
}

/// runs the interface of a session until its stream closes
async fn run(app: Arc<App>, hello: String, mut cmds: mpsc::UnboundedReceiver<Cmd>, welcome: Option<(&str, String)>) {
    let mut session = OperatorSession::new(app);
    // what was said before the stream opened
    let mut backlog = session.handle(&hello).await;
    if let Some((kind, text)) = welcome {
        backlog.push_str(&OperatorSession::notice(kind, &text));
    }
    let mut out: Option<mpsc::UnboundedSender<String>> = None;
    let deadline = tokio::time::sleep(STREAM_TIMEOUT);
    tokio::pin!(deadline);

    loop {
        if let Some(tx) = &out
            && !backlog.is_empty()
            && tx.send(std::mem::take(&mut backlog)).is_err()
        {
            break;
        }
        tokio::select! {
            cmd = cmds.recv() => match cmd {
                Some(Cmd::Input(lines)) => backlog.push_str(&session.handle(&lines).await),
                Some(Cmd::Stream(tx)) => out = Some(tx),
                None => break,
            },
            _ = &mut deadline, if out.is_none() => break,
            _ = async { out.as_ref().unwrap().closed().await }, if out.is_some() => break,
        }
    }
}
