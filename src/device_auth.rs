//! Per-request device authentication.
//!
//! Every authenticated request carries an Ed25519 signature from a paired
//! device over a canonical description of the request:
//!
//! ```text
//! "todex.device-auth.v1\0" || device_id || "\0" || METHOD || "\0"
//!   || path || "\0" || canonical_query || "\0" || ts || "\0"
//!   || nonce || "\0" || base64url(sha256(body))
//! ```
//!
//! `canonical_query` is the form-decoded query pairs minus the four auth
//! parameters, each side re-encoded with strict percent encoding, sorted by
//! (key, value), joined with `&`. Because the signature binds method, path,
//! query and body, a captured credential header cannot be replayed against a
//! different request. Timestamps are accepted within ±300 seconds and nonces
//! are single-use within that window.
use crate::{
    app_state::AppState, devices::DeviceRegistry, error::AppError, server::websocket::AuthContext,
};
use axum::{
    body::{to_bytes, Body},
    extract::{Request, State},
    http::{header::HeaderName, HeaderMap, Method},
    middleware::Next,
    response::Response,
};
use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine};
use ed25519_dalek::{Signature, Verifier, VerifyingKey};
use sha2::{Digest, Sha256};
use std::{
    collections::{HashMap, VecDeque},
    sync::{Arc, Mutex},
    time::{SystemTime, UNIX_EPOCH},
};

pub(crate) const HEADER_DEVICE_ID: &str = "x-todex-device-id";
pub(crate) const HEADER_TIMESTAMP: &str = "x-todex-auth-ts";
pub(crate) const HEADER_NONCE: &str = "x-todex-auth-nonce";
pub(crate) const HEADER_SIGNATURE: &str = "x-todex-auth-sig";
/// Internal marker the middleware sets after verification. Clients can never
/// reach handlers with this header intact: the middleware strips it on entry.
pub(crate) const VERIFIED_HEADER: &str = "x-todex-verified-device";

const QUERY_DEVICE_ID: &str = "device_id";
const QUERY_TIMESTAMP: &str = "auth_ts";
const QUERY_NONCE: &str = "auth_nonce";
const QUERY_SIGNATURE: &str = "auth_sig";
const AUTH_QUERY_KEYS: [&str; 4] = [
    QUERY_DEVICE_ID,
    QUERY_TIMESTAMP,
    QUERY_NONCE,
    QUERY_SIGNATURE,
];

const SIGN_DOMAIN: &[u8] = b"todex.device-auth.v1\0";
const MAX_CLOCK_SKEW_SECS: u64 = 300;
/// Bodies are buffered once for hashing. Matches the largest route-level body
/// limit in the v2 router (workspace file writes are ~12 MiB).
const MAX_AUTH_BODY: usize = 32 * 1024 * 1024;
const NONCE_CACHE_LIMIT: usize = 65_536;

/// `device_id` → `(timestamp, nonce)` claims within the acceptance window.
type NonceCache = Arc<Mutex<HashMap<String, VecDeque<(u64, String)>>>>;

/// Device registry plus the in-memory single-use nonce window.
#[derive(Clone)]
pub(crate) struct DeviceAuthenticator {
    registry: DeviceRegistry,
    seen: NonceCache,
}

