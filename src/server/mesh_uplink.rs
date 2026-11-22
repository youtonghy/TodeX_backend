//! Uplinks (docs/mesh-v1.md "Uplink"): a node T that its relays cannot
//! dial, typically behind NAT, keeps a control WebSocket open to every relay
//! R of its groups. When R relays to T, it asks T over that socket to call
//! back; T's callback becomes a raw byte stream that R speaks HTTP/1.1 (or
//! the `tv=2` WebSocket) over, and T serves it like an inbound connection
//! limited to the routes a relay forwards. The bytes are what a direct
//! relay would carry: sealed for T's pinned key and signed by the device.

use std::{
    collections::HashMap,
    net::SocketAddr,
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc, Mutex, OnceLock,
    },
    time::{Duration, Instant},
};

use axum::{
    body::Body,
    extract::{
        ws::{Message as AxumMessage, WebSocket},
        ConnectInfo, Path, Query, Request as AxumRequest, State, WebSocketUpgrade,
    },
    http::{header, HeaderValue, Request, Response, StatusCode},
    middleware::Next,
    response::IntoResponse,
    routing::get,
    Router,
};
use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine};
use ed25519_dalek::Signature;
use futures_util::{SinkExt, StreamExt};
use hyper::upgrade::Upgraded;
use hyper_util::rt::{TokioIo, TokioTimer};
use rand_core::{OsRng, RngCore};
use serde::Deserialize;
use tokio::{
    io::{AsyncRead, AsyncWrite},
    sync::{mpsc, oneshot, watch},
    task::{AbortHandle, JoinHandle, JoinSet},
};
use tokio_tungstenite::tungstenite::{client::IntoClientRequest, Message};

use crate::{
    app_state::AppState,
    error::AppError,
    mesh::{roster::parse_signing_public_key, uplink_message, MeshStore},
    transport_crypto::{envelope::SEALED_PATH, query_value},
};

/// The control WebSocket; `UPLINK_PATH/{id}` is a callback.
pub(crate) const UPLINK_PATH: &str = "/v2/mesh/uplink";
/// The `Upgrade` token of a callback, answered with `101`.
const UPGRADE_TOKEN: &str = "todex-uplink";
/// `Host` of requests over an uplink: never loopback.
pub(super) const UPLINK_HOST: &str = "todex-uplink.invalid";
const MAX_CLOCK_SKEW: Duration = Duration::from_secs(5 * 60);
/// Nonces are remembered as long as a timestamp is accepted either way.
const NONCE_WINDOW: Duration = Duration::from_secs(10 * 60);
const MAX_NONCES: usize = 4096;
/// How long a relay waits for the node to call back.
const CALLBACK_TIMEOUT: Duration = Duration::from_secs(10);
const PING_INTERVAL: Duration = Duration::from_secs(30);
const MAX_MISSED_PONGS: u32 = 2;
/// A node drops a control socket that carried nothing (not even a ping)
/// for this long: the relay or the path to it is gone.
const SILENCE_TIMEOUT: Duration = Duration::from_secs(75);
const BACKOFF_MIN: Duration = Duration::from_secs(2);
const BACKOFF_MAX: Duration = Duration::from_secs(60);
/// How often a node re-reads its groups for the relays to keep uplinks to.
const ROSTER_POLL: Duration = Duration::from_secs(30);
const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);
/// Callbacks a relay may have pending, and a node serving, per uplink.
const MAX_CALLBACKS: usize = 64;
/// The routes a node serves over an uplink: those a relay forwards.
const SERVED: [&str; 4] = [
    "/health",
    "/v2/version",
    "/v2/transport-policy",
    SEALED_PATH,
];

pub(super) fn routes() -> Router<AppState> {
    Router::new()
        .route(UPLINK_PATH, get(control))
        .route("/v2/mesh/uplink/{id}", get(callback))
}

/// The signed query of both uplink requests.
#[derive(Debug, Deserialize)]
struct Auth {
    node: String,
    relay: String,
    ts: u64,
    nonce: String,
    sig: String,
}

/// A live control socket on this relay.
struct Link {
    generation: u64,
    /// Callback ids for the socket's task to send as `open`.
    opens: mpsc::Sender<String>,
}

struct Pending {
    node: String,
    stream: oneshot::Sender<Upgraded>,
}

#[derive(Default)]
struct Registry {
    /// Node id → its control socket; a newer one replaces the older.
    links: Mutex<HashMap<String, Link>>,
    /// Callback id → who must call back and who waits.
    pending: Mutex<HashMap<String, Pending>>,
    nonces: Mutex<HashMap<String, Instant>>,
    generation: AtomicU64,
}

fn registry() -> &'static Registry {
    static REGISTRY: OnceLock<Registry> = OnceLock::new();
    REGISTRY.get_or_init(Registry::default)
}

