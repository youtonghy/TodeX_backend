use axum::routing::get;
use axum::Router;

use crate::app_state::AppState;

/// Version-independent liveness probe. Never part of a versioned API surface;
/// everything else lives under `/v2` (see `v2::routes`).
pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/health", get(health))
        .route("/v2/transport-policy", get(transport_policy))
        .merge(super::v2::routes())
}

async fn health() -> &'static str {
    "ok"
}

async fn transport_policy(
    axum::extract::State(state): axum::extract::State<AppState>,
) -> (
    [(axum::http::header::HeaderName, &'static str); 1],
    axum::Json<serde_json::Value>,
) {
    (
        [(axum::http::header::CACHE_CONTROL, "no-store")],
        axum::Json(serde_json::json!({
            "requiredProtocol": state.config.pairing_encryption.as_str()
        })),
    )
}