impl DeviceAuthenticator {
    pub(crate) fn new(registry: DeviceRegistry) -> Self {
        Self {
            registry,
            seen: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    fn authenticate(
        &self,
        method: &Method,
        path: &str,
        query: Option<&str>,
        headers: &HeaderMap,
        body: &[u8],
    ) -> Result<AuthContext, AppError> {
        let credential = Credential::extract(headers, query).ok_or(AppError::Unauthenticated)?;
        let timestamp = credential
            .timestamp
            .parse::<u64>()
            .map_err(|_| AppError::Unauthenticated)?;
        let now = unix_secs();
        if now.abs_diff(timestamp) > MAX_CLOCK_SKEW_SECS {
            return Err(AppError::Unauthenticated);
        }
        let Some(record) = self.registry.get(&credential.device_id)? else {
            return Err(AppError::Unauthenticated);
        };
        let public_key = crate::devices::parse_public_key(&record.public_key)
            .map_err(|_| AppError::Unauthenticated)?;
        let key = VerifyingKey::from_bytes(&public_key).map_err(|_| AppError::Unauthenticated)?;
        let signature = Signature::from_slice(
            &URL_SAFE_NO_PAD
                .decode(&credential.signature)
                .map_err(|_| AppError::Unauthenticated)?,
        )
        .map_err(|_| AppError::Unauthenticated)?;
        let payload = signed_payload(
            &credential.device_id,
            method.as_str(),
            path,
            &canonical_query(query),
            &credential.timestamp,
            &credential.nonce,
            body,
        );
        // Verify before claiming the nonce so a forged request cannot burn a
        // nonce belonging to an in-flight legitimate request.
        key.verify(&payload, &signature)
            .map_err(|_| AppError::Unauthenticated)?;
        self.claim_nonce(&credential.device_id, timestamp, &credential.nonce)?;
        self.registry.touch(&credential.device_id);
        Ok(AuthContext {
            principal_id: credential.device_id.clone(),
            tenant_id: "local".to_owned(),
            token_id: credential.device_id,
        })
    }

    /// Single-use nonces scoped per device within the accepted timestamp
    /// window. Replays are rejected even when the signature itself is valid.
    fn claim_nonce(&self, device_id: &str, timestamp: u64, nonce: &str) -> Result<(), AppError> {
        if nonce.is_empty() || nonce.len() > 64 {
            return Err(AppError::Unauthenticated);
        }
        let mut seen = self.seen.lock().map_err(|_| AppError::Unauthenticated)?;
        // Entries whose timestamp fell out of the acceptance window can never
        // be valid again; evict by current time, across every device.
        let window_start = unix_secs().saturating_sub(MAX_CLOCK_SKEW_SECS);
        let mut total = 0usize;
        for queue in seen.values_mut() {
            while queue.front().is_some_and(|(ts, _)| *ts < window_start) {
                queue.pop_front();
            }
            total += queue.len();
        }
        if total >= NONCE_CACHE_LIMIT {
            return Err(AppError::ResourceExhausted(
                "device auth nonce cache is full; retry later".to_owned(),
            ));
        }
        let entries = seen.entry(device_id.to_owned()).or_default();
        if entries.iter().any(|(_, value)| value == nonce) {
            return Err(AppError::Unauthenticated);
        }
        entries.push_back((timestamp, nonce.to_owned()));
        Ok(())
    }
}

/// Axum middleware applied to every authenticated route (HTTP and the WS
/// upgrade). Buffers the body once for the signature hash, verifies the
/// device credential, then marks the request verified for `require_auth`.
pub(crate) async fn device_auth_middleware(
    State(state): State<AppState>,
    request: Request,
    next: Next,
) -> Result<Response, AppError> {
    let (mut parts, body) = request.into_parts();
    // Never trust a client-supplied marker; it only ever comes from us.
    parts
        .headers
        .remove(HeaderName::from_static(VERIFIED_HEADER));
    if !state.config.security.enable_auth {
        return Ok(next.run(Request::from_parts(parts, body)).await);
    }
    let bytes = to_bytes(body, MAX_AUTH_BODY)
        .await
        .map_err(|_| AppError::InvalidRequest("request body is too large".to_owned()))?;
    let auth = state.device_auth.authenticate(
        &parts.method,
        parts.uri.path(),
        parts.uri.query(),
        &parts.headers,
        &bytes,
    )?;
    let mut request = Request::from_parts(parts, Body::from(bytes));
    request.headers_mut().insert(
        HeaderName::from_static(VERIFIED_HEADER),
        axum::http::HeaderValue::from_str(&auth.principal_id)
            .map_err(|_| AppError::Unauthenticated)?,
    );
    Ok(next.run(request).await)
}

/// `require_auth` consults the marker written by the middleware. Handlers keep
/// their existing signature; anonymous deployments synthesize the local
/// principal without a marker.
pub(crate) fn verified_context(
    state: &AppState,
    headers: &HeaderMap,
) -> Result<AuthContext, AppError> {
    if !state.config.security.enable_auth {
        return Ok(AuthContext {
            principal_id: "local".to_owned(),
            tenant_id: "local".to_owned(),
            token_id: "none".to_owned(),
        });
    }
    let device_id = headers
        .get(VERIFIED_HEADER)
        .and_then(|value| value.to_str().ok())
        .filter(|value| !value.is_empty())
        .ok_or(AppError::Unauthenticated)?;
    Ok(AuthContext {
        principal_id: device_id.to_owned(),
        tenant_id: "local".to_owned(),
        token_id: device_id.to_owned(),
    })
}

struct Credential {
    device_id: String,
    timestamp: String,
    nonce: String,
    signature: String,
}

impl Credential {
    /// Headers win; query parameters exist for the WebSocket upgrade and other
    /// clients that cannot set request headers.
    fn extract(headers: &HeaderMap, query: Option<&str>) -> Option<Self> {
        let from_headers = |name: &str, header: &str| {
            headers
                .get(header)
                .and_then(|value| value.to_str().ok())
                .map(str::to_owned)
                .or_else(|| query_value(query, name))
        };
        Some(Self {
            device_id: from_headers(QUERY_DEVICE_ID, HEADER_DEVICE_ID)?,
            timestamp: from_headers(QUERY_TIMESTAMP, HEADER_TIMESTAMP)?,
            nonce: from_headers(QUERY_NONCE, HEADER_NONCE)?,
            signature: from_headers(QUERY_SIGNATURE, HEADER_SIGNATURE)?,
        })
    }
}

fn query_value(query: Option<&str>, key: &str) -> Option<String> {
    form_decode_pairs(query?)
        .into_iter()
        .find(|(name, _)| name == key)
        .map(|(_, value)| value)
}

/// Canonical signed-query form: decode form pairs, drop the auth keys, then
/// re-encode each side so decoded characters cannot forge pair boundaries.
pub(crate) fn canonical_query(query: Option<&str>) -> String {
    let mut pairs: Vec<(String, String)> = form_decode_pairs(query.unwrap_or_default())
        .into_iter()
        .filter(|(key, _)| !AUTH_QUERY_KEYS.contains(&key.as_str()))
        .map(|(key, value)| (strict_encode(&key), strict_encode(&value)))
        .collect();
    pairs.sort();
    pairs
        .iter()
        .map(|(key, value)| format!("{key}={value}"))
        .collect::<Vec<_>>()
        .join("&")
}

fn form_decode_pairs(query: &str) -> Vec<(String, String)> {
    if query.is_empty() {
        return Vec::new();
    }
    query
        .split('&')
        .map(|pair| {
            let (key, value) = pair.split_once('=').unwrap_or((pair, ""));
            (form_decode(key), form_decode(value))
        })
        .collect()
}

/// application/x-www-form-urlencoded decoding: '+' is a space, %XX is a byte.
fn form_decode(value: &str) -> String {
    let bytes = value.as_bytes();
    let mut decoded = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        match bytes[index] {
            b'+' => {
                decoded.push(b' ');
                index += 1;
            }
            b'%' if bytes.get(index + 1).is_some_and(u8::is_ascii_hexdigit)
                && bytes.get(index + 2).is_some_and(u8::is_ascii_hexdigit) =>
            {
                let hex = std::str::from_utf8(&bytes[index + 1..index + 3]).unwrap_or("");
                match u8::from_str_radix(hex, 16) {
                    Ok(byte) => {
                        decoded.push(byte);
                        index += 3;
                    }
                    Err(_) => {
                        decoded.push(bytes[index]);
                        index += 1;
                    }
                }
            }
            byte => {
                decoded.push(byte);
                index += 1;
            }
        }
    }
    String::from_utf8_lossy(&decoded).into_owned()
}

/// RFC 3986 unreserved characters pass through; everything else is %XX.
fn strict_encode(value: &str) -> String {
    let mut encoded = String::with_capacity(value.len());
    for byte in value.as_bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                encoded.push(*byte as char)
            }
            _ => encoded.push_str(&format!("%{byte:02X}")),
        }
    }
    encoded
}