/// Checks an uplink request's signature and returns the calling node.
fn authorize(mesh: &MeshStore, auth: &Auth, id: Option<&str>, now_ms: u64) -> Result<(), AppError> {
    if auth.relay != mesh.node_id() {
        return Err(AppError::Unauthorized(
            "the uplink is meant for another relay".to_owned(),
        ));
    }
    if now_ms.abs_diff(auth.ts) > MAX_CLOCK_SKEW.as_millis() as u64 {
        return Err(AppError::Unauthenticated);
    }
    let key = mesh.uplink_key(&auth.node)?;
    let key = parse_signing_public_key(&key).map_err(|_| AppError::Unauthenticated)?;
    let signature = URL_SAFE_NO_PAD
        .decode(&auth.sig)
        .ok()
        .and_then(|bytes| Signature::from_slice(&bytes).ok())
        .ok_or(AppError::Unauthenticated)?;
    key.verify_strict(
        &uplink_message(&auth.node, &auth.relay, auth.ts, &auth.nonce, id),
        &signature,
    )
    .map_err(|_| AppError::Unauthenticated)?;
    // A callback is single-use already: its random id is signed and leaves
    // the pending table when used. Only control sockets need the nonce
    // cache, which would otherwise fill up with every relayed request.
    if id.is_some() {
        return Ok(());
    }
    let mut nonces = registry()
        .nonces
        .lock()
        .map_err(|_| AppError::TransportBusy)?;
    nonces.retain(|_, seen| seen.elapsed() < NONCE_WINDOW);
    if nonces.contains_key(&auth.nonce) {
        return Err(AppError::Unauthenticated);
    }
    if nonces.len() >= MAX_NONCES {
        return Err(AppError::TransportBusy);
    }
    nonces.insert(auth.nonce.clone(), Instant::now());
    Ok(())
}

/// `GET /v2/mesh/uplink`: a node's control socket.
async fn control(
    State(state): State<AppState>,
    Query(auth): Query<Auth>,
    upgrade: WebSocketUpgrade,
) -> Result<Response<Body>, AppError> {
    let mesh = state
        .mesh
        .as_ref()
        .ok_or_else(|| AppError::Unsupported("device groups need device auth".to_owned()))?;
    authorize(mesh, &auth, None, unix_ms())?;
    Ok(upgrade
        .on_upgrade(move |socket| hold_link(auth.node, socket))
        .into_response())
}

/// Registers the socket as `node`'s uplink and sends `open` for every
/// callback the relay needs, pinging every 30 s, until the node leaves,
/// misses two pongs or a newer socket replaces this one.
async fn hold_link(node: String, socket: WebSocket) {
    let registry = registry();
    let (opens_tx, mut opens) = mpsc::channel(MAX_CALLBACKS);
    let generation = registry.generation.fetch_add(1, Ordering::Relaxed) + 1;
    let replaced = registry.links.lock().ok().and_then(|mut links| {
        links.insert(
            node.clone(),
            Link {
                generation,
                opens: opens_tx,
            },
        )
    });
    if replaced.is_some() {
        tracing::info!(%node, "mesh uplink replaced by a newer connection");
    }
    tracing::info!(%node, "mesh uplink established");
    let (mut sender, mut receiver) = socket.split();
    let mut ping =
        tokio::time::interval_at(tokio::time::Instant::now() + PING_INTERVAL, PING_INTERVAL);
    let mut missed = 0;
    let reason = loop {
        tokio::select! {
            open = opens.recv() => {
                let Some(id) = open else {
                    break "replaced";
                };
                let frame = serde_json::json!({ "t": "open", "id": id }).to_string();
                if sender.send(AxumMessage::Text(frame.into())).await.is_err() {
                    break "send failed";
                }
            }
            _ = ping.tick() => {
                if missed >= MAX_MISSED_PONGS {
                    break "missed pongs";
                }
                missed += 1;
                if sender.send(AxumMessage::Ping(Default::default())).await.is_err() {
                    break "send failed";
                }
            }
            message = receiver.next() => match message {
                Some(Ok(AxumMessage::Close(_))) | Some(Err(_)) | None => break "closed by the node",
                Some(Ok(_)) => missed = 0,
            },
        }
    };
    if let Ok(mut links) = registry.links.lock() {
        if links
            .get(&node)
            .is_some_and(|link| link.generation == generation)
        {
            links.remove(&node);
        }
    }
    let _ = sender.close().await;
    tracing::info!(%node, reason, "mesh uplink closed");
}

