//! Relaying through a group node (docs/mesh-v1.md "Relay"): a device that
//! cannot reach node T directly uses node R's
//! `/v2/mesh/relay/{T}/{ticket}/{path}` as T's address. R forwards only
//! T's public routes, the sealed REST tunnel and the `tv=2` WebSocket, byte
//! for byte: everything stays end-to-end encrypted for T's pinned transport
//! key and signed by the device, so R sees ciphertext, both peers' ids and
//! traffic sizes, never content.

use std::{
    collections::HashMap,
    sync::{Arc, Mutex, OnceLock},
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use axum::{
    body::Body,
    extract::{ws::Message as AxumMessage, FromRequestParts, Path, State, WebSocketUpgrade},
    http::{HeaderMap, HeaderName, Request, Response, StatusCode},
    response::IntoResponse,
    routing::any,
    Router,
};
use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine};
use ed25519_dalek::Signature;
use futures_util::{SinkExt, StreamExt, TryStreamExt};
use tokio::sync::Semaphore;
use tokio_tungstenite::tungstenite::{self, client::IntoClientRequest};

use crate::{app_state::AppState, error::AppError, mesh::roster::parse_signing_public_key};

/// Path prefix of relayed requests; also a direct route for remote peers.
pub(crate) const RELAY_PREFIX: &str = "/v2/mesh/relay/";
const TICKET_LABEL: &[u8] = b"todex.mesh.v1/relay-ticket\0";
/// Longest ticket lifetime a node accepts.
const MAX_TICKET_LIFETIME: Duration = Duration::from_secs(7 * 24 * 60 * 60);
/// Requests and sockets relayed at once by this node.
const MAX_RELAYS: usize = 64;
const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);
/// How long a target address that answered is preferred without a probe.
const GOOD_ADDRESS_TTL: Duration = Duration::from_secs(60);
/// The target routes a relay forwards.
const FORWARDED: [&str; 5] = [
    "health",
    "v2/version",
    "v2/transport-policy",
    "v2/sealed",
    "v2/ws",
];
/// Request and response headers that cross the relay.
const KEPT_HEADERS: [&str; 5] = [
    "content-type",
    "accept",
    "retry-after",
    "cache-control",
    "date",
];

pub(super) fn routes() -> Router<AppState> {
    Router::new().route("/v2/mesh/relay/{target}/{ticket}/{*rest}", any(relay))
}

struct Relays {
    permits: Arc<Semaphore>,
    /// Target node → the address that answered last, and when.
    good: Mutex<HashMap<String, (String, Instant)>>,
    http: reqwest::Client,
}

fn relays() -> &'static Relays {
    static RELAYS: OnceLock<Relays> = OnceLock::new();
    RELAYS.get_or_init(|| Relays {
        permits: Arc::new(Semaphore::new(MAX_RELAYS)),
        good: Mutex::new(HashMap::new()),
        http: reqwest::Client::builder()
            .connect_timeout(CONNECT_TIMEOUT)
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .expect("relay HTTP client"),
    })
}

/// `deviceId.expiresAtMs.b64u(sig)`, the signature by the device over
/// `"todex.mesh.v1/relay-ticket\0" || deviceId || "\0" || relayNodeId ||
/// "\0" || targetNodeId || "\0" || expiresAt (decimal)`.
pub(crate) fn ticket_message(
    device_id: &str,
    relay: &str,
    target: &str,
    expires_at: u64,
) -> Vec<u8> {
    [
        TICKET_LABEL,
        device_id.as_bytes(),
        b"\0",
        relay.as_bytes(),
        b"\0",
        target.as_bytes(),
        b"\0",
        expires_at.to_string().as_bytes(),
    ]
    .concat()
}