pub(crate) fn signed_payload(
    device_id: &str,
    method: &str,
    path: &str,
    canonical_query: &str,
    timestamp: &str,
    nonce: &str,
    body: &[u8],
) -> Vec<u8> {
    let body_hash = URL_SAFE_NO_PAD.encode(Sha256::digest(body));
    [
        SIGN_DOMAIN,
        device_id.as_bytes(),
        b"\0",
        method.as_bytes(),
        b"\0",
        path.as_bytes(),
        b"\0",
        canonical_query.as_bytes(),
        b"\0",
        timestamp.as_bytes(),
        b"\0",
        nonce.as_bytes(),
        b"\0",
        body_hash.as_bytes(),
    ]
    .concat()
}

fn unix_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

#[cfg(test)]
pub(crate) mod test_support {
    //! Signing helpers shared by server tests; mirrors the client algorithm.
    use super::*;
    use ed25519_dalek::{Signer, SigningKey};
    use rand_core::{OsRng, RngCore};
    use std::path::Path;

    pub(crate) struct TestDevice {
        pub key: SigningKey,
        pub device_id: String,
    }

    impl TestDevice {
        pub(crate) fn new(seed: u8) -> Self {
            let key = SigningKey::from_bytes(&[seed; 32]);
            let device_id = crate::devices::device_id_for(&key.verifying_key().to_bytes());
            Self { key, device_id }
        }