/// A byte stream to `node` over its uplink: `None` without one, an error
/// when it did not call back within [`CALLBACK_TIMEOUT`].
pub(super) async fn open(node: &str) -> Option<Result<Upgraded, AppError>> {
    let registry = registry();
    let opens = registry
        .links
        .lock()
        .ok()?
        .get(node)
        .map(|link| link.opens.clone())?;
    let mut bytes = [0u8; 16];
    OsRng.fill_bytes(&mut bytes);
    let id = URL_SAFE_NO_PAD.encode(bytes);
    let (stream, waiting) = oneshot::channel();
    if let Ok(mut pending) = registry.pending.lock() {
        pending.insert(
            id.clone(),
            Pending {
                node: node.to_owned(),
                stream,
            },
        );
    }
    let forget = || {
        if let Ok(mut pending) = registry.pending.lock() {
            pending.remove(&id);
        }
    };
    if opens.try_send(id.clone()).is_err() {
        forget();
        return Some(Err(AppError::TransportBusy));
    }
    match tokio::time::timeout(CALLBACK_TIMEOUT, waiting).await {
        Ok(Ok(stream)) => Some(Ok(stream)),
        _ => {
            forget();
            Some(Err(AppError::RemoteUnreachable(
                "that node did not answer over its uplink".to_owned(),
            )))
        }
    }
}

/// `GET /v2/mesh/uplink/{id}` with `Upgrade: todex-uplink`: the node's
/// callback for `open {id}`, handed to the waiting relay once upgraded.
async fn callback(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Query(auth): Query<Auth>,
    mut request: Request<Body>,
) -> Result<Response<Body>, AppError> {
    let upgrade = request
        .headers()
        .get(header::UPGRADE)
        .and_then(|value| value.to_str().ok());
    if !upgrade.is_some_and(|value| value.eq_ignore_ascii_case(UPGRADE_TOKEN)) {
        return Err(AppError::InvalidRequest(format!(
            "expected Upgrade: {UPGRADE_TOKEN}"
        )));
    }
    let mesh = state
        .mesh
        .as_ref()
        .ok_or_else(|| AppError::Unsupported("device groups need device auth".to_owned()))?;
    authorize(mesh, &auth, Some(&id), unix_ms())?;
    let pending = registry().pending.lock().ok().and_then(|mut pending| {
        pending
            .get(&id)
            .is_some_and(|entry| entry.node == auth.node)
            .then(|| pending.remove(&id))
            .flatten()
    });
    let Some(pending) = pending else {
        return Err(AppError::NotFound("uplink callback".to_owned()));
    };
    let upgraded = hyper::upgrade::on(&mut request);
    tokio::spawn(async move {
        match upgraded.await {
            Ok(stream) => {
                let _ = pending.stream.send(stream);
            }
            Err(error) => tracing::debug!(error = %error, "uplink callback did not upgrade"),
        }
    });
    Ok(Response::builder()
        .status(StatusCode::SWITCHING_PROTOCOLS)
        .header(header::CONNECTION, "upgrade")
        .header(header::UPGRADE, UPGRADE_TOKEN)
        .body(Body::empty())
        .expect("101 response"))
}

/// Keeps an uplink to every relay [`MeshStore::uplink_relays`] lists,
/// re-reading the groups every 30 s and stopping uplinks to nodes that left.
/// `TODEX_AGENTD_MESH_UPLINK=false` turns it off.
pub(crate) fn spawn_uplinks(state: AppState) -> Option<JoinHandle<()>> {
    let mesh = state.mesh.clone()?;
    if !uplink_enabled() {
        tracing::info!("mesh uplinks are off (TODEX_AGENTD_MESH_UPLINK)");
        return None;
    }
    let app = served_app(super::router(state.clone()));
    Some(tokio::spawn(async move {
        // Dropping the set (this task aborted) stops every uplink.
        let mut tasks = JoinSet::new();
        let mut links: HashMap<String, (watch::Sender<Vec<String>>, AbortHandle)> = HashMap::new();
        let mut interval = tokio::time::interval(ROSTER_POLL);
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            interval.tick().await;
            while tasks.try_join_next().is_some() {}
            let own = super::mesh::own_addresses(&state.config);
            let wanted: HashMap<String, Vec<String>> = mesh
                .uplink_relays(&own)
                .into_iter()
                .filter(|(_, urls)| !urls.is_empty())
                .collect();
            links.retain(|relay, (urls, task)| match wanted.get(relay) {
                Some(wanted) => {
                    urls.send_if_modified(|urls| {
                        let changed = urls != wanted;
                        urls.clone_from(wanted);
                        changed
                    });
                    true
                }
                None => {
                    task.abort();
                    tracing::info!(%relay, "mesh uplink stopped: no longer a relay of this node");
                    false
                }
            });
            for (relay, urls) in wanted {
                if links.contains_key(&relay) {
                    continue;
                }
                let (sender, receiver) = watch::channel(urls);
                let task =
                    tasks.spawn(maintain(mesh.clone(), app.clone(), relay.clone(), receiver));
                links.insert(relay, (sender, task));
            }
        }
    }))
}

fn uplink_enabled() -> bool {
    !std::env::var("TODEX_AGENTD_MESH_UPLINK").is_ok_and(|value| {
        matches!(
            value.trim().to_ascii_lowercase().as_str(),
            "0" | "false" | "off" | "no"
        )
    })
}

