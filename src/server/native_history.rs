//! `/v2/native-history/*` — Claude Code, Pi, Codex, Grok Build and Devin
//! native sessions: listing, transcripts, deletion, the per-provider
//! automatic cleanup policy and Claude's own `cleanupPeriodDays` (see
//! [`crate::native_history`]). `{provider}` is `claude-code`, `pi`, `codex`,
//! `grok-build` or `devin`; anything else is `400 INVALID_REQUEST`.

use std::collections::{HashMap, HashSet};

use axum::extract::{Path, Query, State};
use axum::http::HeaderMap;
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::Deserialize;
use serde_json::{json, Value};

use crate::app_state::AppState;
use crate::error::AppError;
use crate::history_cleanup::{CleanupPolicy, CleanupTrigger};
use crate::native_history::{
    run_policy, validate_session_id, NativeProvider, DEFAULT_TRANSCRIPT_LIMIT, MAX_TRANSCRIPT_LIMIT,
};

use super::v2::require_auth;

/// Most sessions one cleanup request may name.
const MAX_CLEANUP_IDS: usize = 500;

pub(super) fn routes() -> Router<AppState> {
    Router::new()
        .route("/v2/native-history/providers", get(providers))
        .route("/v2/native-history/{provider}/sessions", get(sessions))
        .route(
            "/v2/native-history/{provider}/sessions/{session_id}/transcript",
            get(transcript),
        )
        .route("/v2/native-history/{provider}/cleanup", post(cleanup))
        .route(
            "/v2/native-history/{provider}/policy",
            get(policy).put(update_policy),
        )
        .route("/v2/native-history/{provider}/policy/run", post(run))
        .route(
            "/v2/native-history/{provider}/native-retention",
            get(native_retention).put(update_native_retention),
        )
}

