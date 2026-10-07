//! Transport v2 enforcement (`docs/transport-v2.md`, "Enforcement"): a
//! non-loopback peer reaches the API only through transport v2. Directly it
//! may only use the bootstrap routes, the `/v2/sealed` tunnel and a `tv=2`
//! WebSocket upgrade; everything else answers `426 PROTOCOL_UPGRADE_REQUIRED`.
//! Loopback peers keep plaintext access.
use axum::extract::Request;
use axum::middleware::Next;
use axum::response::Response;

use super::sealed::ArrivedViaTransportV2;
use crate::error::AppError;
use crate::transport_crypto::envelope::SEALED_PATH;
use crate::transport_crypto::query_value;

/// Routes a non-loopback peer may call without the tunnel. Pairing stays
/// direct: clients pair before they have a pinned key.
fn is_direct_route(path: &str, query: Option<&str>) -> bool {
    matches!(
        path,
        "/health" | "/v2/transport-policy" | "/v2/version" | SEALED_PATH
    ) || path.starts_with("/v2/device-pairing/")
        || (path == "/v2/ws" && query_value(query, "tv").as_deref() == Some("2"))
}

pub(super) async fn enforce_transport_v2(
    request: Request,
    next: Next,
) -> Result<Response, AppError> {
    // `ConnectInfo` is installed by `into_make_service_with_connect_info`
    // for every served connection. A request without it has no known peer
    // and is treated as remote.
    let loopback = super::sealed::peer_address(request.extensions())
        .is_some_and(|peer| peer.ip().to_canonical().is_loopback());
    if loopback
        || request
            .extensions()
            .get::<ArrivedViaTransportV2>()
            .is_some()
        || is_direct_route(request.uri().path(), request.uri().query())
    {
        return Ok(next.run(request).await);
    }
    Err(AppError::ProtocolUpgradeRequired(
        "non-loopback clients must use transport v2 (POST /v2/sealed, /v2/ws?tv=2)".to_owned(),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn direct_routes_are_the_bootstrap_set() {
        for (path, query) in [
            ("/health", None),
            ("/v2/transport-policy", None),
            ("/v2/version", None),
            ("/v2/sealed", None),
            ("/v2/device-pairing/create", None),
            ("/v2/device-pairing/reveal", None),
            ("/v2/ws", Some("tv=2&enc=x25519")),
        ] {
            assert!(is_direct_route(path, query), "{path}");
        }
        for (path, query) in [
            ("/v2/workspaces", None),
            ("/v2/ws", None),
            ("/v2/ws", Some("enc=x25519")),
            ("/v2/ws", Some("tv=1")),
            ("/v2/sealed/x", None),
            ("/v2/device-pairing", None),
            ("/v2/healthz", None),
        ] {
            assert!(!is_direct_route(path, query), "{path}");
        }
    }
}
