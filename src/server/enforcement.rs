//! Transport v2 enforcement (`docs/transport-v2.md`, "Enforcement"): a
//! non-loopback peer reaches the API only through transport v2. Directly it
//! may only use the bootstrap routes, the `/v2/sealed` tunnel and a `tv=2`
//! WebSocket upgrade; everything else answers `426 PROTOCOL_UPGRADE_REQUIRED`.
//! Loopback peers keep plaintext access, unless a reverse proxy forwarded
//! the request (`Forwarded`, `X-Forwarded-For` or `X-Real-IP`).
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
    // and is treated as remote; IPC peers count as loopback.
    let peer = super::sealed::peer_address(request.extensions());
    // A reverse proxy on this machine forwards remote clients over loopback.
    let loopback = peer.is_some_and(crate::ipc::PeerAddr::is_loopback)
        && crate::listen_addrs::forwarded(
            request.headers(),
            request.extensions(),
            peer.and_then(crate::ipc::PeerAddr::ip),
            &[],
        )
        .is_none();
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

    #[tokio::test]
    async fn requests_a_local_proxy_forwarded_are_not_loopback() {
        use axum::{body::Body, http::Request, http::StatusCode};
        use tower::ServiceExt;

        let root = std::env::temp_dir().join(format!("todex-enforce-{}", uuid::Uuid::new_v4()));
        let config = crate::config::Config {
            data_dir: root.join("data"),
            workspace_roots: vec![root.join("workspaces")],
            ..crate::config::Config::default()
        };
        let state = crate::app_state::AppState::new_for_tests(config)
            .await
            .unwrap();
        let app = super::super::loopback_test_router(state);
        let status = |forwarded: Option<(&'static str, &'static str)>| {
            let mut request = Request::get("/v2/workspaces");
            if let Some((name, value)) = forwarded {
                request = request.header(name, value);
            }
            let app = app.clone();
            async move {
                app.oneshot(request.body(Body::empty()).unwrap())
                    .await
                    .unwrap()
                    .status()
            }
        };
        // Plain loopback reaches device auth (and fails it without a key).
        assert_eq!(status(None).await, StatusCode::UNAUTHORIZED);
        for header in [
            ("x-forwarded-for", "203.0.113.7"),
            ("forwarded", "for=203.0.113.7"),
            ("x-real-ip", "203.0.113.7"),
        ] {
            assert_eq!(
                status(Some(header)).await,
                StatusCode::UPGRADE_REQUIRED,
                "{header:?}"
            );
        }
        let _ = std::fs::remove_dir_all(root);
    }

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
