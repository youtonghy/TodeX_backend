mod agent_providers;
mod device_pairing;
mod git;
pub mod protocol;
mod routes;
mod ssh;
mod v2;
pub(crate) mod websocket;

use std::net::IpAddr;
use std::time::Duration;

use axum::Router;
use tower_http::compression::predicate::{NotForContentType, Predicate, SizeAbove};
use tower_http::compression::CompressionLayer;
use tower_http::cors::CorsLayer;
use tower_http::trace::TraceLayer;

use crate::app_state::AppState;

pub fn router(state: AppState) -> Router {
    Router::new()
        .merge(routes::routes(&state))
        .merge(device_pairing::routes())
        // Loopback + per-conversation token; deliberately outside device auth.
        .merge(crate::agent_mcp::routes(&state))
        .layer(compression_layer())
        .layer(cors_layer(&state.config.host))
        .layer(TraceLayer::new_for_http())
        .with_state(state)
}

/// Responses below this size are sent as-is; gzip framing would not pay off.
const MIN_COMPRESSED_RESPONSE_BYTES: u16 = 1024;

/// Gzip for clients that send `Accept-Encoding: gzip` (mainly large history
/// pages). Only response bodies are touched, so device signatures, which
/// cover the request, are unaffected. Bodyless responses such as the
/// websocket `101 Switching Protocols` fall below the size threshold, and
/// images, gRPC and event streams keep the default exclusions.
fn compression_layer() -> CompressionLayer<impl Predicate> {
    CompressionLayer::new().compress_when(
        SizeAbove::new(MIN_COMPRESSED_RESPONSE_BYTES)
            .and(NotForContentType::GRPC)
            .and(NotForContentType::IMAGES)
            .and(NotForContentType::SSE),
    )
}

/// Clients sign every request with custom `x-todex-*` headers, so each
/// cross-origin call needs a preflight. Without a max-age browsers re-send the
/// preflight almost every time (Chromium keeps it for 5s), doubling the load on
/// their six-connection-per-host pool. Chromium caps the cache at two hours.
const CORS_PREFLIGHT_MAX_AGE: Duration = Duration::from_secs(2 * 60 * 60);

fn cors_layer(host: &str) -> CorsLayer {
    let layer = CorsLayer::permissive().max_age(CORS_PREFLIGHT_MAX_AGE);
    if is_loopback_host(host) {
        layer.allow_private_network(true)
    } else {
        layer
    }
}

fn is_loopback_host(host: &str) -> bool {
    let normalized = host.trim().trim_matches(['[', ']']);
    normalized.eq_ignore_ascii_case("localhost")
        || normalized
            .parse::<IpAddr>()
            .map(|address| address.is_loopback())
            .unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::{cors_layer, is_loopback_host};
    use axum::{body::Body, http::Request, routing::get, Router};
    use tower::ServiceExt;

    #[tokio::test]
    async fn signed_request_preflights_are_cacheable() {
        let app = Router::new()
            .route("/v2/providers/models", get(|| async { "ok" }))
            .layer(cors_layer("127.0.0.1"));
        let response = app
            .oneshot(
                Request::options("/v2/providers/models")
                    .header("origin", "http://localhost:5173")
                    .header("access-control-request-method", "GET")
                    .header("access-control-request-headers", "x-todex-auth-sig")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(
            response.headers()["access-control-max-age"],
            "7200",
            "preflights must not be re-sent for every signed request"
        );
    }

    #[test]
    fn recognizes_loopback_hosts_for_private_network_cors() {
        assert!(is_loopback_host("127.0.0.1"));
        assert!(is_loopback_host("[::1]"));
        assert!(is_loopback_host("localhost"));
        assert!(!is_loopback_host("0.0.0.0"));
        assert!(!is_loopback_host("192.168.1.20"));
    }
}
