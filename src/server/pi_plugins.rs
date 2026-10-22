//! `/v2/providers/pi/plugins*` — Pi packages, built-in extensions and the
//! configuration files of known Pi extensions (see [`crate::pi_plugins`]).
//! Every write needs a signed device: packages and extension settings
//! decide what code Pi loads and what it may run on this host.

use std::sync::Mutex;

use axum::extract::{Path, State};
use axum::http::HeaderMap;
use axum::routing::{get, post, put};
use axum::{Json, Router};
use chrono::Utc;
use serde::{Deserialize, Serialize};
use serde_json::{json, Map, Value};
use tracing::warn;
use uuid::Uuid;

use crate::agent_providers::AgentDirs;
use crate::app_state::AppState;
use crate::error::AppError;
use crate::pi_plugins::{self, PiPluginList, PluginConfigDocument, ToggleTarget};
use crate::provider::process::{run_bounded_command, CommandSpec};
use crate::provider::ManagedCli;

use super::agent_providers::require_signed_device;
use super::v2::require_auth;
use super::websocket::{self, AuthContext};

const OPERATION_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10 * 60);
const OPERATION_OUTPUT_BYTES: usize = 1024 * 1024;
const MAX_ERROR_CHARS: usize = 500;

pub(super) fn routes() -> Router<AppState> {
    Router::new()
        .route("/v2/providers/pi/plugins", get(list))
        .route("/v2/providers/pi/plugins/state", put(set_state))
        .route("/v2/providers/pi/plugins/operations", post(start_operation))
        .route(
            "/v2/providers/pi/plugins/operations/{operation_id}",
            get(operation),
        )
        .route(
            "/v2/providers/pi/plugin-config/{plugin}",
            get(read_config).patch(patch_config),
        )
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct StateRequest {
    revision: String,
    #[serde(default)]
    source: Option<String>,
    #[serde(default)]
    builtin: Option<String>,
    enabled: bool,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct ConfigPatchRequest {
    revision: String,
    patch: Map<String, Value>,
    #[serde(default)]
    replace: Vec<Vec<String>>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
enum PackageAction {
    Install,
    Remove,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct OperationRequest {
    action: PackageAction,
    source: String,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct PackageOperation {
    id: String,
    action: PackageAction,
    source: String,
    /// `running`, `succeeded` or `failed`.
    status: &'static str,
    started_at: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    finished_at: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<String>,
}

/// The latest package operation; one runs at a time.
static OPERATION: Mutex<Option<PackageOperation>> = Mutex::new(None);

async fn list(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<Json<PiPluginList>, AppError> {
    require_auth(&state, &headers)?;
    let installed =
        pi_plugins::installed_paths(&state.config.agent.pi_bin, &state.config.data_dir).await;
    blocking(move || pi_plugins::list(&AgentDirs::detect(), installed)).await
}

async fn set_state(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(body): Json<Value>,
) -> Result<Json<PiPluginList>, AppError> {
    require_signed_device(&state, &headers, "changing Pi plugins")?;
    let request: StateRequest = serde_json::from_value(body)
        .map_err(|error| AppError::InvalidRequest(format!("invalid plugin state: {error}")))?;
    let target = match (request.source, request.builtin) {
        (Some(source), None) => ToggleTarget::Package(source),
        (None, Some(builtin)) => ToggleTarget::Builtin(builtin),
        _ => {
            return Err(AppError::InvalidRequest(
                "name exactly one of `source` or `builtin`".to_owned(),
            ))
        }
    };
    let (revision, enabled) = (request.revision, request.enabled);
    let _ = blocking(move || {
        pi_plugins::set_enabled(&AgentDirs::detect(), &revision, &target, enabled).map(|_| ())
    })
    .await?;
    list(State(state), headers).await
}

async fn read_config(
    State(state): State<AppState>,
    Path(plugin): Path<String>,
    headers: HeaderMap,
) -> Result<Json<PluginConfigDocument>, AppError> {
    require_auth(&state, &headers)?;
    let spec = pi_plugins::plugin_spec(&plugin)?;
    blocking(move || pi_plugins::read_plugin_config(&AgentDirs::detect(), spec)).await
}

async fn patch_config(
    State(state): State<AppState>,
    Path(plugin): Path<String>,
    headers: HeaderMap,
    Json(body): Json<Value>,
) -> Result<Json<PluginConfigDocument>, AppError> {
    let spec = pi_plugins::plugin_spec(&plugin)?;
    require_signed_device(&state, &headers, "editing Pi plugin settings")?;
    let request: ConfigPatchRequest = serde_json::from_value(body).map_err(|error| {
        AppError::InvalidRequest(format!("invalid plugin settings patch: {error}"))
    })?;
    blocking(move || {
        pi_plugins::patch_plugin_config(
            &AgentDirs::detect(),
            spec,
            &request.revision,
            &request.patch,
            &request.replace,
        )
    })
    .await
}

/// Starts `pi install <source>` or `pi remove <source>` in the background.
/// Like CLI upgrades, it waits for running Pi turns and holds the execution
/// gate, so no Pi process starts while packages change under it.
async fn start_operation(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(body): Json<Value>,
) -> Result<Json<PackageOperation>, AppError> {
    let auth = require_signed_device(&state, &headers, "installing or removing Pi packages")?;
    let request: OperationRequest = serde_json::from_value(body)
        .map_err(|error| AppError::InvalidRequest(format!("invalid package operation: {error}")))?;
    let source = request.source.trim().to_owned();
    pi_plugins::validate_source(&source)?;
    if request.action == PackageAction::Remove {
        let configured =
            blocking(|| pi_plugins::list(&AgentDirs::detect(), Ok(Default::default()))).await?;
        if !configured
            .packages
            .iter()
            .any(|package| package.source == source)
        {
            return Err(AppError::NotFound(format!("{source} is not configured")));
        }
    }
    let deny = |reason: &'static str| {
        audit(
            &state,
            &auth,
            request.action,
            &source,
            None,
            "deny",
            Some(reason),
        )
    };
    // Pi's lock alone: other Agents keep running.
    let Some(execution_guard) = state.cli_manager.locks().try_write(ManagedCli::Pi) else {
        deny("CLI_EXECUTION_BUSY").await?;
        return Err(AppError::Conflict(
            "a Pi start or upgrade is in progress; try again".to_owned(),
        ));
    };
    if state.conversations.has_active_turns_for_cli(ManagedCli::Pi) {
        deny("AGENT_ACTIVE").await?;
        return Err(AppError::Conflict(
            "finish active Pi tasks before changing packages".to_owned(),
        ));
    }
    let operation = {
        let mut current = OPERATION
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if current
            .as_ref()
            .is_some_and(|operation| operation.status == "running")
        {
            None
        } else {
            let operation = PackageOperation {
                id: format!("pipkg_{}", Uuid::new_v4().simple()),
                action: request.action,
                source: source.clone(),
                status: "running",
                started_at: Utc::now().to_rfc3339(),
                finished_at: None,
                error: None,
            };
            *current = Some(operation.clone());
            Some(operation)
        }
    };
    let Some(operation) = operation else {
        deny("OPERATION_IN_PROGRESS").await?;
        return Err(AppError::Conflict(
            "another Pi package operation is running".to_owned(),
        ));
    };
    if let Err(error) = audit(
        &state,
        &auth,
        operation.action,
        &source,
        Some(&operation.id),
        "attempt",
        None,
    )
    .await
    {
        finish(
            &operation.id,
            Err("cancelled: the audit record could not be persisted".to_owned()),
        );
        return Err(error);
    }

    let background = state.clone();
    let started = operation.clone();
    tokio::spawn(async move {
        let _execution_guard = execution_guard;
        let verb = match started.action {
            PackageAction::Install => "install",
            PackageAction::Remove => "remove",
        };
        let mut spec =
            CommandSpec::new(&background.config.agent.pi_bin, &background.config.data_dir);
        spec.args = vec![verb.to_owned(), started.source.clone()];
        spec.env.insert("NO_COLOR".to_owned(), "1".to_owned());
        spec.env.insert("FORCE_COLOR".to_owned(), "0".to_owned());
        let result =
            match run_bounded_command(&spec, OPERATION_OUTPUT_BYTES, OPERATION_TIMEOUT).await {
                Ok(output) if output.success => Ok(()),
                Ok(output) => Err(failure_reason(&output.stderr, &output.stdout)),
                Err(error) => Err(error.to_string()),
            };
        let decision = if result.is_ok() { "success" } else { "failure" };
        let reason = result.is_err().then_some(match started.action {
            PackageAction::Install => "PI_PACKAGE_INSTALL_FAILED",
            PackageAction::Remove => "PI_PACKAGE_REMOVE_FAILED",
        });
        finish(&started.id, result);
        crate::agent_providers::bump_config_revision();
        audit(
            &background,
            &auth,
            started.action,
            &started.source,
            Some(&started.id),
            decision,
            reason,
        )
        .await
        .unwrap_or_else(
            |error| warn!(error = %error, "failed to persist final Pi package audit event"),
        );
    });
    Ok(Json(operation))
}

async fn operation(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(operation_id): Path<String>,
) -> Result<Json<PackageOperation>, AppError> {
    require_auth(&state, &headers)?;
    OPERATION
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .clone()
        .filter(|operation| operation.id == operation_id)
        .map(Json)
        .ok_or_else(|| AppError::NotFound("Pi package operation not found".to_owned()))
}

fn finish(operation_id: &str, result: Result<(), String>) {
    let mut current = OPERATION
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    if let Some(operation) = current
        .as_mut()
        .filter(|operation| operation.id == operation_id)
    {
        operation.finished_at = Some(Utc::now().to_rfc3339());
        match result {
            Ok(()) => operation.status = "succeeded",
            Err(error) => {
                operation.status = "failed";
                operation.error = Some(error);
            }
        }
    }
}

/// The last meaningful line Pi printed, which names what went wrong.
fn failure_reason(stderr: &[u8], stdout: &[u8]) -> String {
    let pick = |bytes: &[u8]| {
        pi_plugins::strip_ansi(&String::from_utf8_lossy(bytes))
            .lines()
            .rev()
            .map(str::trim)
            .find(|line| !line.is_empty())
            .map(|line| line.chars().take(MAX_ERROR_CHARS).collect::<String>())
    };
    pick(stderr)
        .or_else(|| pick(stdout))
        .unwrap_or_else(|| "pi exited with an error".to_owned())
}

async fn audit(
    state: &AppState,
    auth: &AuthContext,
    action: PackageAction,
    source: &str,
    operation_id: Option<&str>,
    decision: &str,
    reason: Option<&str>,
) -> Result<(), AppError> {
    let event = crate::event::EventRecord::new(
        "pi.packages.audit",
        None,
        None,
        None,
        json!({
            "principal_id": auth.principal_id,
            "tenant_id": auth.tenant_id,
            "token_id": auth.token_id,
            "operation_id": operation_id,
            "action": action,
            "source": source,
            "decision": decision,
            "reason": reason,
        }),
    );
    websocket::append_audit_event(state, &event).await
}

async fn blocking<T: Send + 'static>(
    work: impl FnOnce() -> Result<T, AppError> + Send + 'static,
) -> Result<Json<T>, AppError> {
    tokio::task::spawn_blocking(work)
        .await
        .map_err(|error| AppError::Anyhow(error.into()))?
        .map(Json)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn failure_reason_prefers_the_last_stderr_line() {
        assert_eq!(
            failure_reason(b"step\n\x1b[31mnpm ERR! 404 not found\x1b[39m\n\n", b"ok"),
            "npm ERR! 404 not found"
        );
        assert_eq!(
            failure_reason(b"", b"No matching package found for npm:x\n"),
            "No matching package found for npm:x"
        );
        assert_eq!(failure_reason(b"", b""), "pi exited with an error");
    }
}
