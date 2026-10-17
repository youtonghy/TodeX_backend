//! `POST /v2/browser/proxy` — runs one HTTP request of the clients' browser
//! preview against this host's loopback and streams the answer back.
//!
//! The desktop intercepts every loopback request of its preview tab and
//! sends it here (through the `/v2/sealed` tunnel when the daemon is
//! remote). The tunnel forwards only a few inner request headers and joins
//! repeated response headers, so the page's own headers travel in the JSON
//! body and the upstream's in `x-todex-proxy-headers`. Redirects are not
//! followed: the preview browser follows them itself.

use std::sync::OnceLock;
use std::time::Duration;

use axum::body::Body;
use axum::extract::{DefaultBodyLimit, State};
use axum::http::{header, HeaderMap, HeaderName, HeaderValue, Method, StatusCode};
use axum::response::Response;
use axum::routing::post;
use axum::{Json, Router};
use base64::engine::general_purpose::{STANDARD as BASE64_STANDARD, URL_SAFE_NO_PAD};
use base64::Engine as _;
use serde::Deserialize;

use crate::app_state::AppState;
use crate::error::AppError;

use super::v2::{is_allowed_browser_target, require_auth, validate_browser_url};

/// Largest decoded request body forwarded to the page's server.
const MAX_PROXY_BODY_BYTES: usize = 16 * 1024 * 1024;
/// Route body limit: the base64 body (4/3 of the decoded size) plus room
/// for the URL and the header list.
const MAX_PROXY_REQUEST_BYTES: usize = MAX_PROXY_BODY_BYTES / 3 * 4 + 8 * 1024 * 1024;
// The route limit holds the largest encoded body and stays below what
// device auth buffers.
const _: () = assert!(
    MAX_PROXY_BODY_BYTES.div_ceil(3) * 4 < MAX_PROXY_REQUEST_BYTES
        && MAX_PROXY_REQUEST_BYTES < crate::device_auth::MAX_AUTH_BODY
);
const PROXY_CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
/// The encoded upstream header list must fit the sealed tunnel's 64 KiB
/// inner response head with room for the wrapper's own headers.
const MAX_PROXY_HEADERS_BYTES: usize = 48 * 1024;

pub(crate) const HEADER_PROXY_STATUS: &str = "x-todex-proxy-status";
pub(crate) const HEADER_PROXY_HEADERS: &str = "x-todex-proxy-headers";

/// Headers that describe one connection, never forwarded either way.
const HOP_BY_HOP: [&str; 6] = [
    "connection",
    "keep-alive",
    "te",
    "trailer",
    "transfer-encoding",
    "upgrade",
];

fn is_hop_by_hop(name: &str) -> bool {
    HOP_BY_HOP.contains(&name) || name.starts_with("proxy-")
}

/// Request headers the proxy sets itself or that belong to TodeX.
fn is_dropped_request_header(name: &str) -> bool {
    is_hop_by_hop(name)
        || name == "host"
        || name == "content-length"
        || name.starts_with("x-todex-")
}

pub(super) fn routes() -> Router<AppState> {
    Router::new().route(
        "/v2/browser/proxy",
        post(browser_proxy).layer(DefaultBodyLimit::max(MAX_PROXY_REQUEST_BYTES)),
    )
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct BrowserProxyRequest {
    method: String,
    url: String,
    #[serde(default)]
    headers: Vec<(String, String)>,
    #[serde(default)]
    body: Option<String>,
}

/// One client for every proxied request: no system proxy, no redirects,
/// no decompression (reqwest is built without it), no total timeout so
/// long responses and event streams keep flowing.
fn proxy_client() -> Result<&'static reqwest::Client, AppError> {
    static CLIENT: OnceLock<reqwest::Client> = OnceLock::new();
    if let Some(client) = CLIENT.get() {
        return Ok(client);
    }
    let client = reqwest::Client::builder()
        .no_proxy()
        .redirect(reqwest::redirect::Policy::none())
        .connect_timeout(PROXY_CONNECT_TIMEOUT)
        .build()
        .map_err(|error| AppError::Anyhow(anyhow::anyhow!("browser proxy client: {error}")))?;
    Ok(CLIENT.get_or_init(|| client))
}

