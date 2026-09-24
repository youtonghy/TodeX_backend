mod agent_providers;
mod device_pairing;
mod git;
pub mod protocol;
mod routes;
mod v2;
pub(crate) mod websocket;

use std::net::IpAddr;

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

fn cors_layer(host: &str) -> CorsLayer {
    if is_loopback_host(host) {
        CorsLayer::permissive().allow_private_network(true)
    } else {
        CorsLayer::permissive()
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
    use super::is_loopback_host;

    #[test]
    fn recognizes_loopback_hosts_for_private_network_cors() {
        assert!(is_loopback_host("127.0.0.1"));
        assert!(is_loopback_host("[::1]"));
        assert!(is_loopback_host("localhost"));
        assert!(!is_loopback_host("0.0.0.0"));
        assert!(!is_loopback_host("192.168.1.20"));
    }
}