/// One relay's uplink: connects to the first of its addresses that takes
/// it, serves callbacks while it lasts, and reconnects after 2 s, doubling
/// up to 60 s (with jitter) while no address takes it.
async fn maintain(mesh: MeshStore, app: Router, relay: String, urls: watch::Receiver<Vec<String>>) {
    let mut backoff = BACKOFF_MIN;
    let mut callbacks = JoinSet::new();
    loop {
        let candidates = urls.borrow().clone();
        let mut connected = false;
        for base in &candidates {
            match connect(&mesh, &relay, base).await {
                Ok(socket) => {
                    tracing::info!(%relay, address = %base, "mesh uplink established");
                    connected = true;
                    let reason =
                        serve_link(&mesh, &app, &relay, base, socket, &mut callbacks).await;
                    tracing::info!(%relay, reason, "mesh uplink closed");
                    break;
                }
                Err(error) => {
                    tracing::debug!(%relay, address = %base, error = %error, "mesh uplink did not connect");
                }
            }
        }
        if connected {
            backoff = BACKOFF_MIN;
        }
        tokio::time::sleep(jittered(backoff)).await;
        if !connected {
            backoff = (backoff * 2).min(BACKOFF_MAX);
        }
    }
}

/// `delay` ±20 %.
fn jittered(delay: Duration) -> Duration {
    let spread = delay.as_millis() as u64 / 5;
    let offset = OsRng.next_u64() % (2 * spread + 1);
    Duration::from_millis(delay.as_millis() as u64 - spread + offset)
}

type ControlSocket = tokio_tungstenite::WebSocketStream<Box<dyn Io>>;

/// Opens the control socket to `base`.
async fn connect(mesh: &MeshStore, relay: &str, base: &str) -> Result<ControlSocket, AppError> {
    let target = Target::parse(base)?;
    let (stream, _) = target.dial().await?;
    let url = format!(
        "{}://{}{}{UPLINK_PATH}?{}",
        if target.tls { "wss" } else { "ws" },
        target.authority,
        target.path,
        signed_query(mesh, relay, None)
    );
    let request = url
        .into_client_request()
        .map_err(|error| AppError::InvalidRequest(format!("uplink request: {error}")))?;
    let (socket, _) = tokio::time::timeout(
        CONNECT_TIMEOUT,
        tokio_tungstenite::client_async(request, stream),
    )
    .await
    .map_err(|_| AppError::RemoteUnreachable("uplink handshake timed out".to_owned()))?
    .map_err(|error| AppError::RemoteUnreachable(format!("uplink handshake: {error}")))?;
    Ok(socket)
}

/// Reads the control socket (pongs go out as it reads) and calls back for
/// every `open`, until it closes or stays silent for [`SILENCE_TIMEOUT`].
async fn serve_link(
    mesh: &MeshStore,
    app: &Router,
    relay: &str,
    base: &str,
    mut socket: ControlSocket,
    callbacks: &mut JoinSet<()>,
) -> &'static str {
    #[derive(Deserialize)]
    struct Open {
        t: String,
        id: String,
    }
    loop {
        while callbacks.try_join_next().is_some() {}
        let message = match tokio::time::timeout(SILENCE_TIMEOUT, socket.next()).await {
            Err(_) => return "the relay went silent",
            Ok(None) | Ok(Some(Err(_))) => return "connection lost",
            Ok(Some(Ok(message))) => message,
        };
        match message {
            Message::Close(_) => return "closed by the relay",
            Message::Text(text) => {
                let Ok(open) = serde_json::from_str::<Open>(text.as_str()) else {
                    continue;
                };
                if open.t != "open" || callbacks.len() >= MAX_CALLBACKS {
                    continue;
                }
                let (mesh, app, relay, base) =
                    (mesh.clone(), app.clone(), relay.to_owned(), base.to_owned());
                callbacks.spawn(async move {
                    if let Err(error) = call_back(&mesh, app, &relay, &base, &open.id).await {
                        tracing::debug!(%relay, error = %error, "uplink callback failed");
                    }
                });
            }
            _ => {}
        }
    }
}

/// Answers `open {id}`: dials the relay, upgrades to a raw stream and
/// serves it as one inbound connection.
async fn call_back(
    mesh: &MeshStore,
    app: Router,
    relay: &str,
    base: &str,
    id: &str,
) -> Result<(), AppError> {
    let target = Target::parse(base)?;
    let (stream, peer) = target.dial().await?;
    let unreachable = |error: hyper::Error| AppError::RemoteUnreachable(format!("uplink: {error}"));
    let (mut sender, connection) = hyper::client::conn::http1::handshake(TokioIo::new(stream))
        .await
        .map_err(unreachable)?;
    tokio::spawn(async move {
        if let Err(error) = connection.with_upgrades().await {
            tracing::debug!(error = %error, "uplink callback connection ended");
        }
    });
    let request = Request::get(format!(
        "{}{UPLINK_PATH}/{id}?{}",
        target.path,
        signed_query(mesh, relay, Some(id))
    ))
    .header(header::HOST, &target.authority)
    .header(header::CONNECTION, "upgrade")
    .header(header::UPGRADE, UPGRADE_TOKEN)
    .body(Body::empty())
    .map_err(|error| AppError::InvalidRequest(format!("uplink callback: {error}")))?;
    let response = tokio::time::timeout(CALLBACK_TIMEOUT, sender.send_request(request))
        .await
        .map_err(|_| AppError::RemoteUnreachable("uplink callback timed out".to_owned()))?
        .map_err(unreachable)?;
    if response.status() != StatusCode::SWITCHING_PROTOCOLS {
        return Err(AppError::RemoteUnreachable(format!(
            "uplink callback answered {}",
            response.status()
        )));
    }
    let stream = hyper::upgrade::on(response).await.map_err(unreachable)?;
    serve_stream(app, stream, peer).await;
    Ok(())
}

