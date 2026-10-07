//! `POST /v2/sealed`: the transport v2 REST tunnel (`docs/transport-v2.md`).
//!
//! The outer request carries one complete inner HTTP request sealed with
//! `k_up`. The inner request runs through the same router as a direct
//! request (device auth, per-route body limits, handlers) and its response
//! streams back sealed with `k_down`, record by record as the inner body is
//! produced, behind a fresh 32-byte response nonce (sealed revision 2). Any
//! problem with the outer request answers a plain
//! `400 TRANSPORT_CRYPTO_FAILED` without detail; an outer request without
//! `X-Todex-Sealed-Revision: 2` answers a plain `426` before its body is read.
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use axum::body::{Body, Bytes};
use axum::extract::{ConnectInfo, Request, State};
use axum::http::{header, HeaderMap, HeaderName, HeaderValue, Method, StatusCode, Uri};
use axum::response::{IntoResponse, Response};
use axum::routing::post;
use axum::Router;
use futures_util::StreamExt;
use rand_core::{OsRng, RngCore};
use tokio::sync::{OwnedSemaphorePermit, Semaphore};
use tokio::time::Instant;
use tower::ServiceExt;
use tracing::debug;

use crate::app_state::AppState;
use crate::config::PairingEncryption;
use crate::device_auth;
use crate::error::AppError;
use crate::transport_crypto::channel::{RecordCipher, DIRECTION_DOWN, DIRECTION_UP};
use crate::transport_crypto::envelope::{
    encode_response_head, parse_inner_head, sealed_stream_length, InnerRequestHead,
    InnerResponseHead, RecordStreamDecoder, RecordStreamSealer, MAX_HEAD_BYTES,
    SEALED_CONTENT_TYPE, SEALED_PATH, SEALED_RESPONSE_CONTENT_TYPE, SEALED_REVISION,
    SEALED_REVISION_HEADER,
};
use crate::transport_crypto::handshake::{decode_b64url, RESPONSE_NONCE_LENGTH};
use crate::transport_crypto::{EncryptionProtocol, TransportCryptoError};

const HEADER_TRANSPORT: &str = "x-todex-transport";
const HEADER_ENCRYPTION: &str = "x-todex-encryption";
const HEADER_CLIENT_KEY: &str = "x-todex-client-key";
const HEADER_KEM_CIPHERTEXT: &str = "x-todex-kem-ciphertext";
const HEADER_REQUEST_NONCE: &str = "x-todex-request-nonce";

/// Largest inner plaintext: the head plus the largest body device auth
/// buffers (every route-level limit is below it).
const MAX_INNER_PLAINTEXT: usize = 4 + MAX_HEAD_BYTES + device_auth::MAX_AUTH_BODY;
/// The outer body limit: that plaintext plus the record overhead.
const MAX_OUTER_BODY: usize = sealed_stream_length(MAX_INNER_PLAINTEXT);

/// Inner body limit for routes that answer without a device credential
/// (see [`is_public_route`]). Their real bodies are small JSON documents.
const PUBLIC_INNER_BODY_MAX: usize = 64 * 1024;

/// Tunnel requests with a device credential that may buffer an inner body
/// at the same time. Only requests whose inner head passed [`admit`] wait
/// for a permit, so peers without a credential cannot occupy one; a request
/// holds it until the inner handler has produced its response head.
const AUTHENTICATED_PERMITS: usize = 32;
/// The same for public inner routes (no credential): a separate, small pool,
/// so anonymous peers cannot take slots from paired devices.
const PUBLIC_PERMITS: usize = 4;

/// Time limits that keep a tunnel request from holding the daemon.
#[derive(Clone, Copy, Debug)]
pub(crate) struct TunnelLimits {
    /// From the start of the request until the inner head is decrypted;
    /// past it the request fails as an outer `400`.
    pub head_deadline: Duration,
    /// Longest gap between two outer body chunks. Before the inner head:
    /// outer `400`; after it: sealed inner `408 REQUEST_TIMEOUT`.
    pub idle_timeout: Duration,
    /// Longest wait for a permit; then sealed inner `503 TRANSPORT_BUSY`.
    pub permit_wait: Duration,
}