        /// Base64url public key for `devicePublicKey` fields.
        pub(crate) fn public_key_b64(&self) -> String {
            URL_SAFE_NO_PAD.encode(self.key.verifying_key().to_bytes())
        }

        /// Register in a data directory the way an approved pairing would.
        pub(crate) fn enroll(&self, data_dir: &Path) {
            DeviceRegistry::load(data_dir)
                .unwrap()
                .register("test-device", &self.key.verifying_key().to_bytes())
                .unwrap();
        }

        /// `Authorization`-style header set for a test request.
        pub(crate) fn sign(
            &self,
            method: &str,
            uri: &str,
            body: &[u8],
        ) -> [(HeaderName, String); 4] {
            let (path, query) = uri.split_once('?').unwrap_or((uri, ""));
            let mut nonce = [0; 16];
            OsRng.fill_bytes(&mut nonce);
            let timestamp = unix_secs().to_string();
            let nonce = URL_SAFE_NO_PAD.encode(nonce);
            let signature = self.key.sign(&signed_payload(
                &self.device_id,
                method,
                path,
                &canonical_query(Some(query).filter(|q| !q.is_empty())),
                &timestamp,
                &nonce,
                body,
            ));
            [
                (
                    HeaderName::from_static(HEADER_DEVICE_ID),
                    self.device_id.clone(),
                ),
                (HeaderName::from_static(HEADER_TIMESTAMP), timestamp),
                (HeaderName::from_static(HEADER_NONCE), nonce),
                (
                    HeaderName::from_static(HEADER_SIGNATURE),
                    URL_SAFE_NO_PAD.encode(signature.to_bytes()),
                ),
            ]
        }