/// Serves `stream` as one inbound HTTP/1.1 connection from `peer`, as the
/// listeners do (`server_runner::serve_listener`). Its requests count as
/// remote: the peer address is never loopback and `Host` is
/// [`UPLINK_HOST`], whatever the relay sent.
async fn serve_stream<S>(app: Router, stream: S, peer: SocketAddr)
where
    S: hyper::rt::Read + hyper::rt::Write + Unpin + Send + 'static,
{
    use hyper_util::service::TowerToHyperService;
    use tower::ServiceExt;

    let peer = remote_peer(peer);
    let service = app.map_request(move |mut request: Request<hyper::body::Incoming>| {
        request.extensions_mut().insert(ConnectInfo(peer));
        request
            .headers_mut()
            .insert(header::HOST, HeaderValue::from_static(UPLINK_HOST));
        request
    });
    let mut builder = hyper::server::conn::http1::Builder::new();
    builder
        .timer(TokioTimer::new())
        .header_read_timeout(CALLBACK_TIMEOUT);
    if let Err(error) = builder
        .serve_connection(stream, TowerToHyperService::new(service))
        .with_upgrades()
        .await
    {
        tracing::debug!(error = %error, "uplink connection ended with an error");
    }
}

/// The relay's address as the peer of uplink requests; `0.0.0.0:0` when it
/// is loopback (a relay on this machine), which must not count as local.
fn remote_peer(peer: SocketAddr) -> SocketAddr {
    if peer.ip().to_canonical().is_loopback() {
        SocketAddr::from(([0, 0, 0, 0], 0))
    } else {
        peer
    }
}

/// The app limited to the routes a relay forwards; anything else is `404`.
fn served_app(app: Router) -> Router {
    app.layer(axum::middleware::from_fn(only_forwarded))
}

async fn only_forwarded(request: AxumRequest, next: Next) -> Response<Body> {
    let path = request.uri().path();
    let served = SERVED.contains(&path)
        || (path == "/v2/ws" && query_value(request.uri().query(), "tv").as_deref() == Some("2"));
    if !served {
        return AppError::NotFound(format!("uplink route {path}")).into_response();
    }
    next.run(request).await
}

/// `node`, `relay`, `ts`, `nonce` and `sig` for an uplink request.
fn signed_query(mesh: &MeshStore, relay: &str, id: Option<&str>) -> String {
    let mut bytes = [0u8; 16];
    OsRng.fill_bytes(&mut bytes);
    let nonce = URL_SAFE_NO_PAD.encode(bytes);
    let ts = unix_ms();
    let sig = mesh.sign_uplink(relay, ts, &nonce, id);
    format!(
        "node={}&relay={relay}&ts={ts}&nonce={nonce}&sig={sig}",
        mesh.node_id()
    )
}

trait Io: AsyncRead + AsyncWrite + Unpin + Send {}
impl<T: AsyncRead + AsyncWrite + Unpin + Send> Io for T {}

/// A relay address split for dialing.
struct Target {
    tls: bool,
    host: String,
    port: u16,
    /// `host[:port]` as `Host` carries it.
    authority: String,
    /// The address's path without a trailing `/`.
    path: String,
}

impl Target {
    fn parse(base: &str) -> Result<Self, AppError> {
        let invalid = || AppError::InvalidRequest(format!("bad relay address {base}"));
        let url = reqwest::Url::parse(base).map_err(|_| invalid())?;
        let tls = match url.scheme() {
            "http" => false,
            "https" => true,
            _ => return Err(invalid()),
        };
        let host = url.host_str().ok_or_else(invalid)?.to_owned();
        let port = url.port_or_known_default().ok_or_else(invalid)?;
        let authority = match url.port() {
            Some(port) => format!("{host}:{port}"),
            None => host.clone(),
        };
        Ok(Self {
            tls,
            host: host.trim_matches(['[', ']']).to_owned(),
            port,
            authority,
            path: url.path().trim_end_matches('/').to_owned(),
        })
    }

