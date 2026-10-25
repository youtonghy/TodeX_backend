//! The local admin API (`/v2/local-admin/*`, docs/API.md "Local admin API"),
//! routed only on the IPC listener. Its callers passed the IPC peer check, so
//! they run as the daemon's user and could edit the data directory directly;
//! these routes take no device signature and do what the TUI and the `pair` /
//! `devices` commands do with those files.
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;

use axum::{
    body::Bytes,
    extract::{FromRequest, Path, State},
    http::StatusCode,
    response::{IntoResponse, Response},
    routing::{delete, get, post},
    Json, Router,
};
use chrono::{DateTime, Utc};
use serde::{de::DeserializeOwned, Deserialize, Serialize};
use serde_json::{json, Value};

use crate::app_state::AppState;
use crate::config::{Config, Deployment, PairingEncryption};
use crate::device_pairing::{self, PairingLookup};
use crate::error::AppError;

/// What the running server knows beyond its configuration.
pub(crate) struct LocalAdminInfo {
    pub started_at: DateTime<Utc>,
    /// The bound main port (the configured one may be `0`).
    pub port: u16,
    /// The bound API listener, when `[api]` is enabled.
    pub api_listen: Option<SocketAddr>,
    pub ipc_path: String,
}

#[derive(Clone)]
struct LocalAdmin {
    state: AppState,
    info: Arc<LocalAdminInfo>,
}

pub(crate) fn routes(state: AppState, info: LocalAdminInfo) -> Router {
    Router::new()
        .route("/v2/local-admin/status", get(status))
        .route("/v2/local-admin/config", get(get_config).put(put_config))
        .route("/v2/local-admin/pairing-requests", get(pairing_requests))
        .route(
            "/v2/local-admin/pairing-requests/{request_id}/decision",
            post(decide),
        )
        .route("/v2/local-admin/devices", get(devices))
        .route("/v2/local-admin/devices/{device_id}", delete(revoke_device))
        .route("/v2/local-admin/reset", post(reset))
        .with_state(LocalAdmin {
            state,
            info: Arc::new(info),
        })
}

/// `{"error":{"code","message"}}` with a lowercase code.
#[derive(Debug)]
struct AdminError {
    status: StatusCode,
    code: &'static str,
    message: String,
}

impl AdminError {
    fn new(status: StatusCode, code: &'static str, message: impl Into<String>) -> Self {
        Self {
            status,
            code,
            message: message.into(),
        }
    }

    fn invalid_config(message: impl Into<String>) -> Self {
        Self::new(StatusCode::BAD_REQUEST, "invalid_config", message)
    }

    fn not_found(message: impl Into<String>) -> Self {
        Self::new(StatusCode::NOT_FOUND, "not_found", message)
    }
}

impl IntoResponse for AdminError {
    fn into_response(self) -> Response {
        (
            self.status,
            Json(json!({"error": {"code": self.code, "message": self.message}})),
        )
            .into_response()
    }
}

impl From<AppError> for AdminError {
    fn from(error: AppError) -> Self {
        match error {
            AppError::NotFound(message) => Self::not_found(message),
            AppError::Conflict(message) => Self::new(StatusCode::CONFLICT, "conflict", message),
            AppError::InvalidRequest(message) => {
                Self::new(StatusCode::BAD_REQUEST, "invalid_request", message)
            }
            other => Self::new(
                StatusCode::INTERNAL_SERVER_ERROR,
                "internal",
                other.to_string(),
            ),
        }
    }
}

impl From<anyhow::Error> for AdminError {
    fn from(error: anyhow::Error) -> Self {
        Self::new(
            StatusCode::INTERNAL_SERVER_ERROR,
            "internal",
            format!("{error:#}"),
        )
    }
}

type AdminResult<T> = Result<Json<T>, AdminError>;

/// `Json` whose rejection is a `400 invalid_request` admin error.
struct AdminJson<T>(T);

