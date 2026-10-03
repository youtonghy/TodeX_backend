//! `/v2/providers/quota` — account-level plan quota snapshots. Codex is
//! refreshed on demand through an ephemeral app-server; Claude Code reports
//! only during turns, so its entry is whatever `quota.updated` last recorded.

use axum::extract::State;
use axum::http::HeaderMap;
use axum::routing::get;
use axum::{Json, Router};
use serde_json::{json, Value};

use crate::app_state::AppState;
use crate::error::AppError;
use crate::provider::process::executable_available;

use super::v2::require_auth;

pub(super) fn routes() -> Router<AppState> {
    Router::new().route("/v2/providers/quota", get(provider_quota))
}

async fn provider_quota(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<Json<Value>, AppError> {
    require_auth(&state, &headers)?;
    let mut providers = serde_json::Map::new();

    let codex = match fetch_codex(&state).await {
        Ok(quota) => {
            state.quota.record(&quota);
            quota
        }
        Err(reason) => state.quota.get("codex").unwrap_or_else(
            || json!({"provider":"codex","scope":"account","state":"unavailable","reason":reason}),
        ),
    };
    providers.insert("codex".to_owned(), codex);

    let claude = state.quota.get("claude-code").unwrap_or_else(|| {
        json!({"provider":"claude-code","scope":"account","state":"idle","reason":"Claude Code reports plan limits during a turn; run a session first."})
    });
    providers.insert("claude-code".to_owned(), claude);

    // Devin's ACP server advertises no plan-quota surface today (verified
    // against devin 3000.11.3: no `billingInformation` push on initialize,
    // session/new, or a full turn, local or cloud relay).
    providers.insert(
        "devin".to_owned(),
        json!({"provider":"devin","scope":"account","state":"unsupported"}),
    );

    Ok(Json(json!({ "providers": providers })))
}

async fn fetch_codex(state: &AppState) -> Result<Value, String> {
    let binary = &state.config.agent.codex_bin;
    if !executable_available(binary) {
        return Err(format!("executable '{binary}' was not found"));
    }
    crate::provider::codex::fetch_codex_quota(binary, &state.config.data_dir)
        .await
        .map_err(|error| error.to_string())
}
