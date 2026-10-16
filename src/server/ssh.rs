//! `/v2/ssh/*` and `/v2/ftp/*` — remote host management on the backend host.
//! All routes sit behind the device-signature middleware like the rest of
//! `/v2`; hosts and keys are those of the user running the daemon.

use axum::body::Bytes;
use axum::extract::{DefaultBodyLimit, Path as AxumPath, State};
use axum::http::HeaderMap;
use axum::routing::{get, post, put};
use axum::{Json, Router};
use serde::Deserialize;
use serde_json::{json, Value};
use zeroize::Zeroizing;

use crate::app_state::AppState;
use crate::error::AppError;
use crate::ssh::keys::{KeyGenerateRequest, KeyImportRequest};
use crate::ssh::{ConnectionSettings, FtpSiteInput, ManagedHost};

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

/// Optional body of `POST /v2/ssh/hosts/{alias}/test`. No `Debug`: it
/// carries a password.
#[derive(Default, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct TestRequest {
    #[serde(default)]
    password: Option<String>,
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
        .route(
            "/v2/ssh/settings",
            get(get_connection_settings).put(set_connection_settings),
        )
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
    body: Bytes,
) -> Result<Json<Value>, AppError> {
    require_auth(&state, &headers)?;
    let password = parse_test_request(&body)?
        .password
        .map(Zeroizing::new)
        .filter(|password| !password.is_empty());
    Ok(Json(serde_json::to_value(
        state
            .ssh
            .test_connection(&alias, password.as_deref().map(String::as_str))
            .await?,
    )?))
}

/// A blank body (whatever its content type) means no password. Parse
/// errors carry only serde's position and category: its messages may quote
/// the rejected value, which can be the password.
fn parse_test_request(body: &[u8]) -> Result<TestRequest, AppError> {
    if body.trim_ascii().is_empty() {
        return Ok(TestRequest::default());
    }
    serde_json::from_slice(body).map_err(|error| {
        let problem = match error.classify() {
            serde_json::error::Category::Data => "expected { password?: string }",
            _ => "not valid JSON",
        };
        AppError::InvalidRequest(format!(
            "invalid test request body at line {} column {}: {problem}",
            error.line(),
            error.column()
        ))
    })
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

async fn get_connection_settings(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<Json<ConnectionSettings>, AppError> {
    require_auth(&state, &headers)?;
    Ok(Json(state.ssh.connection_settings().await))
}

async fn set_connection_settings(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(settings): Json<ConnectionSettings>,
) -> Result<Json<ConnectionSettings>, AppError> {
    require_auth(&state, &headers)?;
    Ok(Json(state.ssh.set_connection_settings(settings).await?))
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

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use crate::config::Config;
    use crate::device_auth::test_support::TestDevice;
    use axum::body::Body;
    use axum::http::{Request, StatusCode};
    use tower::ServiceExt;

    /// Every body shape a client may send to the test endpoint gets either
    /// a result or the usual `{ code, message }` error, never echoing the
    /// password.
    #[tokio::test]
    async fn test_endpoint_accepts_optional_bodies_and_rejects_bad_ones() {
        let fake = crate::ssh::tests::fixture("").await;
        let root = fake.root.join("app");
        let mut config = Config {
            data_dir: root.join("data"),
            workspace_roots: vec![root.join("workspaces")],
            ..Config::default()
        };
        config.agent.ssh_bin = fake.root.join("fake-ssh").display().to_string();
        let state = AppState::new_for_tests(config).await.unwrap();
        let alias = "todex-body-test-host";
        state
            .ssh
            .create_host(ManagedHost {
                alias: alias.into(),
                host_name: "10.0.0.5".into(),
                ..ManagedHost::default()
            })
            .await
            .unwrap();
        let app = crate::server::loopback_test_router(state);
        let device = TestDevice::new(37);
        device.enroll(&root.join("data"));
        let uri = format!("/v2/ssh/hosts/{alias}/test");
        let send = |content_type: Option<&'static str>, body: &'static str| {
            let mut builder = Request::builder().method("POST").uri(&uri);
            if let Some(content_type) = content_type {
                builder = builder.header("content-type", content_type);
            }
            for (name, value) in device.sign("POST", &uri, body.as_bytes()) {
                builder = builder.header(name, value);
            }
            let request = builder.body(Body::from(body)).unwrap();
            let app = app.clone();
            async move {
                let response = app.oneshot(request).await.unwrap();
                let status = response.status();
                let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
                    .await
                    .unwrap();
                (status, serde_json::from_slice::<Value>(&bytes).unwrap())
            }
        };
        let json = Some("application/json");
        for (content_type, body) in [
            (None, ""),
            (json, ""),
            (json, " \n"),
            (json, "{}"),
            (json, r#"{"password":null}"#),
            (json, r#"{"password":""}"#),
            (json, r#"{"password":"hunter2"}"#),
            // The content type is not checked.
            (Some("text/plain"), r#"{"password":"hunter2"}"#),
            (Some("application/x-www-form-urlencoded"), ""),
        ] {
            let (status, value) = send(content_type, body).await;
            assert_eq!(status, StatusCode::OK, "{content_type:?} {body:?}: {value}");
            assert_eq!(value["ok"], true, "{body:?}: {value}");
        }
        for (content_type, body) in [
            (json, r#"{"password":"hunter2","extra":1}"#),
            (json, r#"{"pasword":"hunter2"}"#),
            (json, r#""hunter2""#),
            (json, r#"{"password":5}"#),
            (json, r#"{"password":"hunter2"#),
            (Some("text/plain"), "hunter2"),
            (json, r#"{"password":"hunter2\n"}"#),
            (json, r#"{"password":"hunter2\u0000"}"#),
        ] {
            let (status, value) = send(content_type, body).await;
            assert_eq!(
                status,
                StatusCode::BAD_REQUEST,
                "{content_type:?} {body:?}: {value}"
            );
            assert_eq!(value["code"], "INVALID_REQUEST", "{value}");
            assert!(
                !value["message"].as_str().unwrap().contains("hunter2"),
                "{value}"
            );
        }
    }
}