impl<S, T> FromRequest<S> for AdminJson<T>
where
    S: Send + Sync,
    T: DeserializeOwned,
{
    type Rejection = AdminError;

    async fn from_request(request: axum::extract::Request, state: &S) -> Result<Self, AdminError> {
        match Json::<T>::from_request(request, state).await {
            Ok(Json(value)) => Ok(Self(value)),
            Err(rejection) => Err(AdminError::new(
                StatusCode::BAD_REQUEST,
                "invalid_request",
                format!("invalid request body: {rejection}"),
            )),
        }
    }
}

fn ok() -> Json<Value> {
    Json(json!({ "ok": true }))
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct Listen {
    host: String,
    port: u16,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct ConnectUrl {
    url: String,
    interface: Option<String>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct ApiStatus {
    enabled: bool,
    host: String,
    port: u16,
    listening: Option<String>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct StatusBody {
    version: &'static str,
    deployment: &'static str,
    data_dir: PathBuf,
    pid: u32,
    started_at: String,
    listen: Listen,
    /// `false`: `listen` is the configured but unbound address; clients
    /// connect over IPC only, so `connectUrls` is empty and `pairingLink`
    /// is `""`.
    network_enabled: bool,
    connect_urls: Vec<ConnectUrl>,
    advertise_url: Option<String>,
    pairing_link: String,
    transport_fingerprint: Option<String>,
    pairing_encryption: &'static str,
    auth_enabled: bool,
    device_count: usize,
    pending_pairing_count: usize,
    api: ApiStatus,
    workspace_roots: Vec<PathBuf>,
    ipc_path: String,
    auto_update: bool,
    config_editable: bool,
}

async fn status(State(admin): State<LocalAdmin>) -> AdminResult<StatusBody> {
    let config = &admin.state.config;
    let network_enabled = config.network_enabled;
    // With the network off the bound port only serves agent tools.
    let port = if network_enabled {
        admin.info.port
    } else {
        config.port
    };
    let addresses = network_enabled.then(|| crate::listen_addrs::connect_addresses(&config.host));
    let connect_urls = match addresses {
        None => Vec::new(),
        Some(Ok(addresses)) => addresses
            .into_iter()
            .map(|address| ConnectUrl {
                url: address.ws_url(port),
                interface: address.interface,
            })
            .collect(),
        Some(Err(error)) => {
            tracing::warn!(%error, "failed to list network interface addresses");
            Vec::new()
        }
    };
    let keys = admin.state.pairing_keys.current()?;
    Ok(Json(StatusBody {
        version: crate::version::APP_VERSION,
        deployment: config.deployment.kind.as_str(),
        data_dir: config.data_dir.clone(),
        pid: std::process::id(),
        started_at: admin.info.started_at.to_rfc3339(),
        listen: Listen {
            host: config.host.clone(),
            port,
        },
        network_enabled,
        connect_urls,
        advertise_url: config.deployment.advertise_url.clone(),
        pairing_link: if network_enabled {
            crate::transport_crypto::pairing_link_json(config, port)?
        } else {
            String::new()
        },
        transport_fingerprint: crate::transport_crypto::transport_fingerprint(
            keys.transport_public_key(config.pairing_encryption),
        ),
        pairing_encryption: config.pairing_encryption.as_str(),
        auth_enabled: config.security.enable_auth,
        device_count: crate::devices::list_devices(&config.data_dir)?.len(),
        pending_pairing_count: device_pairing::list_device_pairing_requests(&config.data_dir)?
            .len(),
        api: ApiStatus {
            enabled: config.api.enabled,
            host: config.api.host.clone(),
            port: config.api.port,
            listening: admin
                .info
                .api_listen
                .map(|addr| format!("{}:{}", config.api.host, addr.port())),
        },
        workspace_roots: config.workspace_roots.clone(),
        ipc_path: admin.info.ipc_path.clone(),
        auto_update: crate::update::auto_update_enabled(),
        config_editable: config.deployment.kind != Deployment::Container,
    }))
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct ConfigBody {
    host: String,
    port: u16,
    network_enabled: bool,
    pairing_encryption: &'static str,
    workspace_roots: Vec<PathBuf>,
    auto_update: bool,
    editable: bool,
    restart_required: bool,
}

/// Roots compared by their canonical form, keeping the order (the first is
/// the primary root).
fn comparable_roots(roots: &[PathBuf]) -> Vec<PathBuf> {
    roots
        .iter()
        .map(|root| crate::workspace_paths::canonical_workspace_root(root).unwrap_or(root.clone()))
        .collect()
}

fn config_body(admin: &LocalAdmin) -> Result<ConfigBody, AdminError> {
    let running = &admin.state.config;
    let saved = Config::load_saved(running.data_dir.clone())?;
    // With the network off the bound port is the agent tools' one.
    let running_port = if running.port == 0 && running.network_enabled {
        admin.info.port
    } else {
        running.port
    };
    let restart_required = saved.network_enabled != running.network_enabled
        || saved.host != running.host
        || saved.port != running_port
        || saved.pairing_encryption != running.pairing_encryption
        || comparable_roots(&saved.workspace_roots) != comparable_roots(&running.workspace_roots);
    Ok(ConfigBody {
        host: saved.host,
        port: saved.port,
        network_enabled: saved.network_enabled,
        pairing_encryption: saved.pairing_encryption.as_str(),
        workspace_roots: saved.workspace_roots,
        auto_update: saved.update.auto_update,
        editable: running.deployment.kind != Deployment::Container,
        restart_required,
    })
}

async fn get_config(State(admin): State<LocalAdmin>) -> AdminResult<ConfigBody> {
    Ok(Json(config_body(&admin)?))
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct ConfigPatch {
    host: Option<String>,
    port: Option<i64>,
    network_enabled: Option<bool>,
    pairing_encryption: Option<String>,
    workspace_roots: Option<Vec<PathBuf>>,
    auto_update: Option<bool>,
}

/// Applies `patch` to the saved configuration with the TUI's rules and
/// returns the host, port, encryption and roots to save.
fn validated_settings(
    saved: &Config,
    patch: ConfigPatch,
) -> Result<(String, u16, PairingEncryption, Vec<PathBuf>), AdminError> {
    let host = match patch.host {
        Some(host) => crate::config::validate_host(&host).map_err(AdminError::invalid_config)?,
        None => saved.host.clone(),
    };
    let port = match patch.port {
        Some(port) => {
            let port = u16::try_from(port)
                .map_err(|_| AdminError::invalid_config("port must be a number from 1 to 65535"))?;
            crate::config::validate_listen_port(port, saved.api.enabled.then_some(saved.api.port))
                .map_err(AdminError::invalid_config)?
        }
        None => saved.port,
    };
    let encryption = match patch.pairing_encryption {
        Some(value) => PairingEncryption::parse(&value).ok_or_else(|| {
            AdminError::invalid_config(
                "pairingEncryption must be \"none\", \"x25519\" or \"ml-kem-768\"",
            )
        })?,
        None => saved.pairing_encryption,
    };
    crate::config::ensure_listener_matches_auth(&host, saved.security.enable_auth, encryption)
        .map_err(AdminError::invalid_config)?;
    let roots = match patch.workspace_roots {
        Some(requested) => {
            let mut roots: Vec<PathBuf> = Vec::with_capacity(requested.len());
            for root in requested {
                let root = crate::workspace_paths::canonical_workspace_root(&root)
                    .map_err(|error| AdminError::invalid_config(error.to_string()))?;
                if !roots.contains(&root) {
                    roots.push(root);
                }
            }
            if roots.is_empty() {
                return Err(AdminError::invalid_config(
                    "at least one workspace root is required",
                ));
            }
            roots
        }
        None => saved.workspace_roots.clone(),
    };
    Ok((host, port, encryption, roots))
}

async fn put_config(State(admin): State<LocalAdmin>, body: Bytes) -> AdminResult<ConfigBody> {
    let running = &admin.state.config;
    if running.deployment.kind == Deployment::Container {
        return Err(AdminError::new(
            StatusCode::CONFLICT,
            "managed_by_image",
            "this daemon's settings come from the container image and its environment",
        ));
    }
    let patch: ConfigPatch = serde_json::from_slice(&body)
        .map_err(|error| AdminError::invalid_config(format!("invalid request body: {error}")))?;
    let saved = Config::load_saved(running.data_dir.clone())?;
    let auto_update = patch.auto_update;
    let network_enabled = patch.network_enabled;
    if network_enabled == Some(false) && !saved.ipc.enabled {
        return Err(AdminError::invalid_config(
            "the network listener can only be turned off while [ipc] is enabled",
        ));
    }
    let touches_listener = patch.host.is_some()
        || patch.port.is_some()
        || patch.pairing_encryption.is_some()
        || patch.workspace_roots.is_some();
    let (host, port, encryption, roots) = validated_settings(&saved, patch)?;
    if touches_listener {
        Config::save_tui_settings(running.data_dir.clone(), &host, port, encryption, &roots)?;
    }
    if let Some(enabled) = network_enabled {
        Config::save_network_enabled(running.data_dir.clone(), enabled)?;
    }
    if let Some(enabled) = auto_update {
        Config::save_update_settings(running.data_dir.clone(), enabled)?;
        crate::config::set_auto_update_configured(enabled);
    }
    Ok(Json(config_body(&admin)?))
}

async fn pairing_requests(
    State(admin): State<LocalAdmin>,
) -> AdminResult<Vec<device_pairing::DevicePairingRequestSummary>> {
    Ok(Json(device_pairing::list_device_pairing_requests(
        &admin.state.config.data_dir,
    )?))
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct DecisionBody {
    approved: bool,
    verification_code: String,
}

async fn decide(
    State(admin): State<LocalAdmin>,
    Path(request_id): Path<String>,
    AdminJson(body): AdminJson<DecisionBody>,
) -> AdminResult<Value> {
    let data_dir = &admin.state.config.data_dir;
    let gone = || AdminError::not_found("no pairing request waiting for a decision has this id");
    if uuid::Uuid::parse_str(&request_id).is_err() {
        return Err(gone());
    }
    let summary = match device_pairing::find_device_pairing_request(data_dir, &request_id)?.1 {
        PairingLookup::Found(summary) => summary,
        _ => return Err(gone()),
    };
    if device_pairing::normalize_code(&summary.verification_code)
        != device_pairing::normalize_code(&body.verification_code)
    {
        return Err(AdminError::new(
            StatusCode::CONFLICT,
            "code_mismatch",
            "the verification code does not match this pairing request",
        ));
    }
    device_pairing::decide_device_pairing(data_dir, &summary.request_id, body.approved).map_err(
        |error| match error {
            // Expired or decided since the lookup.
            AppError::Conflict(message) | AppError::NotFound(message) => {
                AdminError::not_found(message)
            }
            other => other.into(),
        },
    )?;
    Ok(ok())
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct DeviceBody {
    device_id: String,
    name: String,
    public_key: String,
    paired_at: u64,
    last_seen_at: Option<u64>,
}

async fn devices(State(admin): State<LocalAdmin>) -> AdminResult<Vec<DeviceBody>> {
    Ok(Json(
        crate::devices::list_devices(&admin.state.config.data_dir)?
            .into_iter()
            .map(|device| DeviceBody {
                device_id: device.device_id,
                name: device.name,
                public_key: device.public_key,
                paired_at: device.paired_at,
                last_seen_at: device.last_seen_at,
            })
            .collect(),
    ))
}

async fn revoke_device(
    State(admin): State<LocalAdmin>,
    Path(device_id): Path<String>,
) -> AdminResult<Value> {
    if crate::devices::revoke_device(&admin.state.config.data_dir, &device_id)? {
        Ok(ok())
    } else {
        Err(AdminError::not_found(format!(
            "no paired device has id {device_id}"
        )))
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "lowercase")]
enum ResetTarget {
    Devices,
    Encryption,
}

#[derive(Deserialize)]
struct ResetBody {
    target: ResetTarget,
}

async fn reset(
    State(admin): State<LocalAdmin>,
    AdminJson(body): AdminJson<ResetBody>,
) -> AdminResult<Value> {
    let data_dir = &admin.state.config.data_dir;
    match body.target {
        ResetTarget::Devices => crate::devices::revoke_all_devices(data_dir)?,
        ResetTarget::Encryption => {
            crate::transport_crypto::PairingKeys::reset(data_dir).await?;
        }
    }
    Ok(Json(json!({ "ok": true, "restartRequired": true })))
}
