use axum::routing::get;
use axum::Router;

use crate::app_state::AppState;

/// Version-independent liveness probe. Never part of a versioned API surface;
/// everything else lives under `/v2` (see `v2::routes`).
pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/health", get(health))
        .merge(super::v2::routes())
}

async fn health() -> &'static str {
    "ok"
}
