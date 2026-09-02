mod git;
pub mod protocol;
mod routes;
mod v2;
mod websocket;

use std::net::IpAddr;

use axum::Router;
use tower_http::cors::CorsLayer;
use tower_http::trace::TraceLayer;

use crate::app_state::AppState;

pub fn router(state: AppState) -> Router {
    Router::new()
        .merge(routes::routes())
        .layer(cors_layer(&state.config.host))
        .layer(TraceLayer::new_for_http())
        .with_state(state)
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
