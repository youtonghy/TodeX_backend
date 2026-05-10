pub mod protocol;
mod routes;
mod websocket;

use axum::Router;
use tower_http::cors::CorsLayer;
use tower_http::trace::TraceLayer;

use crate::app_state::AppState;

pub fn router(state: AppState) -> Router {
    Router::new()
        .merge(routes::routes())
        .layer(CorsLayer::permissive())
        .layer(TraceLayer::new_for_http())
        .with_state(state)
}
