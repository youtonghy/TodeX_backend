mod agent_calling;
mod agent_desktop;
mod agent_providers;
pub(crate) mod api;
mod api_keys;
mod browser_proxy;
mod cli_instructions;
mod cli_settings;
mod device_pairing;
mod dsh_plugins;
mod enforcement;
mod git;
mod history_cleanup;
mod history_keys;
mod local_admin;
mod native_history;
mod pi_mcp;
mod pi_plugins;
pub mod protocol;
mod quota;
mod remote;
mod routes;
mod sealed;
mod ssh;
#[cfg(test)]
mod transport_v2_tests;
mod uploads;
mod v2;
pub(crate) mod websocket;
mod ws;

pub(crate) use api::api_router;
pub(crate) use history_keys::spawn_history_watch;
pub(crate) use local_admin::LocalAdminInfo;

use std::time::Duration;

use axum::Router;
use std::sync::Arc;
use tower_http::compression::predicate::{NotForContentType, Predicate, SizeAbove};
use tower_http::compression::CompressionLayer;

use axum::http::HeaderValue;
use tower_http::cors::{AllowOrigin, AllowPrivateNetwork, Any, CorsLayer};
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
        .layer(cors_layer(
            &state.config.host,
            &state.config.security.cors_origins,
        ))
        .layer(TraceLayer::new_for_http().make_span_with(request_span::<axum::body::Body>))
}

/// The main listener's router with the network off (`network_enabled =
/// false`): only the agent MCP routes, which agents reach over loopback,
/// and the unauthenticated `/v2/version` the daemon health check polls.
/// Clients use the IPC listener.
pub fn agent_router(state: AppState) -> Router {
    Router::new()
        .route("/v2/version", axum::routing::get(v2::version))
        .merge(crate::agent_mcp::routes(&state))
        .with_state(state)
        .layer(TraceLayer::new_for_http().make_span_with(request_span::<axum::body::Body>))
}

/// The IPC listener's router: [`router`] plus the local admin API, which no
/// TCP listener serves.
pub fn ipc_router(state: AppState, info: LocalAdminInfo) -> Router {
    router(state.clone()).merge(local_admin::routes(state, info))
}

/// The request span: method, path and version. The query string is left out
/// because signed requests carry `auth_sig`/`auth_nonce` (and other
/// credentials) there.
fn request_span<B>(request: &axum::http::Request<B>) -> tracing::Span {
    tracing::debug_span!(
        "request",
        method = %request.method(),
        path = %request.uri().path(),
        version = ?request.version(),
    )
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

/// Pages that may read the daemon's responses: the desktop app, any
/// loopback page (a local web client or dev server), and `configured`
/// (`security.cors_origins`). Other pages still reach the daemon with simple
/// requests, as browsers allow, but never read the answers; everything
/// signed or JSON (pairing included) needs a preflight, which they fail.
fn cors_origin_allowed(origin: &HeaderValue, configured: &[String]) -> bool {
    origin.to_str().is_ok_and(|origin| {
        crate::device_auth::is_local_origin(origin)
            || configured.iter().any(|allowed| allowed == origin)
    })
}

fn cors_layer(host: &str, configured: &[String]) -> CorsLayer {
    let configured: Arc<[String]> = configured.into();
    let origins = Arc::clone(&configured);
    let layer = CorsLayer::new()
        .allow_headers(Any)
        .allow_methods(Any)
        .expose_headers(Any)
        .allow_origin(AllowOrigin::predicate(move |origin, _| {
            cors_origin_allowed(origin, &origins)
        }))
        .max_age(CORS_PREFLIGHT_MAX_AGE);
    if is_loopback_host(host) {
        layer.allow_private_network(AllowPrivateNetwork::predicate(move |origin, _| {
            cors_origin_allowed(origin, &configured)
        }))
    } else {
        layer
    }
}

#[cfg(test)]
mod tests {
    use super::{cors_layer, request_span};
    use axum::{body::Body, http::Request, routing::get, Router};
    use std::sync::{Arc, Mutex};
    use tower::ServiceExt;
    use tower_http::trace::TraceLayer;

    #[derive(Clone, Default)]
    struct Captured(Arc<Mutex<Vec<u8>>>);

    impl std::io::Write for Captured {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(bytes);
            Ok(bytes.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    #[tokio::test]
    async fn request_spans_log_the_path_without_the_query() {
        let captured = Captured::default();
        let writer = captured.clone();
        let subscriber = tracing_subscriber::fmt()
            .with_max_level(tracing::Level::DEBUG)
            .with_ansi(false)
            .with_writer(move || writer.clone())
            .finish();
        let _guard = tracing::subscriber::set_default(subscriber);
        let app = Router::new()
            .route("/v2/events", get(|| async { "ok" }))
            .layer(TraceLayer::new_for_http().make_span_with(request_span::<Body>));
        let mut logged = String::new();
        // Another test thread may register these callsites while no
        // subscriber wants them, racing this one's registration and leaving
        // them cached as disabled; recompute and retry until they log.
        for _ in 0..5 {
            tracing::callsite::rebuild_interest_cache();
            app.clone()
                .oneshot(
                    Request::get("/v2/events?auth_sig=SECRET-SIG&auth_nonce=SECRET-NONCE")
                        .body(Body::empty())
                        .unwrap(),
                )
                .await
                .unwrap();
            logged = String::from_utf8(captured.0.lock().unwrap().clone()).unwrap();
            if logged.contains("path=") {
                break;
            }
        }
        assert!(logged.contains("path=/v2/events"), "{logged}");
        assert!(!logged.contains("SECRET"), "{logged}");
    }

    #[tokio::test]
    async fn only_the_desktop_app_loopback_and_configured_origins_pass_cors() {
        let app = Router::new()
            .route("/v2/device-pairing/create", get(|| async { "ok" }))
            .layer(cors_layer(
                "127.0.0.1",
                &["https://todex.example.com".to_owned()],
            ));
        let preflight = |origin: &'static str| {
            let app = app.clone();
            async move {
                app.oneshot(
                    Request::options("/v2/device-pairing/create")
                        .header("origin", origin)
                        .header("access-control-request-method", "POST")
                        .header("access-control-request-headers", "content-type")
                        .header("access-control-request-private-network", "true")
                        .body(Body::empty())
                        .unwrap(),
                )
                .await
                .unwrap()
            }
        };
        for allowed in [
            "todex-desktop://app",
            "http://127.0.0.1:5173",
            "http://localhost:4173",
            "https://todex.example.com",
        ] {
            let response = preflight(allowed).await;
            assert_eq!(
                response.headers()["access-control-allow-origin"],
                allowed,
                "{allowed}"
            );
            assert_eq!(
                response.headers()["access-control-allow-private-network"],
                "true"
            );
        }
        for refused in [
            "https://evil.example",
            "null",
            "http://127.0.0.1.evil.example",
            "https://todex.example.com.evil.example",
        ] {
            let response = preflight(refused).await;
            assert!(
                !response
                    .headers()
                    .contains_key("access-control-allow-origin"),
                "{refused}"
            );
            assert!(!response
                .headers()
                .contains_key("access-control-allow-private-network"));
        }
    }

    #[tokio::test]
    async fn signed_request_preflights_are_cacheable() {
        let app = Router::new()
            .route("/v2/providers/models", get(|| async { "ok" }))
            .layer(cors_layer("127.0.0.1", &[]));
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
