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
use std::net::Ipv4Addr;
use zeroize::Zeroizing;

use crate::app_state::AppState;
use crate::error::AppError;
use crate::privacy_filter::AgentOutputFilter;
use crate::ssh::forward::ForwardInput;
use crate::ssh::ip_pool::{AssignmentInput, AssignmentUpdate, PoolConfigInput};
use crate::ssh::keys::{KeyGenerateRequest, KeyImportRequest};
use crate::ssh::login::LoginAnswer;
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

/// Body of `POST /v2/ssh/hosts/{alias}/login`. No `Debug`: it carries a
/// password.
#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct LoginRequest {
    request_id: String,
    /// Absent (or empty) declines the request.
    #[serde(default)]
    password: Option<String>,
    /// The password came from the client's saved passwords.
    #[serde(default)]
    saved: bool,
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
        .route("/v2/ssh/hosts/{alias}/login", post(answer_login))
        .route(
            "/v2/ssh/settings",
            get(get_connection_settings).put(set_connection_settings),
        )
        .route(
            "/v2/ssh/agent-output-filter",
            get(get_agent_output_filter).put(set_agent_output_filter),
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
        .route("/v2/ssh/forwards", get(list_forwards).post(create_forward))
        .route(
            "/v2/ssh/forwards/{id}",
            put(update_forward).delete(delete_forward),
        )
        .route("/v2/ssh/forwards/{id}/start", post(start_forward))
        .route("/v2/ssh/forwards/{id}/stop", post(stop_forward))
        .route("/v2/ssh/ip-pool", get(get_ip_pool).put(set_ip_pool))
        .route("/v2/ssh/ip-pool/assignments", post(create_ip_assignment))
        .route(
            "/v2/ssh/ip-pool/assignments/{ip}",
            put(update_ip_assignment).delete(delete_ip_assignment),
        )
        .route(
            "/v2/ssh/ip-pool/assignments/{ip}/connect",
            post(connect_ip_assignment),
        )
        .route(
            "/v2/ssh/ip-pool/assignments/{ip}/disconnect",
            post(disconnect_ip_assignment),
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
            "invalid request body at line {} column {}: {problem}",
            error.line(),
            error.column()
        ))
    })
}

/// The optional `{ password?: string }` body of start/connect requests;
/// blank or empty passwords mean none.
fn optional_password(body: &[u8]) -> Result<Option<Zeroizing<String>>, AppError> {
    Ok(parse_test_request(body)?
        .password
        .map(Zeroizing::new)
        .filter(|password| !password.is_empty()))
}