/// Checks a ticket and returns the target's addresses.
fn authorize(state: &AppState, target: &str, ticket: &str) -> Result<Vec<String>, AppError> {
    let mesh = state
        .mesh
        .as_ref()
        .ok_or_else(|| AppError::Unsupported("device groups need device auth".to_owned()))?;
    let mut parts = ticket.splitn(3, '.');
    let (Some(device_id), Some(expires), Some(signature)) =
        (parts.next(), parts.next(), parts.next())
    else {
        return Err(AppError::Unauthenticated);
    };
    let expires_at: u64 = expires.parse().map_err(|_| AppError::Unauthenticated)?;
    let now = unix_ms();
    if expires_at <= now || expires_at > now + MAX_TICKET_LIFETIME.as_millis() as u64 {
        return Err(AppError::Unauthenticated);
    }
    let (device_key, urls) = mesh.relay_route(device_id, target)?;
    let key = parse_signing_public_key(&device_key).map_err(|_| AppError::Unauthenticated)?;
    let signature = URL_SAFE_NO_PAD
        .decode(signature)
        .ok()
        .and_then(|bytes| Signature::from_slice(&bytes).ok())
        .ok_or(AppError::Unauthenticated)?;
    key.verify_strict(
        &ticket_message(device_id, mesh.node_id(), target, expires_at),
        &signature,
    )
    .map_err(|_| AppError::Unauthenticated)?;
    if urls.is_empty() {
        return Err(AppError::RemoteUnreachable(
            "this node knows no address of that node".to_owned(),
        ));
    }
    Ok(urls)
}

async fn relay(
    State(state): State<AppState>,
    Path((target, ticket, rest)): Path<(String, String, String)>,
    request: Request<Body>,
) -> Result<Response<Body>, AppError> {
    if !FORWARDED.contains(&rest.as_str()) {
        return Err(AppError::NotFound(format!("relay route /{rest}")));
    }
    let urls = authorize(&state, &target, &ticket)?;
    let permit = relays()
        .permits
        .clone()
        .try_acquire_owned()
        .map_err(|_| AppError::TransportBusy)?;
    let base = reachable(&target, &urls).await?;
    let query = request
        .uri()
        .query()
        .map(|query| format!("?{query}"))
        .unwrap_or_default();
    if rest == "v2/ws" {
        let (mut parts, _) = request.into_parts();
        let upgrade = WebSocketUpgrade::from_request_parts(&mut parts, &state)
            .await
            .map_err(|_| AppError::InvalidRequest("expected a WebSocket upgrade".to_owned()))?;
        let url = format!("{}/v2/ws{query}", base.replacen("http", "ws", 1));
        let upstream = match tokio_tungstenite::connect_async(
            url.into_client_request()
                .map_err(|_| AppError::InvalidRequest("bad relay target".to_owned()))?,
        )
        .await
        {
            Ok((stream, _)) => stream,
            // The target refused the upgrade: hand the client its status.
            Err(tungstenite::Error::Http(response)) => {
                let status = StatusCode::from_u16(response.status().as_u16())
                    .unwrap_or(StatusCode::BAD_GATEWAY);
                let body = response.into_body().unwrap_or_default();
                return Ok(Response::builder()
                    .status(status)
                    .header("content-type", "application/json")
                    .body(Body::from(body))
                    .expect("response"));
            }
            Err(error) => {
                return Err(AppError::RemoteUnreachable(format!(
                    "relay target: {error}"
                )))
            }
        };
        return Ok(upgrade
            .on_upgrade(move |socket| async move {
                let _permit = permit;
                pipe(socket, upstream).await;
            })
            .into_response());
    }
    let (parts, body) = request.into_parts();
    let method = reqwest::Method::from_bytes(parts.method.as_str().as_bytes())
        .map_err(|_| AppError::InvalidRequest("bad method".to_owned()))?;
    let mut forwarded = relays()
        .http
        .request(method, format!("{base}/{rest}{query}"));
    for (name, value) in parts.headers.iter() {
        if kept(name) {
            forwarded = forwarded.header(name.as_str(), value.as_bytes());
        }
    }
    let stream = body.into_data_stream().map_err(std::io::Error::other);
    let response = forwarded
        .body(reqwest::Body::wrap_stream(stream))
        .send()
        .await
        .map_err(|error| AppError::RemoteUnreachable(format!("relay target: {error}")))?;
    let mut builder = Response::builder().status(response.status().as_u16());
    let headers = builder.headers_mut().expect("builder");
    copy_headers(response.headers(), headers);
    let stream = response.bytes_stream().map_err(std::io::Error::other);
    let body = Body::from_stream(holding(stream, permit));
    Ok(builder.body(body).expect("relay response"))
}