    /// TCP (and TLS for `https`) to the relay, and its peer address.
    async fn dial(&self) -> Result<(Box<dyn Io>, SocketAddr), AppError> {
        let unreachable = |error: std::io::Error| {
            AppError::RemoteUnreachable(format!("relay {}: {error}", self.authority))
        };
        let timed_out =
            |_| AppError::RemoteUnreachable(format!("relay {}: timed out", self.authority));
        let stream = tokio::time::timeout(
            CONNECT_TIMEOUT,
            tokio::net::TcpStream::connect((self.host.as_str(), self.port)),
        )
        .await
        .map_err(timed_out)?
        .map_err(unreachable)?;
        let _ = stream.set_nodelay(true);
        let peer = stream.peer_addr().map_err(unreachable)?;
        if !self.tls {
            return Ok((Box::new(stream), peer));
        }
        let name = tokio_rustls::rustls::pki_types::ServerName::try_from(self.host.clone())
            .map_err(|_| AppError::InvalidRequest(format!("bad relay host {}", self.host)))?;
        let stream = tokio::time::timeout(CONNECT_TIMEOUT, tls_connector().connect(name, stream))
            .await
            .map_err(timed_out)?
            .map_err(unreachable)?;
        Ok((Box::new(stream), peer))
    }
}

fn tls_connector() -> tokio_rustls::TlsConnector {
    use tokio_rustls::rustls::{self, ClientConfig, RootCertStore};
    static CONFIG: OnceLock<Arc<ClientConfig>> = OnceLock::new();
    let config = CONFIG.get_or_init(|| {
        let roots = RootCertStore::from_iter(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
        Arc::new(
            ClientConfig::builder_with_provider(Arc::new(rustls::crypto::ring::default_provider()))
                .with_safe_default_protocol_versions()
                .expect("ring supports the default TLS versions")
                .with_root_certificates(roots)
                .with_no_client_auth(),
        )
    });
    tokio_rustls::TlsConnector::from(Arc::clone(config))
}

fn unix_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_millis() as u64)
        .unwrap_or_default()
}

#[cfg(test)]
fn drop_link(node: &str) {
    if let Ok(mut links) = registry().links.lock() {
        links.remove(node);
    }
}

