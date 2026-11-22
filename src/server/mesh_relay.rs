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
    http::{header, HeaderMap, HeaderName, Request, Response, StatusCode},
    response::IntoResponse,
    routing::any,
    Router,
};
use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine};
use ed25519_dalek::Signature;
use futures_util::{SinkExt, StreamExt, TryStreamExt};
use hyper_util::rt::TokioIo;
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
    let own = super::mesh::own_addresses(&state.config);
    let (device_key, urls) = mesh.relay_route(device_id, target, &own)?;
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
    // No address may still mean an uplink; `route` decides.
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
    let query = request
        .uri()
        .query()
        .map(|query| format!("?{query}"))
        .unwrap_or_default();
    let (mut parts, body) = request.into_parts();
    let upgrade = if rest == "v2/ws" {
        Some(
            WebSocketUpgrade::from_request_parts(&mut parts, &state)
                .await
                .map_err(|_| AppError::InvalidRequest("expected a WebSocket upgrade".to_owned()))?,
        )
    } else {
        None
    };
    let upstream = route(&target, &urls).await?;
    if let Some(upgrade) = upgrade {
        return match upstream {
            Upstream::Direct(base) => {
                let url = format!("{}/v2/ws{query}", base.replacen("http", "ws", 1));
                let request = url
                    .into_client_request()
                    .map_err(|_| AppError::InvalidRequest("bad relay target".to_owned()))?;
                match tokio_tungstenite::connect_async(request).await {
                    Ok((socket, _)) => Ok(bridge(upgrade, socket, permit)),
                    Err(error) => refused(error),
                }
            }
            Upstream::Uplink(stream) => {
                let url = format!("ws://{}/v2/ws{query}", super::mesh_uplink::UPLINK_HOST);
                let request = url
                    .into_client_request()
                    .map_err(|_| AppError::InvalidRequest("bad relay target".to_owned()))?;
                match tokio_tungstenite::client_async(request, TokioIo::new(stream)).await {
                    Ok((socket, _)) => Ok(bridge(upgrade, socket, permit)),
                    Err(error) => refused(error),
                }
            }
        };
    }
    match upstream {
        Upstream::Direct(base) => {
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
            copy_headers(response.headers(), builder.headers_mut().expect("builder"));
            let stream = response.bytes_stream().map_err(std::io::Error::other);
            Ok(builder
                .body(Body::from_stream(holding(stream, permit)))
                .expect("relay response"))
        }
        Upstream::Uplink(stream) => {
            let unreachable =
                |error: hyper::Error| AppError::RemoteUnreachable(format!("relay target: {error}"));
            let (mut sender, connection) = hyper::client::conn::http1::handshake(stream)
                .await
                .map_err(unreachable)?;
            tokio::spawn(async move {
                if let Err(error) = connection.await {
                    tracing::debug!(error = %error, "relayed uplink connection ended");
                }
            });
            let mut forwarded = Request::builder()
                .method(parts.method)
                .uri(format!("/{rest}{query}"))
                .header(header::HOST, super::mesh_uplink::UPLINK_HOST);
            for (name, value) in parts.headers.iter() {
                if kept(name) {
                    forwarded = forwarded.header(name, value);
                }
            }
            let forwarded = forwarded
                .body(body)
                .map_err(|_| AppError::InvalidRequest("bad relayed request".to_owned()))?;
            let response = sender.send_request(forwarded).await.map_err(unreachable)?;
            let mut builder = Response::builder().status(response.status());
            copy_headers(response.headers(), builder.headers_mut().expect("builder"));
            let stream = Body::new(response.into_body())
                .into_data_stream()
                .map_err(std::io::Error::other);
            Ok(builder
                .body(Body::from_stream(holding(stream, permit)))
                .expect("relay response"))
        }
    }
}

/// Where a relayed request goes.
enum Upstream {
    /// A direct address of the target.
    Direct(String),
    /// A byte stream over the target's uplink to this node.
    Uplink(hyper::upgrade::Upgraded),
}

/// A direct address that answered in the last minute, else the target's
/// uplink when it holds one, else the first direct address that answers.
async fn route(target: &str, urls: &[String]) -> Result<Upstream, AppError> {
    if let Some(base) = cached_address(target, urls) {
        return Ok(Upstream::Direct(base));
    }
    match super::mesh_uplink::open(target).await {
        Some(Ok(stream)) => {
            tracing::info!(%target, "relaying via the target's uplink");
            return Ok(Upstream::Uplink(stream));
        }
        Some(Err(error)) => {
            tracing::info!(%target, error = %error, "the target's uplink failed; probing its addresses");
        }
        None => {}
    }
    probe_addresses(target, urls).await.map(Upstream::Direct)
}

/// Pipes the client's socket to `upstream` once it upgrades.
fn bridge<S>(
    upgrade: WebSocketUpgrade,
    upstream: tokio_tungstenite::WebSocketStream<S>,
    permit: tokio::sync::OwnedSemaphorePermit,
) -> Response<Body>
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send + 'static,
{
    upgrade
        .on_upgrade(move |socket| async move {
            let _permit = permit;
            pipe(socket, upstream).await;
        })
        .into_response()
}

