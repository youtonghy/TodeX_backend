mod agent_desktop;
mod agent_providers;
pub(crate) mod api;
mod api_keys;
mod device_pairing;
mod enforcement;
mod git;
mod history_keys;
pub mod protocol;
mod quota;
mod remote;
mod routes;
mod sealed;
mod ssh;
#[cfg(test)]
mod transport_v2_tests;
mod v2;
pub(crate) mod websocket;
mod ws;

pub(crate) use api::api_router;
pub(crate) use history_keys::spawn_history_watch;

use std::time::Duration;

use axum::Router;
use tower_http::compression::predicate::{NotForContentType, Predicate, SizeAbove};
use tower_http::compression::CompressionLayer;
use tower_http::cors::CorsLayer;
use tower_http::trace::TraceLayer;

use crate::app_state::AppState;
use crate::listen_addrs::is_loopback_host;

pub fn router(state: AppState) -> Router {
    let api: Router = Router::new()
        .merge(routes::routes(&state))
        .merge(device_pairing::routes())
        // Loopback + per-conversation token; deliberately outside device auth.
        .merge(crate::agent_mcp::routes(&state))
        .with_state(state.clone());
    // `/v2/sealed` runs inner requests through `api`, which does not contain
    // the tunnel itself, so nesting is impossible.
    api.clone()
        .merge(sealed::routes(api, state.clone()))
        // Inside CORS, so browsers can read the 426.
        .layer(axum::middleware::from_fn(enforcement::enforce_transport_v2))
        .layer(compression_layer())
        .layer(cors_layer(&state.config.host))
        .layer(TraceLayer::new_for_http())
}

/// The router as in-process tests drive it with `oneshot`: without a served
/// connection there is no peer address, which the transport enforcement
/// treats as remote, so these requests come from a mocked loopback peer
/// (an explicit `ConnectInfo` extension still wins).
#[cfg(test)]
pub(crate) fn loopback_test_router(state: AppState) -> Router {
    router(state).layer(axum::extract::connect_info::MockConnectInfo(
        std::net::SocketAddr::from(([127, 0, 0, 1], 0)),
    ))
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
            .and(NotForContentType::SSE)
            // Remote file downloads: arbitrary bytes with an exact length.
            .and(NotForContentType::const_new("application/octet-stream"))
            // Transport v2 tunnel: ciphertext does not compress, and the
            // records must stream as they are sealed.
            .and(NotForContentType::const_new(
                crate::transport_crypto::envelope::SEALED_CONTENT_TYPE,
            )),
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

#[cfg(test)]
mod tests {
    use super::cors_layer;
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
}
