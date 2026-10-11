//! `/v2/history/*` — conversation history storage usage, batch cleanup and
//! the per-tenant automatic cleanup policy (see [`crate::history_cleanup`]).
//! Every route acts on the authenticated tenant's conversations only.

use axum::extract::State;
use axum::http::HeaderMap;
use axum::routing::{get, post};
use axum::{Json, Router};
use chrono::Utc;
use serde::Deserialize;
use serde_json::{json, Value};

use crate::app_state::AppState;
use crate::error::AppError;
use crate::history_cleanup::{run_policy, CleanupPolicy, CleanupTrigger};

use super::v2::require_auth;

/// Most conversations one batch cleanup request may name.
const MAX_CLEANUP_IDS: usize = 500;

pub(super) fn routes() -> Router<AppState> {
    Router::new()
        .route("/v2/history/storage", get(history_storage))
        .route("/v2/history/cleanup", post(history_cleanup))
        .route(
            "/v2/history/cleanup-policy",
            get(cleanup_policy).put(update_cleanup_policy),
        )
        .route("/v2/history/cleanup-policy/run", post(run_cleanup_policy))
}

async fn history_storage(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<Json<Value>, AppError> {
    let auth = require_auth(&state, &headers)?;
    let conversations = state.conversations.storage_owned(&auth.tenant_id).await?;
    let total_bytes = conversations
        .iter()
        .map(|usage| usage.bytes)
        .fold(0u64, u64::saturating_add);
    Ok(Json(json!({
        "totalBytes": total_bytes,
        "count": conversations.len(),
        "measuredAt": Utc::now(),
        "conversations": conversations,
    })))
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct CleanupRequest {
    conversation_ids: Vec<String>,
}

async fn history_cleanup(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(body): Json<Value>,
) -> Result<Json<Value>, AppError> {
    let auth = require_auth(&state, &headers)?;
    let request: CleanupRequest = serde_json::from_value(body)
        .map_err(|error| AppError::InvalidRequest(format!("invalid cleanup request: {error}")))?;
    if request.conversation_ids.is_empty() || request.conversation_ids.len() > MAX_CLEANUP_IDS {
        return Err(AppError::InvalidRequest(format!(
            "conversationIds must name between 1 and {MAX_CLEANUP_IDS} conversations"
        )));
    }
    let report = state
        .conversations
        .cleanup_owned(&auth.tenant_id, &request.conversation_ids)
        .await;
    Ok(Json(serde_json::to_value(report)?))
}

async fn cleanup_policy(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<Json<Value>, AppError> {
    let auth = require_auth(&state, &headers)?;
    let tenant = state.history_cleanup.tenant(&auth.tenant_id).await;
    Ok(Json(state.history_cleanup.view(&tenant)))
}

async fn update_cleanup_policy(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(body): Json<Value>,
) -> Result<Json<Value>, AppError> {
    let auth = require_auth(&state, &headers)?;
    let policy = CleanupPolicy::from_request(body)?;
    let tenant = state
        .history_cleanup
        .set_policy(&auth.tenant_id, policy)
        .await?;
    Ok(Json(state.history_cleanup.view(&tenant)))
}

async fn run_cleanup_policy(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<Json<Value>, AppError> {
    let auth = require_auth(&state, &headers)?;
    let policy = state.history_cleanup.tenant(&auth.tenant_id).await.policy;
    if !policy.has_threshold() {
        return Err(AppError::InvalidRequest(
            "the cleanup policy sets neither maxAgeDays nor maxTotalBytes".to_owned(),
        ));
    }
    let (report, run) = run_policy(
        &state.conversations,
        &state.history_cleanup,
        &auth.tenant_id,
        policy,
        CleanupTrigger::Manual,
    )
    .await?;
    Ok(Json(json!({
        "deleted": report.deleted,
        "skipped": report.skipped,
        "freedBytes": report.freed_bytes,
        "lastRun": run,
    })))
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use axum::body::Body;
    use axum::http::{Request, StatusCode};
    use tower::ServiceExt;

    use crate::config::Config;
    use crate::conversation::{ConversationManifest, ProviderKind};

    use super::*;

    async fn call(
        app: &Router,
        method: &str,
        uri: &str,
        body: Option<Value>,
    ) -> (StatusCode, Value) {
        let request = Request::builder()
            .method(method)
            .uri(uri)
            .header("host", "127.0.0.1:7345")
            .header("content-type", "application/json");
        let request = match body {
            Some(body) => request.body(Body::from(body.to_string())),
            None => request.body(Body::empty()),
        }
        .unwrap();
        let response = app.clone().oneshot(request).await.unwrap();
        let status = response.status();
        let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        (
            status,
            serde_json::from_slice(&bytes).unwrap_or(Value::Null),
        )
    }

    #[tokio::test]
    async fn history_routes_follow_the_contract() {
        let root =
            std::env::temp_dir().join(format!("todex-history-routes-{}", uuid::Uuid::new_v4()));
        let workspace = root.join("workspaces").join("project");
        std::fs::create_dir_all(&workspace).unwrap();
        let mut state = AppState::new_for_tests(Config {
            port: 0,
            data_dir: root.join("data"),
            workspace_roots: vec![root.join("workspaces")],
            ..Config::default()
        })
        .await
        .unwrap();
        Arc::make_mut(&mut state.config).security.enable_auth = false;
        let manifest = state
            .conversation_store()
            .create(ConversationManifest::new(
                ProviderKind::Codex,
                workspace,
                None,
                None,
            ))
            .await
            .unwrap();
        let app: Router = crate::server::loopback_test_router(state);

        let (status, storage) = call(&app, "GET", "/v2/history/storage", None).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(storage["count"], 1);
        assert_eq!(storage["conversations"][0]["id"], manifest.id);
        assert!(storage["conversations"][0]["bytes"].as_u64().unwrap() > 0);
        assert_eq!(storage["totalBytes"], storage["conversations"][0]["bytes"]);
        assert!(storage["measuredAt"].is_string());

        let (status, policy) = call(&app, "GET", "/v2/history/cleanup-policy", None).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(
            policy,
            json!({
                "policy": {"enabled": false, "maxAgeDays": null, "maxTotalBytes": null},
                "lastRun": null,
                "nextRunAt": null,
            })
        );
        let (status, error) = call(&app, "POST", "/v2/history/cleanup-policy/run", None).await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert_eq!(error["code"], "INVALID_REQUEST");
        let (status, error) = call(
            &app,
            "PUT",
            "/v2/history/cleanup-policy",
            Some(json!({"enabled": true, "maxAgeDays": null, "maxTotalBytes": null})),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert_eq!(error["code"], "INVALID_REQUEST");
        let (status, saved) = call(
            &app,
            "PUT",
            "/v2/history/cleanup-policy",
            Some(json!({"enabled": false, "maxAgeDays": 30, "maxTotalBytes": null})),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(saved["policy"]["maxAgeDays"], 30);
        assert_eq!(saved["nextRunAt"], Value::Null);
        // The fresh conversation is younger than 30 days: nothing to remove.
        let (status, run) = call(&app, "POST", "/v2/history/cleanup-policy/run", None).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(run["deleted"], json!([]));
        assert_eq!(run["skipped"], json!([]));
        assert_eq!(run["freedBytes"], 0);
        assert_eq!(run["lastRun"]["trigger"], "manual");
        assert_eq!(run["lastRun"]["deleted"], 0);
        assert_eq!(run["lastRun"]["error"], Value::Null);
        let (_, policy) = call(&app, "GET", "/v2/history/cleanup-policy", None).await;
        assert_eq!(policy["lastRun"], run["lastRun"]);

        for body in [json!({"conversationIds": []}), json!({})] {
            let (status, error) = call(&app, "POST", "/v2/history/cleanup", Some(body)).await;
            assert_eq!(status, StatusCode::BAD_REQUEST);
            assert_eq!(error["code"], "INVALID_REQUEST");
        }
        let too_many: Vec<String> = (0..501).map(|_| uuid::Uuid::new_v4().to_string()).collect();
        let (status, _) = call(
            &app,
            "POST",
            "/v2/history/cleanup",
            Some(json!({"conversationIds": too_many})),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        let missing = uuid::Uuid::new_v4().to_string();
        let (status, cleanup) = call(
            &app,
            "POST",
            "/v2/history/cleanup",
            Some(json!({"conversationIds": [manifest.id, missing]})),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(cleanup["deleted"], json!([manifest.id]));
        assert_eq!(cleanup["skipped"][0]["conversationId"], missing);
        assert_eq!(cleanup["skipped"][0]["code"], "NOT_FOUND");
        assert!(cleanup["skipped"][0]["message"].is_string());
        assert_eq!(cleanup["freedBytes"], storage["totalBytes"]);
        let (_, storage) = call(&app, "GET", "/v2/history/storage", None).await;
        assert_eq!(storage["count"], 0);
        assert_eq!(storage["totalBytes"], 0);
        let _ = std::fs::remove_dir_all(root);
    }
}