/// The target refused the upgrade: hand the client its status.
fn refused(error: tungstenite::Error) -> Result<Response<Body>, AppError> {
    match error {
        tungstenite::Error::Http(response) => {
            let status =
                StatusCode::from_u16(response.status().as_u16()).unwrap_or(StatusCode::BAD_GATEWAY);
            let body = response.into_body().unwrap_or_default();
            Ok(Response::builder()
                .status(status)
                .header("content-type", "application/json")
                .body(Body::from(body))
                .expect("response"))
        }
        error => Err(AppError::RemoteUnreachable(format!(
            "relay target: {error}"
        ))),
    }
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

fn copy_headers(from: &HeaderMap, to: &mut HeaderMap) {
    for (name, value) in from.iter() {
        if kept(name) {
            to.append(name.clone(), value.clone());
        }
    }
}

/// The address of `target` that answered in the last minute, if it is
/// still one of `urls`.
fn cached_address(target: &str, urls: &[String]) -> Option<String> {
    let cached = relays().good.lock().ok().and_then(|good| {
        good.get(target)
            .filter(|(_, at)| at.elapsed() < GOOD_ADDRESS_TTL)
            .map(|(url, _)| url.clone())
    });
    cached
        .filter(|url| urls.contains(url))
        .map(|url| url.trim_end_matches('/').to_owned())
}

/// The first of `urls` where `target` answers `/v2/version`, remembered
/// for a minute. An address whose answer names another node (`nodeId`) is
/// skipped; one without `nodeId` (an older backend) counts when it answers
/// at all.
async fn probe_addresses(target: &str, urls: &[String]) -> Result<String, AppError> {
    if urls.is_empty() {
        return Err(AppError::RemoteUnreachable(
            "this node knows no address of that node".to_owned(),
        ));
    }
    for url in urls {
        let base = url.trim_end_matches('/').to_owned();
        if answers_as(target, &base).await {
            if let Ok(mut good) = relays().good.lock() {
                good.insert(target.to_owned(), (url.clone(), Instant::now()));
            }
            return Ok(base);
        }
    }
    Err(AppError::RemoteUnreachable(
        "no address of that node answers".to_owned(),
    ))
}

/// `base/v2/version` answers 2xx and does not name a node other than
/// `target`.
async fn answers_as(target: &str, base: &str) -> bool {
    let response = relays()
        .http
        .get(format!("{base}/v2/version"))
        .timeout(CONNECT_TIMEOUT)
        .send()
        .await;
    let Ok(response) = response else {
        return false;
    };
    if !response.status().is_success() {
        return false;
    }
    let body = response
        .json::<serde_json::Value>()
        .await
        .unwrap_or_default();
    match body.get("nodeId").and_then(serde_json::Value::as_str) {
        Some(node_id) if node_id != target => {
            tracing::debug!(%target, answered = %node_id, "relay address leads to another node");
            false
        }
        _ => true,
    }
}

/// Copies frames both ways until either side closes; the close code of
/// one side reaches the other.
async fn pipe<S>(
    client: axum::extract::ws::WebSocket,
    upstream: tokio_tungstenite::WebSocketStream<S>,
) where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
{
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

#[cfg(test)]
mod tests {
    use super::*;
    use axum::{routing::get, Json};

    /// A server answering `/v2/version` with `body`.
    async fn version_server(body: serde_json::Value) -> String {
        let app = Router::new().route(
            "/v2/version",
            get(move || {
                let body = body.clone();
                async move { Json(body) }
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        format!("http://{address}")
    }

    #[tokio::test]
    async fn addresses_that_answer_as_another_node_are_skipped() {
        let target = format!("node_{}", uuid::Uuid::new_v4().simple());
        let other = version_server(serde_json::json!({"mesh": 1, "nodeId": "node_other"})).await;
        let right = version_server(serde_json::json!({"mesh": 1, "nodeId": target})).await;
        let urls = vec![other.clone(), right.clone()];
        assert_eq!(probe_addresses(&target, &urls).await.unwrap(), right);
        assert_eq!(cached_address(&target, &urls), Some(right.clone()));
        // Only the wrong node answers: unreachable, nothing cached.
        let lonely = format!("node_{}", uuid::Uuid::new_v4().simple());
        assert!(matches!(
            probe_addresses(&lonely, &[other]).await,
            Err(AppError::RemoteUnreachable(_))
        ));
        // An older backend without `nodeId` still counts.
        let older = version_server(serde_json::json!({"name": "todex-agentd"})).await;
        let quiet = format!("node_{}", uuid::Uuid::new_v4().simple());
        assert_eq!(
            probe_addresses(&quiet, std::slice::from_ref(&older))
                .await
                .unwrap(),
            older
        );
        assert!(cached_address(&lonely, &[older]).is_none());
    }
}
