//! `POST /v2/sealed`: the transport v2 REST tunnel (`docs/transport-v2.md`).
//!
//! The outer request carries one complete inner HTTP request sealed with
//! `k_up`. The inner request runs through the same router as a direct
//! request (device auth, per-route body limits, handlers) and its response
//! streams back sealed with `k_down`, record by record as the inner body is
//! produced. Any problem with the outer request answers a plain
//! `400 TRANSPORT_CRYPTO_FAILED` without detail.
use std::net::SocketAddr;

use axum::body::{Body, Bytes};
use axum::extract::{ConnectInfo, Request, State};
use axum::http::{header, HeaderMap, HeaderName, HeaderValue, Method, StatusCode, Uri};
use axum::response::{IntoResponse, Response};
use axum::routing::post;
use axum::Router;
use futures_util::StreamExt;
use tower::ServiceExt;
use tracing::debug;

use crate::app_state::AppState;
use crate::config::PairingEncryption;
use crate::device_auth;
use crate::error::AppError;
use crate::transport_crypto::channel::{RecordCipher, DIRECTION_DOWN, DIRECTION_UP};
use crate::transport_crypto::envelope::{
    encode_response_head, sealed_stream_length, split_inner_request, InnerRequestHead,
    InnerResponseHead, RecordStreamDecoder, RecordStreamSealer, MAX_HEAD_BYTES,
    SEALED_CONTENT_TYPE, SEALED_PATH,
};
use crate::transport_crypto::handshake::{decode_b64url, REST_LABEL};
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
}

pub(super) fn routes(api: Router, state: AppState) -> Router {
    Router::new()
        .route(SEALED_PATH, post(sealed))
        .with_state(SealedState { api, state })
}

async fn sealed(State(sealed): State<SealedState>, request: Request) -> Response {
    let (inner, down) = match open_request(&sealed.state, request).await {
        Ok(opened) => opened,
        Err(error) => {
            debug!(reason = error.reason(), "sealed request rejected");
            return AppError::TransportCryptoFailed.into_response();
        }
    };
    let response = match sealed.api.oneshot(inner).await {
        Ok(response) => response,
        Err(infallible) => match infallible {},
    };
    match seal_response(response, down) {
        Ok(response) => response,
        Err(error) => {
            debug!(reason = error.reason(), "sealed response failed");
            AppError::TransportCryptoFailed.into_response()
        }
    }
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
/// and builds the inner request. Returns the response cipher.
async fn open_request(
    state: &AppState,
    request: Request,
) -> Result<(Request, RecordCipher), TransportCryptoError> {
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
    let keys = state.pairing_keys.server_session_keys(
        REST_LABEL,
        protocol,
        "",
        &material,
        &client_nonce,
        &[],
    )?;
    let mut decoder =
        RecordStreamDecoder::new(RecordCipher::new(&keys.k_up, &keys.th, DIRECTION_UP));
    let down = RecordCipher::new(&keys.k_down, &keys.th, DIRECTION_DOWN);
    drop(keys);

    let mut received = 0_usize;
    let mut plaintext = Vec::new();
    let mut stream = body.into_data_stream();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(|_| TransportCryptoError::new("outer body read"))?;
        received = received.saturating_add(chunk.len());
        if received > MAX_OUTER_BODY {
            return Err(TransportCryptoError::new("outer body too large"));
        }
        decoder.push(&chunk, &mut plaintext)?;
    }
    decoder.finish()?;
    let (head, body) = split_inner_request(plaintext)?;
    let inner = inner_request(head, body, &parts.headers, &parts.extensions)?;
    Ok((inner, down))
}

fn inner_request(
    head: InnerRequestHead,
    body: Vec<u8>,
    outer_headers: &HeaderMap,
    outer_extensions: &axum::http::Extensions,
) -> Result<Request, TransportCryptoError> {
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
    let mut request = Request::new(Body::from(body));
    *request.method_mut() = method;
    *request.uri_mut() = uri;
    let headers = request.headers_mut();
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
    // Only the peer address carries over: the inner request gets no other
    // outer extension (body limits, matched route, ...).
    if let Some(peer) = peer_address(outer_extensions) {
        request.extensions_mut().insert(ConnectInfo(peer));
    }
    request.extensions_mut().insert(ArrivedViaTransportV2);
    Ok(request)
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

/// `200 application/vnd.todex.sealed` whose body is the inner response
/// sealed record by record as the inner body streams.
fn seal_response(response: Response, down: RecordCipher) -> Result<Response, TransportCryptoError> {
    let (parts, body) = response.into_parts();
    let mut headers = std::collections::BTreeMap::<String, String>::new();
    for (name, value) in &parts.headers {
        if HOP_BY_HOP.contains(&name.as_str()) {
            continue;
        }
        let Ok(value) = value.to_str() else {
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
    let sealer = RecordStreamSealer::new(down, head);
    let stream = futures_util::stream::unfold(
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
    Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, SEALED_CONTENT_TYPE)
        .header(header::CACHE_CONTROL, "no-store")
        .body(Body::from_stream(stream))
        .map_err(|_| TransportCryptoError::new("outer response"))
}

fn stream_error(reason: &'static str) -> std::io::Error {
    std::io::Error::other(reason)
}