/// Keeps the relay permit until the response body is done.
fn holding<S>(
    stream: S,
    permit: tokio::sync::OwnedSemaphorePermit,
) -> impl futures_util::Stream<Item = Result<axum::body::Bytes, std::io::Error>>
where
    S: futures_util::Stream<Item = Result<axum::body::Bytes, std::io::Error>>,
{
    stream.map(move |item| {
        let _ = &permit;
        item
    })
}

fn kept(name: &HeaderName) -> bool {
    let name = name.as_str();
    KEPT_HEADERS.contains(&name) || name.starts_with("x-todex-")
}

fn copy_headers(from: &reqwest::header::HeaderMap, to: &mut HeaderMap) {
    for (name, value) in from.iter() {
        let Ok(name) = HeaderName::from_bytes(name.as_str().as_bytes()) else {
            continue;
        };
        if !kept(&name) {
            continue;
        }
        if let Ok(value) = axum::http::HeaderValue::from_bytes(value.as_bytes()) {
            to.append(name, value);
        }
    }
}

/// The first of `urls` that answers `/health`, preferring the last one
/// that did.
async fn reachable(target: &str, urls: &[String]) -> Result<String, AppError> {
    let relays = relays();
    let cached = relays.good.lock().ok().and_then(|good| {
        good.get(target)
            .filter(|(_, at)| at.elapsed() < GOOD_ADDRESS_TTL)
            .map(|(url, _)| url.clone())
    });
    if let Some(url) = cached.filter(|url| urls.contains(url)) {
        return Ok(url.trim_end_matches('/').to_owned());
    }
    for url in urls {
        let base = url.trim_end_matches('/').to_owned();
        let probe = relays
            .http
            .get(format!("{base}/health"))
            .timeout(CONNECT_TIMEOUT)
            .send()
            .await;
        if probe.is_ok_and(|response| response.status().is_success()) {
            if let Ok(mut good) = relays.good.lock() {
                good.insert(target.to_owned(), (url.clone(), Instant::now()));
            }
            return Ok(base);
        }
    }
    Err(AppError::RemoteUnreachable(
        "no address of that node answers".to_owned(),
    ))
}

/// Copies frames both ways until either side closes; the close code of
/// one side reaches the other.
async fn pipe(
    client: axum::extract::ws::WebSocket,
    upstream: tokio_tungstenite::WebSocketStream<
        tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>,
    >,
) {
    let (mut client_tx, mut client_rx) = client.split();
    let (mut upstream_tx, mut upstream_rx) = upstream.split();
    let up = async {
        while let Some(Ok(message)) = client_rx.next().await {
            let forwarded = match message {
                AxumMessage::Text(text) => tungstenite::Message::Text(text.as_str().into()),
                AxumMessage::Binary(bytes) => tungstenite::Message::Binary(bytes),
                AxumMessage::Ping(bytes) => tungstenite::Message::Ping(bytes),
                AxumMessage::Pong(bytes) => tungstenite::Message::Pong(bytes),
                AxumMessage::Close(frame) => {
                    let frame = frame.map(|frame| tungstenite::protocol::CloseFrame {
                        code: frame.code.into(),
                        reason: frame.reason.as_str().into(),
                    });
                    let _ = upstream_tx.send(tungstenite::Message::Close(frame)).await;
                    break;
                }
            };
            if upstream_tx.send(forwarded).await.is_err() {
                break;
            }
        }
    };
    let down = async {
        while let Some(Ok(message)) = upstream_rx.next().await {
            let forwarded = match message {
                tungstenite::Message::Text(text) => AxumMessage::Text(text.as_str().into()),
                tungstenite::Message::Binary(bytes) => AxumMessage::Binary(bytes),
                tungstenite::Message::Ping(bytes) => AxumMessage::Ping(bytes),
                tungstenite::Message::Pong(bytes) => AxumMessage::Pong(bytes),
                tungstenite::Message::Close(frame) => {
                    let frame = frame.map(|frame| axum::extract::ws::CloseFrame {
                        code: frame.code.into(),
                        reason: frame.reason.as_str().into(),
                    });
                    let _ = client_tx.send(AxumMessage::Close(frame)).await;
                    break;
                }
                tungstenite::Message::Frame(_) => continue,
            };
            if client_tx.send(forwarded).await.is_err() {
                break;
            }
        }
    };
    tokio::select! {
        _ = up => {}
        _ = down => {}
    }
}

fn unix_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis() as u64)
        .unwrap_or_default()
}
