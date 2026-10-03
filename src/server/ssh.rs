//! `/v2/ssh/*` and `/v2/ftp/*` — remote host management on the backend host.
//! All routes sit behind the device-signature middleware like the rest of
//! `/v2`; hosts and keys are those of the user running the daemon.

use axum::extract::{DefaultBodyLimit, Path as AxumPath, State};
use axum::http::HeaderMap;
use axum::routing::{get, post, put};
use axum::{Json, Router};
use serde::Deserialize;
use serde_json::{json, Value};

use crate::app_state::AppState;
use crate::error::AppError;
use crate::ssh::keys::{KeyGenerateRequest, KeyImportRequest};
use crate::ssh::{FtpSiteInput, ManagedHost};

use super::v2::require_auth;

const MAX_IMPORT_BYTES: usize = 256 * 1024;
/// Real private keys are a few KiB; the body also carries a public key line.
const MAX_KEY_BODY_BYTES: usize = 64 * 1024;

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct ImportRequest {
    text: String,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct AgentAccessRequest {
    enabled: bool,
}

pub(super) fn routes() -> Router<AppState> {
    Router::new()
        .route("/v2/ssh/hosts", get(list_hosts).post(create_host))
        .route(
            "/v2/ssh/hosts/import",
            post(import_hosts).layer(DefaultBodyLimit::max(MAX_IMPORT_BYTES)),
        )
        .route(
            "/v2/ssh/hosts/{alias}",
            put(update_host).delete(delete_host),
        )
        .route("/v2/ssh/hosts/{alias}/agent-access", put(set_agent_access))
        .route("/v2/ssh/hosts/{alias}/test", post(test_host))
        .route("/v2/ssh/hosts/{alias}/disconnect", post(disconnect_host))
        .route("/v2/ssh/keys", get(list_keys))
        .route(
            "/v2/ssh/keys/import",
            post(import_key).layer(DefaultBodyLimit::max(MAX_KEY_BODY_BYTES)),
        )
        .route(
            "/v2/ssh/keys/generate",
            post(generate_key).layer(DefaultBodyLimit::max(MAX_KEY_BODY_BYTES)),
        )
        .route("/v2/ftp/sites", post(create_ftp_site))
        .route(
            "/v2/ftp/sites/{id}",
            put(update_ftp_site).delete(delete_ftp_site),
        )
}

async fn list_hosts(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<Json<Value>, AppError> {
    require_auth(&state, &headers)?;
    let hosts = state.ssh.list_hosts().await?;
    let ftp_sites = state.ssh.ftp_sites().await;
    Ok(Json(json!({ "hosts": hosts, "ftpSites": ftp_sites })))
}

async fn create_host(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(host): Json<ManagedHost>,
) -> Result<Json<Value>, AppError> {
    require_auth(&state, &headers)?;
    let host = state.ssh.create_host(host).await?;
    Ok(Json(json!({ "host": host })))
}

async fn import_hosts(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(request): Json<ImportRequest>,
) -> Result<Json<Value>, AppError> {
    require_auth(&state, &headers)?;
    Ok(Json(serde_json::to_value(
        state.ssh.import_snippet(&request.text).await?,
    )?))
}

async fn update_host(
    State(state): State<AppState>,
    headers: HeaderMap,
    AxumPath(alias): AxumPath<String>,
    Json(host): Json<ManagedHost>,
) -> Result<Json<Value>, AppError> {
    require_auth(&state, &headers)?;
    let host = state.ssh.update_host(&alias, host).await?;
    Ok(Json(json!({ "host": host })))
}

async fn delete_host(
    State(state): State<AppState>,
    headers: HeaderMap,
    AxumPath(alias): AxumPath<String>,
) -> Result<Json<Value>, AppError> {
    require_auth(&state, &headers)?;
    state.ssh.delete_host(&alias).await?;
    Ok(Json(json!({ "deleted": true })))
}

async fn set_agent_access(
    State(state): State<AppState>,
    headers: HeaderMap,
    AxumPath(alias): AxumPath<String>,
    Json(request): Json<AgentAccessRequest>,
) -> Result<Json<Value>, AppError> {
    require_auth(&state, &headers)?;
    state.ssh.set_agent_access(&alias, request.enabled).await?;
    Ok(Json(
        json!({ "alias": alias, "agentAccess": request.enabled }),
    ))
}

async fn test_host(
    State(state): State<AppState>,
    headers: HeaderMap,
    AxumPath(alias): AxumPath<String>,
) -> Result<Json<Value>, AppError> {
    require_auth(&state, &headers)?;
    Ok(Json(serde_json::to_value(
        state.ssh.test_connection(&alias).await?,
    )?))
}

async fn disconnect_host(
    State(state): State<AppState>,
    headers: HeaderMap,
    AxumPath(alias): AxumPath<String>,
) -> Result<Json<Value>, AppError> {
    require_auth(&state, &headers)?;
    let disconnected = state.ssh.disconnect(&alias).await?;
    Ok(Json(json!({ "disconnected": disconnected })))
}

async fn list_keys(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<Json<Value>, AppError> {
    require_auth(&state, &headers)?;
    Ok(Json(serde_json::to_value(state.ssh.list_keys().await?)?))
}

async fn import_key(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(request): Json<KeyImportRequest>,
) -> Result<Json<Value>, AppError> {
    require_auth(&state, &headers)?;
    let key = state.ssh.import_key(request).await?;
    Ok(Json(json!({ "key": key })))
}

async fn generate_key(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(request): Json<KeyGenerateRequest>,
) -> Result<Json<Value>, AppError> {
    require_auth(&state, &headers)?;
    let key = state.ssh.generate_key(request).await?;
    Ok(Json(json!({ "key": key })))
}

async fn create_ftp_site(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(input): Json<FtpSiteInput>,
) -> Result<Json<Value>, AppError> {
    require_auth(&state, &headers)?;
    let site = state.ssh.upsert_ftp_site(None, input).await?;
    Ok(Json(json!({ "site": site })))
}

async fn update_ftp_site(
    State(state): State<AppState>,
    headers: HeaderMap,
    AxumPath(id): AxumPath<String>,
    Json(input): Json<FtpSiteInput>,
) -> Result<Json<Value>, AppError> {
    require_auth(&state, &headers)?;
    let site = state.ssh.upsert_ftp_site(Some(&id), input).await?;
    Ok(Json(json!({ "site": site })))
}

async fn delete_ftp_site(
    State(state): State<AppState>,
    headers: HeaderMap,
    AxumPath(id): AxumPath<String>,
) -> Result<Json<Value>, AppError> {
    require_auth(&state, &headers)?;
    state.ssh.delete_ftp_site(&id).await?;
    Ok(Json(json!({ "deleted": true })))
}