async fn providers(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<Json<Value>, AppError> {
    require_auth(&state, &headers)?;
    Ok(Json(state.native_history.providers().await?))
}

async fn sessions(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(provider): Path<String>,
) -> Result<Json<Value>, AppError> {
    let auth = require_auth(&state, &headers)?;
    let provider = NativeProvider::parse(&provider)?;
    Ok(Json(
        state
            .native_history
            .sessions(&state.conversations, provider, &auth.tenant_id)
            .await?,
    ))
}

async fn transcript(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path((provider, session_id)): Path<(String, String)>,
    Query(query): Query<HashMap<String, String>>,
) -> Result<Json<Value>, AppError> {
    require_auth(&state, &headers)?;
    let provider = NativeProvider::parse(&provider)?;
    let limit = match query.get("limit") {
        None => DEFAULT_TRANSCRIPT_LIMIT,
        Some(raw) => raw
            .parse::<usize>()
            .ok()
            .filter(|limit| (1..=MAX_TRANSCRIPT_LIMIT).contains(limit))
            .ok_or_else(|| {
                AppError::InvalidRequest(format!(
                    "limit must be an integer between 1 and {MAX_TRANSCRIPT_LIMIT}"
                ))
            })?,
    };
    Ok(Json(
        state
            .native_history
            .transcript(provider, &session_id, limit)
            .await?,
    ))
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct CleanupRequest {
    session_ids: Vec<String>,
}

async fn cleanup(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(provider): Path<String>,
    Json(body): Json<Value>,
) -> Result<Json<Value>, AppError> {
    require_auth(&state, &headers)?;
    let provider = NativeProvider::parse(&provider)?;
    let request: CleanupRequest = serde_json::from_value(body)
        .map_err(|error| AppError::InvalidRequest(format!("invalid cleanup request: {error}")))?;
    let mut seen = HashSet::new();
    let ids: Vec<String> = request
        .session_ids
        .into_iter()
        .filter(|id| seen.insert(id.clone()))
        .collect();
    if ids.is_empty() || ids.len() > MAX_CLEANUP_IDS {
        return Err(AppError::InvalidRequest(format!(
            "sessionIds must name between 1 and {MAX_CLEANUP_IDS} sessions"
        )));
    }
    for id in &ids {
        validate_session_id(id)?;
    }
    let report = state
        .native_history
        .cleanup(&state.conversations, provider, &ids)
        .await?;
    Ok(Json(serde_json::to_value(report)?))
}

async fn policy(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(provider): Path<String>,
) -> Result<Json<Value>, AppError> {
    require_auth(&state, &headers)?;
    let provider = NativeProvider::parse(&provider)?;
    let cleanup = &state.native_history.cleanup;
    let saved = cleanup.tenant(provider.as_str()).await;
    Ok(Json(cleanup.view(&saved)))
}

async fn update_policy(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(provider): Path<String>,
    Json(body): Json<Value>,
) -> Result<Json<Value>, AppError> {
    require_auth(&state, &headers)?;
    let provider = NativeProvider::parse(&provider)?;
    let policy = CleanupPolicy::from_request(body)?;
    provider.validate_policy(&policy)?;
    let cleanup = &state.native_history.cleanup;
    let saved = cleanup.set_policy(provider.as_str(), policy).await?;
    Ok(Json(cleanup.view(&saved)))
}

async fn run(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(provider): Path<String>,
) -> Result<Json<Value>, AppError> {
    require_auth(&state, &headers)?;
    let provider = NativeProvider::parse(&provider)?;
    let policy = state
        .native_history
        .cleanup
        .tenant(provider.as_str())
        .await
        .policy;
    if !policy.has_threshold() {
        return Err(AppError::InvalidRequest(
            "the cleanup policy sets neither maxAgeDays nor maxTotalBytes".to_owned(),
        ));
    }
    let (report, run) = run_policy(
        &state.native_history,
        &state.conversations,
        provider,
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

async fn native_retention(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(provider): Path<String>,
) -> Result<Json<Value>, AppError> {
    require_auth(&state, &headers)?;
    let provider = NativeProvider::parse(&provider)?;
    Ok(Json(state.native_history.native_retention(provider).await?))
}

async fn update_native_retention(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(provider): Path<String>,
    Json(body): Json<Value>,
) -> Result<Json<Value>, AppError> {
    require_auth(&state, &headers)?;
    let provider = NativeProvider::parse(&provider)?;
    Ok(Json(
        state
            .native_history
            .set_native_retention(provider, &body)
            .await?,
    ))
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use axum::body::Body;
    use axum::http::{Request, StatusCode};
    use tower::ServiceExt;

    use crate::config::Config;
    use crate::native_history::NativeRoots;

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
    async fn native_history_routes_follow_the_contract() {
        let root = std::env::temp_dir().join(format!(
            "todex-native-history-routes-{}",
            uuid::Uuid::new_v4()
        ));
        let claude_dir = root.join("claude");
        let pi_dir = root.join("pi");
        std::fs::create_dir_all(root.join("workspaces")).unwrap();
        let mut state = AppState::new_for_tests(Config {
            port: 0,
            data_dir: root.join("data"),
            workspace_roots: vec![root.join("workspaces")],
            ..Config::default()
        })
        .await
        .unwrap();
        Arc::make_mut(&mut state.config).security.enable_auth = false;
        state.native_history.roots_override = Some(NativeRoots {
            claude_dir: claude_dir.clone(),
            pi_session_dir: pi_dir.clone(),
            omp_session_dir: root.join("omp").join("sessions"),
            codex_home: root.join("codex"),
            grok_home: root.join("grok"),
            devin_dir: root.join("devin").join("cli"),
        });
        let id = uuid::Uuid::new_v4().to_string();
        let transcript = claude_dir
            .join("projects")
            .join("-w")
            .join(format!("{id}.jsonl"));
        std::fs::create_dir_all(transcript.parent().unwrap()).unwrap();
        std::fs::write(
            &transcript,
            format!(
                "{}\n{}\n",
                json!({"type": "user", "cwd": "/w", "message": {"role": "user", "content": "hello"}}),
                json!({"type": "assistant", "message": {"role": "assistant", "content": [{"type": "text", "text": "hi"}]}}),
            ),
        )
        .unwrap();
        std::fs::File::options()
            .write(true)
            .open(&transcript)
            .unwrap()
            .set_modified(std::time::SystemTime::now() - std::time::Duration::from_secs(3600))
            .unwrap();
        let app: Router = crate::server::loopback_test_router(state);

        let (status, providers) = call(&app, "GET", "/v2/native-history/providers", None).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(providers["providers"][0]["provider"], "claude-code");
        assert_eq!(providers["providers"][0]["available"], true);
        assert_eq!(providers["providers"][0]["sessionCount"], 1);
        assert_eq!(
            providers["providers"][0]["native"]["settingsPath"],
            json!(claude_dir.join("settings.json"))
        );
        assert_eq!(providers["providers"][1]["provider"], "pi");
        assert_eq!(providers["providers"][1]["available"], false);
        assert_eq!(providers["providers"][1]["native"], Value::Null);
        let ids: Vec<&str> = providers["providers"]
            .as_array()
            .unwrap()
            .iter()
            .map(|provider| provider["provider"].as_str().unwrap())
            .collect();
        assert_eq!(
            ids,
            ["claude-code", "pi", "codex", "grok-build", "devin", "omp"]
        );
        for (index, view, sizes) in [
            (0, true, true),
            (2, true, true),
            (3, true, true),
            (4, false, false),
        ] {
            assert_eq!(
                providers["providers"][index]["capabilities"],
                json!({"view": view, "sizes": sizes}),
                "provider {index}"
            );
        }
        // Missing stores list nothing rather than failing.
        for provider in ["codex", "grok-build", "devin"] {
            assert_eq!(
                providers["providers"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .find(|entry| entry["provider"] == provider)
                    .unwrap()["available"],
                false
            );
            let (status, listing) = call(
                &app,
                "GET",
                &format!("/v2/native-history/{provider}/sessions"),
                None,
            )
            .await;
            assert_eq!(status, StatusCode::OK, "{provider}");
            assert_eq!(listing["count"], 0);
            assert_eq!(listing["sessions"], json!([]));
            assert_eq!(listing["totalBytes"], 0);
        }
        let (status, error) = call(
            &app,
            "GET",
            &format!("/v2/native-history/devin/sessions/{id}/transcript"),
            None,
        )
        .await;
        assert_eq!(status, StatusCode::NOT_IMPLEMENTED);
        assert_eq!(error["code"], "UNSUPPORTED");
        let (status, error) = call(
            &app,
            "PUT",
            "/v2/native-history/devin/policy",
            Some(json!({"enabled": true, "maxAgeDays": null, "maxTotalBytes": 1000})),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert_eq!(error["code"], "INVALID_REQUEST");
        let (status, saved) = call(
            &app,
            "PUT",
            "/v2/native-history/devin/policy",
            Some(json!({"enabled": true, "maxAgeDays": 30, "maxTotalBytes": null})),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(saved["policy"]["maxAgeDays"], 30);
        let (status, _) = call(
            &app,
            "GET",
            "/v2/native-history/grok-build/native-retention",
            None,
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST);

        let (status, listing) =
            call(&app, "GET", "/v2/native-history/claude-code/sessions", None).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(listing["provider"], "claude-code");
        assert_eq!(listing["count"], 1);
        assert_eq!(listing["sessions"][0]["id"], id);
        assert_eq!(listing["sessions"][0]["title"], "hello");
        assert!(listing["sessions"][0]["updatedAt"].is_string());
        let (status, error) = call(&app, "GET", "/v2/native-history/opencode/sessions", None).await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert_eq!(error["code"], "INVALID_REQUEST");

        let uri = format!("/v2/native-history/claude-code/sessions/{id}/transcript?limit=1");
        let (status, transcript_view) = call(&app, "GET", &uri, None).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(transcript_view["totalMessages"], 2);
        assert_eq!(transcript_view["truncated"], true);
        assert_eq!(
            transcript_view["messages"],
            json!([{"role": "assistant", "text": "hi", "at": null}])
        );
        for limit in ["0", "1001", "x"] {
            let uri =
                format!("/v2/native-history/claude-code/sessions/{id}/transcript?limit={limit}");
            let (status, _) = call(&app, "GET", &uri, None).await;
            assert_eq!(status, StatusCode::BAD_REQUEST, "limit {limit}");
        }
        let missing = uuid::Uuid::new_v4();
        let (status, error) = call(
            &app,
            "GET",
            &format!("/v2/native-history/pi/sessions/{missing}/transcript"),
            None,
        )
        .await;
        assert_eq!(status, StatusCode::NOT_FOUND);
        assert_eq!(error["code"], "NOT_FOUND");

        // Policy: same shapes and validation as /v2/history/cleanup-policy.
        let (status, policy) = call(&app, "GET", "/v2/native-history/pi/policy", None).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(
            policy,
            json!({
                "policy": {"enabled": false, "maxAgeDays": null, "maxTotalBytes": null},
                "lastRun": null,
                "nextRunAt": null,
            })
        );
        let (status, _) = call(&app, "POST", "/v2/native-history/pi/policy/run", None).await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        let (status, _) = call(
            &app,
            "PUT",
            "/v2/native-history/pi/policy",
            Some(json!({"enabled": true})),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        let (status, saved) = call(
            &app,
            "PUT",
            "/v2/native-history/claude-code/policy",
            Some(json!({"enabled": false, "maxAgeDays": 30, "maxTotalBytes": null})),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(saved["policy"]["maxAgeDays"], 30);
        let (status, run) = call(
            &app,
            "POST",
            "/v2/native-history/claude-code/policy/run",
            None,
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(run["deleted"], json!([]));
        assert_eq!(run["skipped"], json!([]));
        assert_eq!(run["freedBytes"], 0);
        assert_eq!(run["lastRun"]["trigger"], "manual");
        let (_, policy) = call(&app, "GET", "/v2/native-history/claude-code/policy", None).await;
        assert_eq!(policy["lastRun"], run["lastRun"]);
        let (_, policy) = call(&app, "GET", "/v2/native-history/pi/policy", None).await;
        assert_eq!(policy["policy"]["maxAgeDays"], Value::Null);

        // Native retention (Claude only).
        let (status, retention) = call(
            &app,
            "GET",
            "/v2/native-history/claude-code/native-retention",
            None,
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(retention["cleanupPeriodDays"], Value::Null);
        let (status, retention) = call(
            &app,
            "PUT",
            "/v2/native-history/claude-code/native-retention",
            Some(json!({"cleanupPeriodDays": 14})),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(retention["cleanupPeriodDays"], 14);
        assert_eq!(
            retention["settingsPath"],
            json!(claude_dir.join("settings.json"))
        );
        let (status, _) = call(
            &app,
            "PUT",
            "/v2/native-history/claude-code/native-retention",
            Some(json!({"cleanupPeriodDays": 0})),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        let (status, _) = call(&app, "GET", "/v2/native-history/pi/native-retention", None).await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        std::fs::write(claude_dir.join("settings.json"), b"{broken").unwrap();
        let (status, error) = call(
            &app,
            "PUT",
            "/v2/native-history/claude-code/native-retention",
            Some(json!({"cleanupPeriodDays": null})),
        )
        .await;
        assert_eq!(status, StatusCode::CONFLICT);
        assert_eq!(error["code"], "CONFLICT");

        // Cleanup.
        for body in [
            json!({"sessionIds": []}),
            json!({}),
            json!({"sessionIds": ["../../etc"]}),
        ] {
            let (status, error) = call(
                &app,
                "POST",
                "/v2/native-history/claude-code/cleanup",
                Some(body),
            )
            .await;
            assert_eq!(status, StatusCode::BAD_REQUEST);
            assert_eq!(error["code"], "INVALID_REQUEST");
        }
        let too_many: Vec<String> = (0..501).map(|_| uuid::Uuid::new_v4().to_string()).collect();
        let (status, _) = call(
            &app,
            "POST",
            "/v2/native-history/claude-code/cleanup",
            Some(json!({"sessionIds": too_many})),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        let (status, cleanup) = call(
            &app,
            "POST",
            "/v2/native-history/claude-code/cleanup",
            Some(json!({"sessionIds": [id, id, missing.to_string()]})),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(cleanup["deleted"], json!([id]));
        assert_eq!(cleanup["skipped"][0]["conversationId"], missing.to_string());
        assert_eq!(cleanup["skipped"][0]["code"], "NOT_FOUND");
        assert_eq!(cleanup["freedBytes"], listing["totalBytes"]);
        assert!(!transcript.exists());
        let _ = std::fs::remove_dir_all(root);
    }
}