impl Default for TunnelLimits {
    fn default() -> Self {
        Self {
            head_deadline: Duration::from_secs(30),
            idle_timeout: Duration::from_secs(30),
            permit_wait: Duration::from_secs(10),
        }
    }
}

/// Inner request headers the tunnel forwards; everything else (hop-by-hop
/// headers, `x-todex-transport*`, cookies, ...) is dropped.
const FORWARDED_INNER_HEADERS: [&str; 6] = [
    "content-type",
    "accept",
    device_auth::HEADER_DEVICE_ID,
    device_auth::HEADER_TIMESTAMP,
    device_auth::HEADER_NONCE,
    device_auth::HEADER_SIGNATURE,
];

/// Inner response headers that describe the outer connection, not the
/// inner message.
const HOP_BY_HOP: [&str; 8] = [
    "connection",
    "keep-alive",
    "proxy-authenticate",
    "proxy-authorization",
    "te",
    "trailer",
    "transfer-encoding",
    "upgrade",
];

/// Request extension the tunnel sets on every inner request. Only the
/// server can create it (it never comes from the wire), so handlers and
/// middleware can tell that the request arrived through transport v2.
#[derive(Clone, Copy, Debug)]
pub(crate) struct ArrivedViaTransportV2;

#[derive(Clone)]
struct SealedState {
    /// The router without the tunnel route: a nested `/v2/sealed` cannot
    /// even be routed.
    api: Router,
    state: AppState,
    limits: TunnelLimits,
    authenticated_permits: Arc<Semaphore>,
    public_permits: Arc<Semaphore>,
}

pub(super) fn routes(api: Router, state: AppState) -> Router {
    routes_with_limits(api, state, TunnelLimits::default(), AUTHENTICATED_PERMITS)
}

/// [`routes`] with explicit limits and authenticated pool size (tests).
pub(super) fn routes_with_limits(
    api: Router,
    state: AppState,
    limits: TunnelLimits,
    authenticated_permits: usize,
) -> Router {
    Router::new()
        .route(SEALED_PATH, post(sealed))
        .with_state(SealedState {
            api,
            state,
            limits,
            authenticated_permits: Arc::new(Semaphore::new(authenticated_permits)),
            public_permits: Arc::new(Semaphore::new(PUBLIC_PERMITS)),
        })
}

async fn sealed(State(sealed): State<SealedState>, request: Request) -> Response {
    // Checked before anything else, so an older client learns that it must
    // update instead of failing to decrypt the response.
    let revision = request
        .headers()
        .get(SEALED_REVISION_HEADER)
        .and_then(|value| value.to_str().ok());
    if revision != Some(SEALED_REVISION.to_string().as_str()) {
        return AppError::ProtocolUpgradeRequired(format!(
            "this server speaks sealed revision {SEALED_REVISION}; update the client (X-Todex-Sealed-Revision: {SEALED_REVISION})"
        ))
        .into_response();
    }
    let (opened, down) = match open_request(&sealed, request).await {
        Ok(opened) => opened,
        Err(error) => {
            debug!(reason = error.reason(), "sealed request rejected");
            return AppError::TransportCryptoFailed.into_response();
        }
    };
    let response = match opened {
        Opened::Forward(inner, permit) => {
            let response = match sealed.api.oneshot(inner).await {
                Ok(response) => response,
                Err(infallible) => match infallible {},
            };
            // The inner body was consumed; streaming the response needs no
            // permit.
            drop(permit);
            response
        }
        // Answered from the inner head alone, before the inner body was read.
        Opened::Rejected(error) => error.into_response(),
    };
    match seal_response(response, down) {
        Ok(response) => response,
        Err(error) => {
            debug!(reason = error.reason(), "sealed response failed");
            AppError::TransportCryptoFailed.into_response()
        }
    }
}

/// What an opened tunnel request turns into.
enum Opened {
    /// The complete inner request, holding a permit from its pool.
    Forward(Request, OwnedSemaphorePermit),
    /// The request was refused without running it (by the inner head
    /// alone, no free permit, or a stalled body); the error is sealed like
    /// any inner response.
    Rejected(AppError),
}

/// The response half of a tunnel request: a fresh response nonce and the
/// `k_down` cipher derived from it.
struct ResponseKeys {
    nonce: [u8; RESPONSE_NONCE_LENGTH],
    cipher: RecordCipher,
}