async fn browser_proxy(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(request): Json<BrowserProxyRequest>,
) -> Result<Response, AppError> {
    require_auth(&state, &headers)?;
    let url = validate_browser_url(&request.url)?;
    let parsed = reqwest::Url::parse(&url)
        .map_err(|_| AppError::InvalidRequest("browser target is not allowed".to_owned()))?;
    // The daemon's own port is refused: a proxied request would arrive as
    // a loopback peer and could reach the anonymous local-only routes.
    if !is_allowed_browser_target(&parsed)
        || crate::agent_browser::policy::is_daemon_port(parsed.port_or_known_default())
    {
        return Err(AppError::InvalidRequest(
            "browser target is not allowed".to_owned(),
        ));
    }
    let method = Method::from_bytes(request.method.trim().as_bytes())
        .ok()
        .filter(|method| *method != Method::CONNECT)
        .ok_or_else(|| AppError::InvalidRequest("invalid HTTP method".to_owned()))?;
    let mut forwarded = HeaderMap::new();
    for (name, value) in &request.headers {
        let name = HeaderName::from_bytes(name.trim().as_bytes())
            .map_err(|_| AppError::InvalidRequest("invalid request header name".to_owned()))?;
        if is_dropped_request_header(name.as_str()) {
            continue;
        }
        let value = HeaderValue::from_str(value)
            .map_err(|_| AppError::InvalidRequest("invalid request header value".to_owned()))?;
        forwarded.append(name, value);
    }
    let body = match request.body.as_deref() {
        Some(encoded) if !encoded.is_empty() => {
            if encoded.len() > MAX_PROXY_BODY_BYTES.div_ceil(3) * 4 {
                return Err(AppError::InvalidRequest(format!(
                    "browser proxy body exceeds {MAX_PROXY_BODY_BYTES} bytes"
                )));
            }
            Some(BASE64_STANDARD.decode(encoded).map_err(|_| {
                AppError::InvalidRequest("browser proxy body is not valid base64".to_owned())
            })?)
        }
        _ => None,
    };

    let target = format!(
        "{}:{}",
        parsed.host_str().unwrap_or_default(),
        parsed.port_or_known_default().unwrap_or_default()
    );
    let mut upstream = proxy_client()?.request(method, parsed).headers(forwarded);
    if let Some(body) = body {
        upstream = upstream.body(body);
    }
    let response = upstream.send().await.map_err(|error| {
        AppError::BrowserTargetUnreachable(format!("{target}: {}", error_chain(&error)))
    })?;

    let pairs: Vec<(&str, String)> = response
        .headers()
        .iter()
        .filter(|(name, _)| !is_hop_by_hop(name.as_str()))
        .map(|(name, value)| {
            (
                name.as_str(),
                String::from_utf8_lossy(value.as_bytes()).into_owned(),
            )
        })
        .collect();
    let encoded_headers = URL_SAFE_NO_PAD.encode(serde_json::to_vec(&pairs)?);
    if encoded_headers.len() > MAX_PROXY_HEADERS_BYTES {
        return Err(AppError::BrowserTargetUnreachable(format!(
            "{target}: response headers exceed {MAX_PROXY_HEADERS_BYTES} bytes"
        )));
    }
    let status = response.status().as_u16();
    Response::builder()
        .status(StatusCode::OK)
        .header(HEADER_PROXY_STATUS, status)
        .header(HEADER_PROXY_HEADERS, encoded_headers)
        .header(header::CONTENT_TYPE, "application/octet-stream")
        .header(header::CACHE_CONTROL, "no-store")
        .body(Body::from_stream(response.bytes_stream()))
        .map_err(|error| AppError::Anyhow(anyhow::anyhow!("browser proxy response: {error}")))
}

/// The error and its sources, so "connection refused" is not hidden behind
/// reqwest's generic "error sending request".
fn error_chain(error: &(dyn std::error::Error + 'static)) -> String {
    let mut message = error.to_string();
    let mut source = error.source();
    while let Some(cause) = source {
        let text = cause.to_string();
        if !message.contains(&text) {
            message.push_str(": ");
            message.push_str(&text);
        }
        source = cause.source();
    }
    message
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn request_header_filter_drops_connection_host_length_and_todex_headers() {
        for name in [
            "connection",
            "keep-alive",
            "proxy-authorization",
            "proxy-connection",
            "te",
            "trailer",
            "transfer-encoding",
            "upgrade",
            "host",
            "content-length",
            "x-todex-device-id",
        ] {
            assert!(is_dropped_request_header(name), "{name}");
        }
        for name in [
            "cookie",
            "authorization",
            "origin",
            "referer",
            "range",
            "if-none-match",
            "accept-encoding",
            "content-type",
        ] {
            assert!(!is_dropped_request_header(name), "{name}");
        }
    }
}
