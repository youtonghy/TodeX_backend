//! `/v2/providers/pi/mcp` — Pi's native MCP servers (see [`crate::pi_mcp`]).
//! Writes need a signed device: a server entry is a command Pi runs on this
//! host, or a URL it sends credentials to.

use axum::extract::State;
use axum::http::HeaderMap;
use axum::routing::get;
use axum::{Json, Router};
use serde::Deserialize;
use serde_json::{Map, Value};

use crate::agent_providers::AgentDirs;
use crate::app_state::AppState;
use crate::error::AppError;
use crate::pi_mcp::{self, PiMcpDocument};

use super::agent_providers::require_signed_device;
use super::v2::require_auth;

pub(super) fn routes() -> Router<AppState> {
    Router::new().route("/v2/providers/pi/mcp", get(read).patch(patch))
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct PatchRequest {
    revision: String,
    patch: Map<String, Value>,
}

async fn read(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<Json<PiMcpDocument>, AppError> {
    require_auth(&state, &headers)?;
    blocking(|| pi_mcp::read(&AgentDirs::detect())).await
}

async fn patch(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(body): Json<Value>,
) -> Result<Json<PiMcpDocument>, AppError> {
    require_signed_device(&state, &headers, "editing Pi MCP servers")?;
    let request: PatchRequest = serde_json::from_value(body)
        .map_err(|error| AppError::InvalidRequest(format!("invalid MCP patch: {error}")))?;
    blocking(move || pi_mcp::patch(&AgentDirs::detect(), &request.revision, &request.patch)).await
}

async fn blocking<T: Send + 'static>(
    work: impl FnOnce() -> Result<T, AppError> + Send + 'static,
) -> Result<Json<T>, AppError> {
    tokio::task::spawn_blocking(work)
        .await
        .map_err(|error| AppError::Anyhow(error.into()))?
        .map(Json)
}