/// Routes that answer without a device credential (compare
/// `enforcement::is_direct_route`). Every other inner path, including
/// unknown ones, needs a credential before its body is read.
fn is_public_route(path: &str) -> bool {
    matches!(path, "/health" | "/v2/transport-policy" | "/v2/version")
        || path.starts_with("/v2/device-pairing/")
}

/// Body-independent checks on the inner head, run as soon as the head is
/// decrypted: the device credential is present and names a registered
/// device (anonymous deployments: the request is local), so an unsigned
/// peer cannot make the daemon buffer a large inner body. Returns the inner
/// body limit. The full signature check over the body still runs in the
/// router's device auth middleware.
fn admit(state: &AppState, parts: &InnerParts) -> Result<Admission, AppError> {
    if is_public_route(parts.uri.path()) {
        return Ok(Admission {
            body_limit: PUBLIC_INNER_BODY_MAX,
            public: true,
        });
    }
    if state.config.security.enable_auth {
        state
            .device_auth
            .check_credential_headers(&parts.headers, parts.uri.query())?;
    } else {
        device_auth::ensure_local_request(&parts.headers, &parts.uri)?;
    }
    Ok(Admission {
        body_limit: device_auth::MAX_AUTH_BODY,
        public: false,
    })
}

/// The inner body limit and permit pool of an admitted inner head.
struct Admission {
    body_limit: usize,
    public: bool,
}

fn header<'a>(headers: &'a HeaderMap, name: &str) -> Result<&'a str, TransportCryptoError> {
    headers
        .get(name)
        .and_then(|value| value.to_str().ok())
        .ok_or(TransportCryptoError::new("missing outer header"))
}

fn is_sealed_content_type(headers: &HeaderMap) -> bool {
    header(headers, header::CONTENT_TYPE.as_str()).is_ok_and(|value| {
        value
            .split(';')
            .next()
            .is_some_and(|media| media.trim().eq_ignore_ascii_case(SEALED_CONTENT_TYPE))
    })
}