        /// Same credential as query parameters for the WS handshake.
        pub(crate) fn sign_query(&self, path_and_query: &str) -> String {
            let (path, query) = path_and_query
                .split_once('?')
                .unwrap_or((path_and_query, ""));
            let mut nonce = [0; 16];
            OsRng.fill_bytes(&mut nonce);
            let timestamp = unix_secs().to_string();
            let nonce = URL_SAFE_NO_PAD.encode(nonce);
            let canonical = canonical_query(Some(query).filter(|q| !q.is_empty()));
            let signature = self.key.sign(&signed_payload(
                &self.device_id,
                "GET",
                path,
                &canonical,
                &timestamp,
                &nonce,
                &[],
            ));
            format!(
                "device_id={}&auth_ts={}&auth_nonce={}&auth_sig={}",
                self.device_id,
                timestamp,
                nonce,
                URL_SAFE_NO_PAD.encode(signature.to_bytes())
            )
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ed25519_dalek::Signer;

    fn write_fixture(name: &str, value: &serde_json::Value) {
        if std::env::var_os("TODEX_WRITE_FIXTURES").is_some() {
            let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("tests/fixtures")
                .join(name);
            std::fs::write(&path, serde_json::to_string_pretty(value).unwrap()).unwrap();
        }
    }

    #[test]
    fn canonical_query_is_order_independent_and_auth_params_are_excluded() {
        assert_eq!(canonical_query(None), "");
        assert_eq!(canonical_query(Some("")), "");
        assert_eq!(
            canonical_query(Some("b=2&a=1&device_id=x&auth_sig=y")),
            canonical_query(Some("a=1&b=2"))
        );
        // Re-encoding decoded values keeps pair boundaries unforgeable:
        // `a=b&c=d` (two pairs) differs from `a=b%26c%3Dd` (one pair).
        assert_eq!(canonical_query(Some("a=b%26c%3Dd")), "a=b%26c%3Dd");
        assert_eq!(canonical_query(Some("a=b&c=d")), "a=b&c=d");
        // Form '+' decodes to a space and re-encodes as %20.
        assert_eq!(canonical_query(Some("q=a+b")), "q=a%20b");
    }

    #[test]
    fn cross_language_auth_vector() {
        // tests/fixtures/device-auth-v1.json pins this derivation for the TS
        // and Swift clients.
        let key = ed25519_dalek::SigningKey::from_bytes(&[21; 32]);
        let device_id = crate::devices::device_id_for(&key.verifying_key().to_bytes());
        let payload = signed_payload(
            &device_id,
            "POST",
            "/v2/conversations",
            &canonical_query(Some("auth_sig=ignored&workspacePath=%2Ftmp%2Fx&a=1")),
            "1700000000",
            "AAECAwQFBgcICQohAg0ODxA",
            b"{\"prompt\":\"hi\"}",
        );
        let signature = key.sign(&payload);
        let vector = serde_json::json!({
            "deviceSeed": URL_SAFE_NO_PAD.encode([21; 32]),
            "devicePublicKey": URL_SAFE_NO_PAD.encode(key.verifying_key().to_bytes()),
            "deviceId": device_id,
            "method": "POST",
            "path": "/v2/conversations",
            "rawQuery": "auth_sig=ignored&workspacePath=%2Ftmp%2Fx&a=1",
            "canonicalQuery": canonical_query(Some("auth_sig=ignored&workspacePath=%2Ftmp%2Fx&a=1")),
            "timestamp": "1700000000",
            "nonce": "AAECAwQFBgcICQohAg0ODxA",
            "body": "{\"prompt\":\"hi\"}",
            "payload": URL_SAFE_NO_PAD.encode(&payload),
            "signature": URL_SAFE_NO_PAD.encode(signature.to_bytes()),
        });
        write_fixture("device-auth-v1.json", &vector);
        let expected: serde_json::Value =
            serde_json::from_str(include_str!("../tests/fixtures/device-auth-v1.json")).unwrap();
        assert_eq!(vector, expected);
    }

    #[test]
    fn nonce_replay_is_rejected() {
        let auth = DeviceAuthenticator::new(
            DeviceRegistry::load(
                &std::env::temp_dir().join(format!("todex-auth-{}", uuid::Uuid::new_v4())),
            )
            .unwrap(),
        );
        assert!(auth.claim_nonce("dev_a", unix_secs(), "n1").is_ok());
        assert!(auth.claim_nonce("dev_a", unix_secs(), "n1").is_err());
        assert!(auth.claim_nonce("dev_b", unix_secs(), "n1").is_ok());
        assert!(auth
            .claim_nonce("dev_a", unix_secs() - MAX_CLOCK_SKEW_SECS, "n2")
            .is_ok());
    }
}