async fn list_forwards(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<Json<Value>, AppError> {
    require_auth(&state, &headers)?;
    Ok(Json(json!({ "forwards": state.ssh.list_forwards().await })))
}

async fn create_forward(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(input): Json<ForwardInput>,
) -> Result<Json<Value>, AppError> {
    require_auth(&state, &headers)?;
    let forward = state.ssh.create_forward(input).await?;
    Ok(Json(json!({ "forward": forward })))
}

async fn update_forward(
    State(state): State<AppState>,
    headers: HeaderMap,
    AxumPath(id): AxumPath<String>,
    Json(input): Json<ForwardInput>,
) -> Result<Json<Value>, AppError> {
    require_auth(&state, &headers)?;
    let forward = state.ssh.update_forward(&id, input).await?;
    Ok(Json(json!({ "forward": forward })))
}

async fn delete_forward(
    State(state): State<AppState>,
    headers: HeaderMap,
    AxumPath(id): AxumPath<String>,
) -> Result<Json<Value>, AppError> {
    require_auth(&state, &headers)?;
    state.ssh.delete_forward(&id).await?;
    Ok(Json(json!({ "deleted": true })))
}

async fn start_forward(
    State(state): State<AppState>,
    headers: HeaderMap,
    AxumPath(id): AxumPath<String>,
    body: Bytes,
) -> Result<Json<Value>, AppError> {
    require_auth(&state, &headers)?;
    let password = optional_password(&body)?;
    let forward = state
        .ssh
        .start_forward(&id, password.as_deref().map(String::as_str))
        .await?;
    Ok(Json(json!({ "forward": forward })))
}

async fn stop_forward(
    State(state): State<AppState>,
    headers: HeaderMap,
    AxumPath(id): AxumPath<String>,
) -> Result<Json<Value>, AppError> {
    require_auth(&state, &headers)?;
    let forward = state.ssh.stop_forward(&id).await?;
    Ok(Json(json!({ "forward": forward })))
}

/// An unknown or malformed address is simply not assigned.
fn assignment_ip(raw: &str) -> Result<Ipv4Addr, AppError> {
    raw.parse()
        .map_err(|_| AppError::NotFound(format!("ip pool assignment {raw}")))
}

async fn get_ip_pool(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<Json<Value>, AppError> {
    require_auth(&state, &headers)?;
    Ok(Json(serde_json::to_value(state.ssh.ip_pool().await)?))
}

async fn set_ip_pool(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(input): Json<PoolConfigInput>,
) -> Result<Json<Value>, AppError> {
    require_auth(&state, &headers)?;
    Ok(Json(serde_json::to_value(
        state.ssh.set_ip_pool(input).await?,
    )?))
}

async fn create_ip_assignment(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(input): Json<AssignmentInput>,
) -> Result<Json<Value>, AppError> {
    require_auth(&state, &headers)?;
    let assignment = state.ssh.create_ip_assignment(input).await?;
    Ok(Json(json!({ "assignment": assignment })))
}

async fn update_ip_assignment(
    State(state): State<AppState>,
    headers: HeaderMap,
    AxumPath(ip): AxumPath<String>,
    Json(input): Json<AssignmentUpdate>,
) -> Result<Json<Value>, AppError> {
    require_auth(&state, &headers)?;
    let assignment = state
        .ssh
        .update_ip_assignment(assignment_ip(&ip)?, input)
        .await?;
    Ok(Json(json!({ "assignment": assignment })))
}

async fn delete_ip_assignment(
    State(state): State<AppState>,
    headers: HeaderMap,
    AxumPath(ip): AxumPath<String>,
) -> Result<Json<Value>, AppError> {
    require_auth(&state, &headers)?;
    state.ssh.delete_ip_assignment(assignment_ip(&ip)?).await?;
    Ok(Json(json!({ "deleted": true })))
}

async fn connect_ip_assignment(
    State(state): State<AppState>,
    headers: HeaderMap,
    AxumPath(ip): AxumPath<String>,
    body: Bytes,
) -> Result<Json<Value>, AppError> {
    require_auth(&state, &headers)?;
    let ip = assignment_ip(&ip)?;
    let password = optional_password(&body)?;
    let assignment = state
        .ssh
        .connect_ip_assignment(ip, password.as_deref().map(String::as_str))
        .await?;
    Ok(Json(json!({ "assignment": assignment })))
}

async fn disconnect_ip_assignment(
    State(state): State<AppState>,
    headers: HeaderMap,
    AxumPath(ip): AxumPath<String>,
) -> Result<Json<Value>, AppError> {
    require_auth(&state, &headers)?;
    let assignment = state
        .ssh
        .disconnect_ip_assignment(assignment_ip(&ip)?)
        .await?;
    Ok(Json(json!({ "assignment": assignment })))
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

/// Answers an agent's `ssh.login.requested`.
async fn answer_login(
    State(state): State<AppState>,
    headers: HeaderMap,
    AxumPath(alias): AxumPath<String>,
    body: Bytes,
) -> Result<Json<Value>, AppError> {
    require_auth(&state, &headers)?;
    let request: LoginRequest = serde_json::from_slice(&body).map_err(|error| {
        // serde's messages may quote the rejected value, the password.
        let problem = match error.classify() {
            serde_json::error::Category::Data => {
                "expected { requestId: string, password?: string, saved?: boolean }"
            }
            _ => "not valid JSON",
        };
        AppError::InvalidRequest(format!("invalid login request: {problem}"))
    })?;
    let password = request
        .password
        .map(Zeroizing::new)
        .filter(|password| !password.is_empty());
    let answer = state
        .ssh
        .answer_login(
            &alias,
            &request.request_id,
            password.as_deref().map(String::as_str),
            request.saved,
        )
        .await?;
    Ok(Json(match answer {
        LoginAnswer::Connected => json!({ "status": "connected" }),
        LoginAnswer::Declined => json!({ "status": "declined" }),
        LoginAnswer::Failed(result) => json!({
            "status": "failed",
            "failure": result.failure,
            "detail": result.detail,
        }),
    }))
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

async fn get_agent_output_filter(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<Json<AgentOutputFilter>, AppError> {
    require_auth(&state, &headers)?;
    Ok(Json(state.ssh.agent_output_filter().await))
}

async fn set_agent_output_filter(
    State(state): State<AppState>,
    headers: HeaderMap,
    body: Bytes,
) -> Result<Json<AgentOutputFilter>, AppError> {
    require_auth(&state, &headers)?;
    let filter: AgentOutputFilter = serde_json::from_slice(&body).map_err(|_| {
        AppError::InvalidRequest(
            "invalid agent output filter: expected { enabled, categories: { privateKey, \
             publicKey, credential, bankCard, idCard } }, all booleans"
                .to_owned(),
        )
    })?;
    Ok(Json(state.ssh.set_agent_output_filter(filter).await?))
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

    /// A client answers an agent's login request; stale ids are 404 and
    /// malformed bodies never echo the password.
    #[tokio::test]
    async fn login_endpoint_answers_open_requests_only() {
        let fake = crate::ssh::tests::fixture("Host web\n").await;
        let root = fake.root.join("app");
        let mut config = Config {
            data_dir: root.join("data"),
            workspace_roots: vec![root.join("workspaces")],
            ..Config::default()
        };
        config.agent.ssh_bin = fake.root.join("fake-ssh").display().to_string();
        let mut state = AppState::new_for_tests(config).await.unwrap();
        // The fixture's home holds the `web` alias.
        state.ssh = fake.service.clone();
        let app = crate::server::loopback_test_router(state.clone());
        let device = TestDevice::new(38);
        device.enroll(&root.join("data"));
        let uri = "/v2/ssh/hosts/web/login";
        let send = |body: String| {
            let mut builder = Request::builder()
                .method("POST")
                .uri(uri)
                .header("content-type", "application/json");
            for (name, value) in device.sign("POST", uri, body.as_bytes()) {
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

        let (status, value) = send(json!({ "requestId": "sshl_gone" }).to_string()).await;
        assert_eq!(status, StatusCode::NOT_FOUND, "{value}");
        for body in [
            r#"{"password":"hunter2"}"#,
            r#"{"requestId":"x","password":"hunter2","extra":1}"#,
            r#"{"requestId":"x","password":"hunter2"#,
        ] {
            let (status, value) = send(body.to_owned()).await;
            assert_eq!(status, StatusCode::BAD_REQUEST, "{body}: {value}");
            assert!(!value.to_string().contains("hunter2"), "{value}");
        }

        let crate::ssh::login::LoginAsk::New(request) = state.ssh.ask_login("c1", "web") else {
            panic!("expected a new request");
        };
        let id = request.id().to_owned();
        let waiting = tokio::spawn(request.wait());
        let (status, value) =
            send(json!({ "requestId": id, "password": "hunter2", "saved": true }).to_string())
                .await;
        assert_eq!(status, StatusCode::OK, "{value}");
        assert_eq!(value, json!({ "status": "connected" }));
        let resolution = waiting.await.unwrap();
        assert_eq!(
            resolution.outcome,
            crate::ssh::login::LoginOutcome::Connected
        );
        assert!(resolution.saved_password);
    }

    /// The forward and IP pool routes speak the documented JSON and error
    /// shapes.
    #[tokio::test]
    async fn forward_and_pool_routes() {
        let fake = crate::ssh::tests::fixture("").await;
        let root = fake.root.join("app");
        let mut config = Config {
            data_dir: root.join("data"),
            workspace_roots: vec![root.join("workspaces")],
            ..Config::default()
        };
        config.agent.ssh_bin = fake.root.join("fake-ssh").display().to_string();
        let state = AppState::new_for_tests(config).await.unwrap();
        state
            .ssh
            .create_host(ManagedHost {
                alias: "fwd-host".into(),
                host_name: "10.0.0.5".into(),
                ..ManagedHost::default()
            })
            .await
            .unwrap();
        let app = crate::server::loopback_test_router(state);
        let device = TestDevice::new(38);
        device.enroll(&root.join("data"));
        let send = |method: &'static str, uri: String, body: String| {
            let mut builder = Request::builder().method(method).uri(&uri);
            builder = builder.header("content-type", "application/json");
            for (name, value) in device.sign(method, &uri, body.as_bytes()) {
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
                (
                    status,
                    serde_json::from_slice::<Value>(&bytes).unwrap_or(Value::Null),
                )
            }
        };
        let (status, value) = send(
            "POST",
            "/v2/ssh/forwards".into(),
            json!({"alias": "fwd-host", "kind": "dynamic", "bindPort": 1080}).to_string(),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{value}");
        assert_eq!(value["forward"]["status"], "stopped");
        assert_eq!(value["forward"]["bindAddress"], "127.0.0.1");
        let id = value["forward"]["id"].as_str().unwrap().to_owned();
        let (status, value) = send(
            "POST",
            "/v2/ssh/forwards".into(),
            json!({"alias": "fwd-host", "kind": "local", "bindAddress": "0.0.0.0",
                "bindPort": 8080, "targetHost": "db", "targetPort": 5432})
            .to_string(),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{value}");
        assert_eq!(value["code"], "INVALID_REQUEST");
        let (status, value) = send("GET", "/v2/ssh/forwards".into(), String::new()).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(value["forwards"].as_array().unwrap().len(), 1);
        let (status, value) = send(
            "POST",
            format!("/v2/ssh/forwards/{id}/start"),
            r#"{"password":"hunter2","x":1}"#.into(),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert!(!value["message"].as_str().unwrap().contains("hunter2"));
        let (status, _) = send("POST", "/v2/ssh/forwards/nope/stop".into(), String::new()).await;
        assert_eq!(status, StatusCode::NOT_FOUND);
        let (status, value) = send("DELETE", format!("/v2/ssh/forwards/{id}"), String::new()).await;
        assert_eq!((status, value), (StatusCode::OK, json!({"deleted": true})));

        let (status, value) = send("GET", "/v2/ssh/ip-pool".into(), String::new()).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(value["cidr"], "127.77.0.0/24");
        assert_eq!(value["manualPortLimit"], 4096);
        let (status, value) = send(
            "POST",
            "/v2/ssh/ip-pool/assignments".into(),
            json!({"alias": "fwd-host", "portMode": "auto"}).to_string(),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{value}");
        assert_eq!(value["assignment"]["ip"], "127.77.0.1");
        assert_eq!(value["assignment"]["state"], "disabled");
        assert_eq!(value["assignment"]["ports"], json!([]));
        let (status, _) = send(
            "POST",
            "/v2/ssh/ip-pool/assignments/not-an-ip/disconnect".into(),
            String::new(),
        )
        .await;
        assert_eq!(status, StatusCode::NOT_FOUND);
        let (status, value) = send(
            "PUT",
            "/v2/ssh/ip-pool".into(),
            json!({"enabled": false, "cidr": "127.0.0.0/16", "idleMinutes": 15}).to_string(),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{value}");
        let (status, _) = send(
            "DELETE",
            "/v2/ssh/ip-pool/assignments/127.77.0.1".into(),
            String::new(),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
    }
}