/// Checks the outer headers, runs the key agreement, opens the record stream
/// and builds the inner request. Returns the response keys.
async fn open_request(
    sealed: &SealedState,
    request: Request,
) -> Result<(Opened, ResponseKeys), TransportCryptoError> {
    let started = Instant::now();
    let state = &sealed.state;
    let limits = sealed.limits;
    let (parts, body) = request.into_parts();
    let headers = &parts.headers;
    if !is_sealed_content_type(headers) || header(headers, HEADER_TRANSPORT)? != "2" {
        return Err(TransportCryptoError::new("outer headers"));
    }
    let protocol = EncryptionProtocol::parse(header(headers, HEADER_ENCRYPTION)?)
        .ok_or(TransportCryptoError::new("outer protocol"))?;
    let required = state.config.pairing_encryption;
    if required != PairingEncryption::None && required.as_str() != protocol.as_str() {
        return Err(TransportCryptoError::new(
            "protocol is not the required one",
        ));
    }
    let material = decode_b64url(header(
        headers,
        match protocol {
            EncryptionProtocol::X25519 => HEADER_CLIENT_KEY,
            EncryptionProtocol::MlKem768 => HEADER_KEM_CIPHERTEXT,
        },
    )?)?;
    let client_nonce = decode_b64url(header(headers, HEADER_REQUEST_NONCE)?)?;
    let pairing_keys = state.pairing_keys.current().map_err(|error| {
        tracing::warn!(%error, "pairing keys unavailable; refusing sealed request");
        TransportCryptoError::new("pairing keys unavailable")
    })?;
    let keys = pairing_keys.server_rest_keys(protocol, &material, &client_nonce)?;
    let mut decoder =
        RecordStreamDecoder::new(RecordCipher::new(&keys.k_up, &keys.th, DIRECTION_UP));
    // Every response, sealed inner errors included, gets its own nonce and
    // therefore its own key, even when the request is a replay. Deriving it
    // now lets `prk` be wiped right away.
    let mut nonce = [0_u8; RESPONSE_NONCE_LENGTH];
    OsRng.fill_bytes(&mut nonce);
    let (th, k_down) = keys.into_k_down(&nonce)?;
    let down = ResponseKeys {
        nonce,
        cipher: RecordCipher::new(&k_down, &th, DIRECTION_DOWN),
    };
    drop((th, k_down));

    let head_deadline = started + limits.head_deadline;
    let mut received = 0_usize;
    let mut plaintext = Vec::new();
    // Set once the inner head is decrypted and admitted.
    let mut admitted: Option<(InnerParts, usize, usize, OwnedSemaphorePermit)> = None;
    let mut stream = body.into_data_stream();
    loop {
        if admitted.is_none() {
            if let Some((head, head_end)) = parse_inner_head(&plaintext)? {
                let inner = inner_parts(head, &parts.headers)?;
                let admission = match admit(state, &inner) {
                    Ok(admission) => admission,
                    // Stop reading: the rest of the outer body is dropped.
                    Err(error) => return Ok((Opened::Rejected(error), down)),
                };
                let pool = if admission.public {
                    &sealed.public_permits
                } else {
                    &sealed.authenticated_permits
                };
                let permit = match tokio::time::timeout(
                    limits.permit_wait,
                    Arc::clone(pool).acquire_owned(),
                )
                .await
                {
                    Ok(Ok(permit)) => permit,
                    Ok(Err(_)) => return Err(TransportCryptoError::new("tunnel closed")),
                    // Nothing ran and no signature nonce was claimed.
                    Err(_) => return Ok((Opened::Rejected(AppError::TransportBusy), down)),
                };
                admitted = Some((inner, head_end, admission.body_limit, permit));
            }
        }
        if let Some((_, head_end, body_limit, _)) = &admitted {
            if plaintext.len() - head_end > *body_limit {
                return Err(TransportCryptoError::new("inner body too large"));
            }
        }
        let idle_deadline = Instant::now() + limits.idle_timeout;
        let deadline = if admitted.is_some() {
            idle_deadline
        } else {
            idle_deadline.min(head_deadline)
        };
        let next = match tokio::time::timeout_at(deadline, stream.next()).await {
            Ok(next) => next,
            Err(_) if admitted.is_some() => {
                return Ok((Opened::Rejected(AppError::RequestTimeout), down))
            }
            Err(_) => return Err(TransportCryptoError::new("inner head not received in time")),
        };
        let Some(chunk) = next else {
            break;
        };
        let chunk = chunk.map_err(|_| TransportCryptoError::new("outer body read"))?;
        received = received.saturating_add(chunk.len());
        if received > MAX_OUTER_BODY {
            return Err(TransportCryptoError::new("outer body too large"));
        }
        decoder.push(&chunk, &mut plaintext)?;
    }
    decoder.finish()?;
    let Some((inner, head_end, _, permit)) = admitted else {
        return Err(TransportCryptoError::new("inner head truncated"));
    };
    // Reuse the allocation for the body.
    plaintext.drain(..head_end);
    let request = inner_request(inner, plaintext, &parts.extensions);
    Ok((Opened::Forward(request, permit), down))
}

/// Method, target and forwarded headers of the inner request.
struct InnerParts {
    method: Method,
    uri: Uri,
    headers: HeaderMap,
}

fn inner_parts(
    head: InnerRequestHead,
    outer_headers: &HeaderMap,
) -> Result<InnerParts, TransportCryptoError> {
    fn invalid<E>(_: E) -> TransportCryptoError {
        TransportCryptoError::new("invalid inner request")
    }
    let method = Method::from_bytes(head.method.as_bytes()).map_err(invalid)?;
    let target = match head.query.as_deref().filter(|query| !query.is_empty()) {
        Some(query) if query.contains('#') => return Err(TransportCryptoError::new("inner query")),
        Some(query) => format!("{}?{query}", head.path),
        None => head.path,
    };
    let uri = Uri::try_from(target).map_err(invalid)?;
    if uri.scheme().is_some() || uri.authority().is_some() {
        return Err(TransportCryptoError::new("inner request target"));
    }
    let mut headers = HeaderMap::new();
    for (name, value) in &head.headers {
        let name = name.to_ascii_lowercase();
        if !FORWARDED_INNER_HEADERS.contains(&name.as_str()) {
            continue;
        }
        headers.insert(
            HeaderName::from_bytes(name.as_bytes()).map_err(invalid)?,
            HeaderValue::from_str(value).map_err(invalid)?,
        );
    }
    // `Host` and `Origin` describe the outer connection; anonymous
    // (auth disabled) deployments check them for every request.
    for name in [header::HOST, header::ORIGIN] {
        for value in outer_headers.get_all(&name) {
            headers.append(name.clone(), value.clone());
        }
    }
    Ok(InnerParts {
        method,
        uri,
        headers,
    })
}