#[cfg(test)]
fn link_generation(node: &str) -> Option<u64> {
    registry()
        .links
        .lock()
        .ok()?
        .get(node)
        .map(|link| link.generation)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::PairingEncryption;
    use crate::device_auth::test_support::TestDevice;
    use crate::mesh::roster::{
        test_support::{append, genesis, node_member},
        verify_chain, Member, RelayRole,
    };
    use crate::server::transport_v2_tests::{
        open_response, ping, seal_request, signed_inner, ws_offer, TestServer,
    };
    use crate::transport_crypto::EncryptionProtocol;
    use ed25519_dalek::Signer;

    const DEVICE_SEED: u8 = 51;

    fn store() -> MeshStore {
        let root = std::env::temp_dir().join(format!("todex-uplink-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&root).unwrap();
        MeshStore::load(&root).unwrap()
    }

    fn node(mesh: &MeshStore, added_by: &str) -> Member {
        node_member(
            &parse_signing_public_key(&mesh.node_public_key()).unwrap(),
            added_by,
        )
    }

    /// Puts `relay` and `target` in one group created by the test device;
    /// `edit` adjusts their roster entries.
    fn group(relay: &MeshStore, target: &MeshStore, edit: impl FnOnce(&mut Member, &mut Member)) {
        let creator = ed25519_dalek::SigningKey::from_bytes(&[DEVICE_SEED; 32]);
        let chain = genesis(&creator, DEVICE_SEED);
        let device = verify_chain(&chain).unwrap().head().members[0].clone();
        let mut relay_member = node(relay, &device.id);
        let mut target_member = node(target, &device.id);
        target_member.addresses = vec!["http://127.0.0.1:1".to_owned()];
        edit(&mut relay_member, &mut target_member);
        let chain = append(&chain, &creator, vec![device, relay_member, target_member]);
        relay.accept_chain(&chain).unwrap();
        target.accept_chain(&chain).unwrap();
    }

    fn auth(node: &MeshStore, relay: &str, ts: u64, id: Option<&str>) -> Auth {
        let mut bytes = [0u8; 16];
        OsRng.fill_bytes(&mut bytes);
        let nonce = URL_SAFE_NO_PAD.encode(bytes);
        Auth {
            node: node.node_id().to_owned(),
            relay: relay.to_owned(),
            ts,
            sig: node.sign_uplink(relay, ts, &nonce, id),
            nonce,
        }
    }

    #[test]
    fn uplinks_need_a_fresh_signature_of_a_relayable_group_node() {
        let (relay, target) = (store(), store());
        group(&relay, &target, |_, _| {});
        let r = relay.node_id().to_owned();
        let now = unix_ms();
        let valid = auth(&target, &r, now, None);
        assert!(authorize(&relay, &valid, None, now).is_ok());
        // The same nonce again is a replay.
        assert!(matches!(
            authorize(&relay, &valid, None, now),
            Err(AppError::Unauthenticated)
        ));
        // A signature over other bytes, or without the callback id it binds.
        let mut forged = auth(&target, &r, now, None);
        forged.sig = auth(&target, &r, now, None).sig;
        assert!(matches!(
            authorize(&relay, &forged, None, now),
            Err(AppError::Unauthenticated)
        ));
        let control = auth(&target, &r, now, None);
        assert!(matches!(
            authorize(&relay, &control, Some("abc"), now),
            Err(AppError::Unauthenticated)
        ));
        let callback = auth(&target, &r, now, Some("abc"));
        assert!(authorize(&relay, &callback, Some("abc"), now).is_ok());
        // Callbacks stay out of the nonce cache (busy relays would fill it):
        // the pending table, which hands each id out once, makes them
        // single-use.
        assert!(authorize(&relay, &callback, Some("abc"), now).is_ok());
        // Older or newer than five minutes.
        let skew = MAX_CLOCK_SKEW.as_millis() as u64 + 1_000;
        for ts in [now - skew, now + skew] {
            assert!(matches!(
                authorize(&relay, &auth(&target, &r, ts, None), None, now),
                Err(AppError::Unauthenticated)
            ));
        }
        // Meant for another relay.
        assert!(matches!(
            authorize(&relay, &auth(&target, "node_other", now, None), None, now),
            Err(AppError::Unauthorized(_))
        ));
        // A node outside the group.
        let stranger = store();
        assert!(matches!(
            authorize(&relay, &auth(&stranger, &r, now, None), None, now),
            Err(AppError::Unauthorized(_))
        ));
    }

    #[test]
    fn uplinks_follow_the_roster_relay_settings() {
        // This node does not relay.
        let (relay, target) = (store(), store());
        group(&relay, &target, |relay, _| {
            relay.relay = Some(RelayRole::Denied)
        });
        let now = unix_ms();
        let request = auth(&target, relay.node_id(), now, None);
        assert!(matches!(
            authorize(&relay, &request, None, now),
            Err(AppError::Unauthorized(_))
        ));
        assert!(target.uplink_relays(&Default::default()).is_empty());
        // The node may not be reached by relay.
        let (relay, target) = (store(), store());
        group(&relay, &target, |_, target| {
            target.relay_target = Some(false)
        });
        let request = auth(&target, relay.node_id(), now, None);
        assert!(matches!(
            authorize(&relay, &request, None, now),
            Err(AppError::Unauthorized(_))
        ));
        assert!(target.uplink_relays(&Default::default()).is_empty());
        // Otherwise the node keeps an uplink to the relay (and the relay,
        // reachable by relay itself, to the node, whose only pinned address
        // is loopback and is left out).
        let (relay, target) = (store(), store());
        group(&relay, &target, |_, _| {});
        assert_eq!(
            target.uplink_relays(&Default::default()),
            vec![(relay.node_id().to_owned(), Vec::new())]
        );
        assert_eq!(
            relay.uplink_relays(&Default::default()),
            vec![(target.node_id().to_owned(), Vec::new())]
        );
    }

    #[tokio::test(start_paused = true)]
    async fn a_relay_gives_up_on_a_callback_after_ten_seconds() {
        let node = format!("node_{}", uuid::Uuid::new_v4().simple());
        let (opens, mut asked) = mpsc::channel(1);
        registry().links.lock().unwrap().insert(
            node.clone(),
            Link {
                generation: 0,
                opens,
            },
        );
        let started = tokio::time::Instant::now();
        let opened = open(&node).await.unwrap();
        assert!(matches!(opened, Err(AppError::RemoteUnreachable(_))));
        assert!(started.elapsed() >= CALLBACK_TIMEOUT);
        let id = asked.recv().await.unwrap();
        assert!(!registry().pending.lock().unwrap().contains_key(&id));
        drop_link(&node);
        assert!(open(&node).await.is_none());
    }

    #[tokio::test]
    async fn served_streams_are_remote_and_limited_to_the_forwarded_routes() {
        async fn echo(request: AxumRequest) -> String {
            let peer = super::super::sealed::peer_address(request.extensions()).unwrap();
            format!(
                "{} {}",
                peer.is_loopback(),
                request.headers()[header::HOST].to_str().unwrap()
            )
        }
        let app = served_app(
            Router::new()
                .route("/v2/version", get(echo))
                .route("/v2/ws", get(echo))
                .route("/v2/workspaces", get(echo)),
        );
        let (client, server) = tokio::io::duplex(64 * 1024);
        tokio::spawn(serve_stream(
            app,
            TokioIo::new(server),
            SocketAddr::from(([127, 0, 0, 1], 5)),
        ));
        let (mut sender, connection) = hyper::client::conn::http1::handshake(TokioIo::new(client))
            .await
            .unwrap();
        tokio::spawn(connection);
        for (path, status, body) in [
            ("/v2/version", 200, Some(format!("false {UPLINK_HOST}"))),
            ("/v2/ws?tv=2", 200, Some(format!("false {UPLINK_HOST}"))),
            ("/v2/ws", 404, None),
            ("/v2/workspaces", 404, None),
        ] {
            let request = Request::get(path)
                .header(header::HOST, "127.0.0.1")
                .body(Body::empty())
                .unwrap();
            let response = sender.send_request(request).await.unwrap();
            assert_eq!(response.status().as_u16(), status, "{path}");
            let bytes = axum::body::to_bytes(Body::new(response.into_body()), 1 << 16)
                .await
                .unwrap();
            if let Some(body) = body {
                assert_eq!(String::from_utf8_lossy(&bytes), body, "{path}");
            }
        }
    }

    /// Waits until `node` holds an uplink other than `previous`.
    async fn wait_for_link(node: &str, previous: Option<u64>) -> u64 {
        tokio::time::timeout(Duration::from_secs(15), async {
            loop {
                if let Some(generation) = link_generation(node).filter(|g| Some(*g) != previous) {
                    return generation;
                }
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
        })
        .await
        .expect("the node connects its uplink")
    }

    #[tokio::test]
    async fn a_relay_reaches_a_node_behind_nat_over_its_uplink() {
        let relay = TestServer::start(PairingEncryption::X25519).await;
        let target = TestServer::start(PairingEncryption::X25519).await;
        let (relay_mesh, target_mesh) = (
            relay.state.mesh.clone().unwrap(),
            target.state.mesh.clone().unwrap(),
        );
        // The target's only address is unreachable (and loopback, so the
        // relay skips it anyway).
        group(&relay_mesh, &target_mesh, |_, _| {});
        let (r, t) = (
            relay_mesh.node_id().to_owned(),
            target_mesh.node_id().to_owned(),
        );
        let device = TestDevice::new(DEVICE_SEED);
        let expires = unix_ms() + 60_000;
        let signature = device.key.sign(&super::super::mesh_relay::ticket_message(
            &device.device_id,
            &r,
            &t,
            expires,
        ));
        let ticket = format!(
            "{}.{expires}.{}",
            device.device_id,
            URL_SAFE_NO_PAD.encode(signature.to_bytes())
        );
        let base = format!("http://{}/v2/mesh/relay/{t}/{ticket}", relay.address);
        let http = reqwest::Client::new();

        // Without an uplink the target is unreachable.
        let response = http.get(format!("{base}/health")).send().await.unwrap();
        assert_eq!(response.status(), 502, "{:?}", response.text().await);

        // The target dials the relay (its loopback address is allowed only
        // here; the dialer skips loopback relay addresses).
        let (_urls, receiver) = watch::channel(vec![format!("http://{}", relay.address)]);
        let uplink = tokio::spawn(maintain(
            target_mesh.clone(),
            served_app(super::super::router(target.state.clone())),
            r.clone(),
            receiver,
        ));
        let first = wait_for_link(&t, None).await;

        // Public routes reach the target, not the relay.
        let version: serde_json::Value = http
            .get(format!("{base}/v2/version"))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(version["nodeId"], t);
        let response = http.get(format!("{base}/health")).send().await.unwrap();
        assert_eq!(response.status(), 200);

        // A sealed request for the target's key, signed by the group device.
        let keys = target.state.pairing_keys.current().unwrap();
        let protocol = EncryptionProtocol::X25519;
        let inner = signed_inner(&device, "GET", "/v2/workspaces", &[]);
        let (response, down) = seal_request(&keys, protocol, &inner)
            .send_to(&format!("{base}/v2/sealed"))
            .await;
        assert_eq!(response.status(), 200);
        let opened = open_response(down, &response.bytes().await.unwrap());
        assert_eq!(
            opened.status,
            200,
            "{}",
            String::from_utf8_lossy(&opened.body)
        );

        // The encrypted socket, keyed for the target.
        let offer = ws_offer(&keys, protocol);
        let signed = device.sign_query(&format!("/v2/ws?{}", offer.query));
        let ws = format!(
            "ws://{}/v2/mesh/relay/{t}/{ticket}/v2/ws?{}&{signed}",
            relay.address, offer.query
        );
        let (mut socket, _) = tokio_tungstenite::connect_async(ws).await.unwrap();
        let mut channel = offer
            .accept_hello(&keys, &device.device_id, &mut socket)
            .await;
        ping(&mut socket, &mut channel, "via-uplink").await;

        // A callback nobody waits for.
        let response = http
            .get(format!(
                "http://{}{UPLINK_PATH}/unknown?{}",
                relay.address,
                signed_query(&target_mesh, &r, Some("unknown"))
            ))
            .header(header::CONNECTION, "upgrade")
            .header(header::UPGRADE, UPGRADE_TOKEN)
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), 404);

        // The relay drops the control socket; the target reconnects.
        drop_link(&t);
        wait_for_link(&t, Some(first)).await;
        let response = http.get(format!("{base}/health")).send().await.unwrap();
        assert_eq!(response.status(), 200);

        uplink.abort();
        drop(socket);
    }
}