fn inner_request(
    parts: InnerParts,
    body: Vec<u8>,
    outer_extensions: &axum::http::Extensions,
) -> Request {
    let mut request = Request::new(Body::from(body));
    *request.method_mut() = parts.method;
    *request.uri_mut() = parts.uri;
    *request.headers_mut() = parts.headers;
    // Only the peer address carries over: the inner request gets no other
    // outer extension (body limits, matched route, ...).
    if let Some(peer) = peer_address(outer_extensions) {
        request.extensions_mut().insert(ConnectInfo(peer));
    }
    request.extensions_mut().insert(ArrivedViaTransportV2);
    request
}

/// The peer address the way the `ConnectInfo` extractor resolves it: the
/// served connection's, or axum's test-only `MockConnectInfo`.
pub(crate) fn peer_address(extensions: &axum::http::Extensions) -> Option<SocketAddr> {
    extensions
        .get::<ConnectInfo<SocketAddr>>()
        .map(|ConnectInfo(peer)| *peer)
        .or_else(|| {
            extensions
                .get::<axum::extract::connect_info::MockConnectInfo<SocketAddr>>()
                .map(|mock| mock.0)
        })
}

/// `200 application/vnd.todex.sealed; r=2` whose body is the response nonce
/// followed by the inner response sealed record by record as the inner body
/// streams.
fn seal_response(response: Response, down: ResponseKeys) -> Result<Response, TransportCryptoError> {
    let (parts, body) = response.into_parts();
    let mut headers = std::collections::BTreeMap::<String, String>::new();
    for (name, value) in &parts.headers {
        if HOP_BY_HOP.contains(&name.as_str()) {
            continue;
        }
        let Ok(value) = value.to_str() else {
            // The inner head is JSON text; a non-UTF-8 value cannot travel.
            debug!(
                header = name.as_str(),
                "sealed response dropped a non-text header value"
            );
            continue;
        };
        headers
            .entry(name.as_str().to_owned())
            .and_modify(|joined| {
                joined.push_str(", ");
                joined.push_str(value);
            })
            .or_insert_with(|| value.to_owned());
    }
    let head = encode_response_head(&InnerResponseHead {
        status: parts.status.as_u16(),
        headers,
    })?;
    let ResponseKeys { nonce, cipher } = down;
    let sealer = RecordStreamSealer::new(cipher, head);
    let records = futures_util::stream::unfold(
        Some((body.into_data_stream(), sealer)),
        |state| async move {
            let (mut inner, mut sealer) = state?;
            loop {
                match inner.next().await {
                    Some(Ok(chunk)) => match sealer.push(&chunk) {
                        Ok(out) if out.is_empty() => continue,
                        Ok(out) => return Some((Ok(Bytes::from(out)), Some((inner, sealer)))),
                        Err(error) => return Some((Err(stream_error(error.reason())), None)),
                    },
                    // A failed inner body ends the outer body without a
                    // final record, which clients report as truncation.
                    Some(Err(_)) => return Some((Err(stream_error("inner body failed")), None)),
                    None => {
                        return Some((
                            sealer
                                .finish()
                                .map(Bytes::from)
                                .map_err(|error| stream_error(error.reason())),
                            None,
                        ))
                    }
                }
            }
        },
    );
    let stream = futures_util::stream::once(async move {
        Ok::<_, std::io::Error>(Bytes::copy_from_slice(&nonce))
    })
    .chain(records);
    Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, SEALED_RESPONSE_CONTENT_TYPE)
        .header(header::CACHE_CONTROL, "no-store")
        .body(Body::from_stream(stream))
        .map_err(|_| TransportCryptoError::new("outer response"))
}

fn stream_error(reason: &'static str) -> std::io::Error {
    std::io::Error::other(reason)
}
