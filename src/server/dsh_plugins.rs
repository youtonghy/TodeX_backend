//! `/v2/providers/deepseek-harness/plugins*` — DeepSeek Harness plugin rows,
//! bundles, version exemptions and known plugin settings (see
//! [`crate::dsh_plugins`]). Every write needs a signed device: plugins are
//! code dsh loads into its own process, outside the agent's sandbox.

use std::sync::Mutex;
use std::time::{Duration, Instant};

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
use crate::dsh_config::{ComposedRow, DshPaths, PatchFile};
use crate::dsh_plugins::{self, DshPluginList, IncompatiblePackage, PluginConfigDocument};
use crate::error::AppError;
use crate::provider::deepseek_harness;
use crate::provider::process::{
    executable_available, executable_stamp, run_bounded_command, CommandSpec,
};
use crate::provider::ManagedCli;

use super::agent_providers::require_signed_device;
use super::v2::require_auth;
use super::websocket::{self, AuthContext};

const OPERATION_TIMEOUT: Duration = Duration::from_secs(10 * 60);
const OPERATION_OUTPUT_BYTES: usize = 1024 * 1024;
const VERSION_TIMEOUT: Duration = Duration::from_secs(15);
/// The composed profile only changes with files the cache key covers; the
/// bound catches edits made where TodeX cannot see them (bundle files).
const COMPOSED_TTL: Duration = Duration::from_secs(300);

pub(super) fn routes() -> Router<AppState> {
    Router::new()
        .route("/v2/providers/deepseek-harness/plugins", get(list))
        .route(
            "/v2/providers/deepseek-harness/plugins/state",
            put(set_state),
        )
        .route(
            "/v2/providers/deepseek-harness/plugins/bundles",
            put(set_bundle),
        )
        .route(
            "/v2/providers/deepseek-harness/plugins/operations",
            post(start_operation),
        )
        .route(
            "/v2/providers/deepseek-harness/plugins/operations/{operation_id}",
            get(operation),
        )
        .route(
            "/v2/providers/deepseek-harness/plugin-config/{plugin}",
            get(read_config).patch(patch_config),
        )
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct StateRequest {
    revision: String,
    id: String,
    enabled: bool,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct BundleRequest {
    revision: String,
    name: String,
    enabled: bool,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct ConfigPatchRequest {
    revision: String,
    patch: Map<String, Value>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "kebab-case")]
enum PluginAction {
    Install,
    Remove,
    AllowVersion,
    RevokeVersion,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct OperationRequest {
    action: PluginAction,
    /// install: the spec; remove: the installed package name; exemptions:
    /// the exact `name@version`.
    source: String,
    #[serde(default)]
    runtime_version: Option<String>,
    /// allow-version: the user accepted that an incompatible plugin may
    /// crash dsh or lose data.
    #[serde(default)]
    accept_risk: bool,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct PluginOperation {
    id: String,
    action: PluginAction,
    source: String,
    /// `running`, `succeeded` or `failed`.
    status: &'static str,
    started_at: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    finished_at: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<String>,
    /// Packages refused for their version, with the exemption that admits
    /// each.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    incompatible: Vec<IncompatiblePackage>,
}

/// The latest plugin operation; one runs at a time.
static OPERATION: Mutex<Option<PluginOperation>> = Mutex::new(None);

struct Composed {
    key: String,
    rows: Vec<ComposedRow>,
    runtime_version: Option<String>,
    at: Instant,
}

static COMPOSED: Mutex<Option<Composed>> = Mutex::new(None);

fn cache_key(state: &AppState) -> Result<String, AppError> {
    let paths = DshPaths::of(&AgentDirs::detect());
    let revision = |path: &std::path::Path| {
        std::fs::read(path)
            .map(|bytes| crate::dsh_config::revision(&bytes))
            .unwrap_or_default()
    };
    Ok(format!(
        "{}|{}|{}|{}|{:?}",
        crate::agent_providers::config_revision(),
        PatchFile::read(&paths.profile_patch())?.revision,
        revision(&paths.home_patch()),
        revision(&paths.profile_manifest()),
        executable_stamp(&state.config.agent.dsh_bin),
    ))
}

/// The composed `acp` profile and dsh's version, cached until a file they
/// derive from changes. dsh is not started while a CLI upgrade holds the
/// execution gate.
async fn composed(state: &AppState) -> Result<(Vec<ComposedRow>, Option<String>), AppError> {
    let key = cache_key(state)?;
    {
        let cache = COMPOSED
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if let Some(cached) = cache
            .as_ref()
            .filter(|cached| cached.key == key && cached.at.elapsed() < COMPOSED_TTL)
        {
            return Ok((cached.rows.clone(), cached.runtime_version.clone()));
        }
    }
    let Some(_gate) = state
        .cli_manager
        .locks()
        .try_read(&[ManagedCli::DeepseekHarness])
    else {
        return Err(AppError::Conflict(
            "a CLI upgrade or plugin operation is running; try again".to_owned(),
        ));
    };
    let rows =
        deepseek_harness::composed_profile(&state.config.agent, &state.config.data_dir).await?;
    let runtime_version = runtime_version(state).await;
    *COMPOSED
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(Composed {
        key,
        rows: rows.clone(),
        runtime_version: runtime_version.clone(),
        at: Instant::now(),
    });
    Ok((rows, runtime_version))
}

/// `dsh -V`; ACP's `agentInfo.version` is a fixed placeholder.
async fn runtime_version(state: &AppState) -> Option<String> {
    let mut spec = CommandSpec::new(&state.config.agent.dsh_bin, neutral_cwd(state));
    spec.args = vec!["-V".to_owned()];
    spec.env = deepseek_harness::dsh_environment(&state.config.agent.dsh_env_allowlist);
    let output = run_bounded_command(&spec, 4096, VERSION_TIMEOUT)
        .await
        .ok()?;
    output
        .success
        .then(|| String::from_utf8_lossy(&output.stdout).trim().to_owned())
        .filter(|version| !version.is_empty())
}

fn neutral_cwd(state: &AppState) -> std::path::PathBuf {
    let dir = state
        .config
        .data_dir
        .join("providers")
        .join(crate::conversation::ProviderKind::DeepseekHarness.as_str());
    let _ = std::fs::create_dir_all(&dir);
    dir
}

fn overlay(state: &AppState) -> std::path::PathBuf {
    neutral_cwd(state).join("todex.patch.yml")
}

async fn list(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<Json<DshPluginList>, AppError> {
    require_auth(&state, &headers)?;
    let composed = composed(&state).await;
    let (rows, runtime_version, error) = match composed {
        Ok((rows, version)) => (rows, version, None),
        Err(error) => (Vec::new(), None, Some(error.to_string())),
    };
    let overlay = overlay(&state);
    let pnpm = executable_available("pnpm");
    blocking(move || {
        let composed = match error {
            None => Ok(rows.as_slice()),
            Some(error) => Err(error),
        };
        dsh_plugins::list(
            &AgentDirs::detect(),
            composed,
            &overlay,
            runtime_version,
            pnpm,
        )
    })
    .await
}

async fn set_state(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(body): Json<Value>,
) -> Result<Json<DshPluginList>, AppError> {
    require_signed_device(&state, &headers, "changing DeepSeek Harness plugins")?;
    let request: StateRequest = serde_json::from_value(body)
        .map_err(|error| AppError::InvalidRequest(format!("invalid plugin state: {error}")))?;
    let (rows, _) = composed(&state).await?;
    let overlay = overlay(&state);
    let _ = blocking(move || {
        dsh_plugins::set_row_enabled(
            &AgentDirs::detect(),
            &rows,
            &overlay,
            &request.revision,
            &request.id,
            request.enabled,
        )
    })
    .await?;
    list(State(state), headers).await
}

async fn set_bundle(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(body): Json<Value>,
) -> Result<Json<DshPluginList>, AppError> {
    require_signed_device(&state, &headers, "changing DeepSeek Harness bundles")?;
    let request: BundleRequest = serde_json::from_value(body)
        .map_err(|error| AppError::InvalidRequest(format!("invalid bundle state: {error}")))?;
    let _ = blocking(move || {
        dsh_plugins::set_bundle_selected(
            &AgentDirs::detect(),
            &request.revision,
            &request.name,
            request.enabled,
        )
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
    let (rows, _) = composed(&state).await?;
    blocking(move || dsh_plugins::plugin_config_page(&AgentDirs::detect(), &rows, &plugin)).await
}

async fn patch_config(
    State(state): State<AppState>,
    Path(plugin): Path<String>,
    headers: HeaderMap,
    Json(body): Json<Value>,
) -> Result<Json<PluginConfigDocument>, AppError> {
    require_signed_device(&state, &headers, "editing DeepSeek Harness plugin settings")?;
    let request: ConfigPatchRequest = serde_json::from_value(body).map_err(|error| {
        AppError::InvalidRequest(format!("invalid plugin settings patch: {error}"))
    })?;
    let (rows, _) = composed(&state).await?;
    let id = plugin.clone();
    let _ = blocking(move || {
        dsh_plugins::patch_plugin_config(
            &AgentDirs::detect(),
            &rows,
            &id,
            &request.revision,
            &request.patch,
        )
    })
    .await?;
    read_config(State(state), Path(plugin), headers).await
}

/// The `dsh plugin --profile acp …` arguments for a validated request.
fn plugin_args(request: &OperationRequest) -> Result<Vec<String>, AppError> {
    let source = request.source.trim();
    let mut args = vec![
        "plugin".to_owned(),
        "--profile".to_owned(),
        crate::dsh_config::PROFILE.to_owned(),
    ];
    match request.action {
        PluginAction::Install | PluginAction::Remove => {
            dsh_plugins::validate_source(source)?;
            if source.starts_with('.') {
                return Err(AppError::InvalidRequest(
                    "a local plugin path must be absolute".to_owned(),
                ));
            }
            let verb = if request.action == PluginAction::Install {
                "add"
            } else {
                "remove"
            };
            args.extend([verb.to_owned(), source.to_owned()]);
        }
        PluginAction::AllowVersion | PluginAction::RevokeVersion => {
            let runtime = request.runtime_version.as_deref().unwrap_or_default();
            dsh_plugins::validate_exemption(source, runtime)?;
            let allow = request.action == PluginAction::AllowVersion;
            if allow && !request.accept_risk {
                return Err(AppError::InvalidRequest(
                    "allowing an incompatible plugin needs acceptRisk".to_owned(),
                ));
            }
            args.extend([
                if allow {
                    "allow-version"
                } else {
                    "revoke-version"
                }
                .to_owned(),
                source.to_owned(),
                "--dsh-version".to_owned(),
                runtime.to_owned(),
            ]);
            if allow {
                args.push("--accept-risk".to_owned());
            }
        }
    }
    Ok(args)
}

/// Runs one `dsh plugin` command in the background. Like CLI upgrades, it
/// waits for running DeepSeek Harness turns and holds the execution gate,
/// so no dsh process starts while the profile's packages change.
async fn start_operation(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(body): Json<Value>,
) -> Result<Json<PluginOperation>, AppError> {
    let auth = require_signed_device(&state, &headers, "changing DeepSeek Harness packages")?;
    let request: OperationRequest = serde_json::from_value(body)
        .map_err(|error| AppError::InvalidRequest(format!("invalid plugin operation: {error}")))?;
    let args = plugin_args(&request)?;
    let source = request.source.trim().to_owned();
    if matches!(request.action, PluginAction::Install | PluginAction::Remove)
        && !executable_available("pnpm")
    {
        return Err(AppError::ProviderUnavailable(
            "dsh installs plugins with pnpm, which is not on the daemon's PATH".to_owned(),
        ));
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
    // DeepSeek Harness's lock alone: other Agents keep running.
    let Some(execution_guard) = state
        .cli_manager
        .locks()
        .try_write(ManagedCli::DeepseekHarness)
    else {
        deny("CLI_EXECUTION_BUSY").await?;
        return Err(AppError::Conflict(
            "a DeepSeek Harness start or upgrade is in progress; try again".to_owned(),
        ));
    };
    if state
        .conversations
        .has_active_turns_for_cli(ManagedCli::DeepseekHarness)
    {
        deny("AGENT_ACTIVE").await?;
        return Err(AppError::Conflict(
            "finish active DeepSeek Harness tasks before changing plugins".to_owned(),
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
            let operation = PluginOperation {
                id: format!("dshpkg_{}", Uuid::new_v4().simple()),
                action: request.action,
                source: source.clone(),
                status: "running",
                started_at: Utc::now().to_rfc3339(),
                finished_at: None,
                error: None,
                incompatible: Vec::new(),
            };
            *current = Some(operation.clone());
            Some(operation)
        }
    };
    let Some(operation) = operation else {
        deny("OPERATION_IN_PROGRESS").await?;
        return Err(AppError::Conflict(
            "another DeepSeek Harness plugin operation is running".to_owned(),
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
            Err((
                "cancelled: the audit record could not be persisted".to_owned(),
                Vec::new(),
            )),
        );
        return Err(error);
    }

    let background = state.clone();
    let started = operation.clone();
    tokio::spawn(async move {
        let _execution_guard = execution_guard;
        let mut spec = CommandSpec::new(&background.config.agent.dsh_bin, neutral_cwd(&background));
        spec.args = args;
        spec.env = deepseek_harness::dsh_environment(&background.config.agent.dsh_env_allowlist);
        spec.env.insert("FORCE_COLOR".to_owned(), "0".to_owned());
        let result =
            match run_bounded_command(&spec, OPERATION_OUTPUT_BYTES, OPERATION_TIMEOUT).await {
                Ok(output) if output.success => Ok(()),
                Ok(output) => {
                    let stderr = String::from_utf8_lossy(&output.stderr);
                    let stdout = String::from_utf8_lossy(&output.stdout);
                    Err((
                        dsh_plugins::failure_reason(&stderr, &stdout),
                        dsh_plugins::incompatible_packages(&stderr),
                    ))
                }
                Err(error) => Err((error.to_string(), Vec::new())),
            };
        let decision = if result.is_ok() { "success" } else { "failure" };
        let reason = result.is_err().then_some("DSH_PLUGIN_OPERATION_FAILED");
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
            |error| warn!(error = %error, "failed to persist final dsh plugin audit event"),
        );
    });
    Ok(Json(operation))
}

async fn operation(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(operation_id): Path<String>,
) -> Result<Json<PluginOperation>, AppError> {
    require_auth(&state, &headers)?;
    OPERATION
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .clone()
        .filter(|operation| operation.id == operation_id)
        .map(Json)
        .ok_or_else(|| AppError::NotFound("DeepSeek Harness plugin operation not found".to_owned()))
}

fn finish(operation_id: &str, result: Result<(), (String, Vec<IncompatiblePackage>)>) {
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
            Err((error, incompatible)) => {
                operation.status = "failed";
                operation.error = Some(error);
                operation.incompatible = incompatible;
            }
        }
    }
}

async fn audit(
    state: &AppState,
    auth: &AuthContext,
    action: PluginAction,
    source: &str,
    operation_id: Option<&str>,
    decision: &str,
    reason: Option<&str>,
) -> Result<(), AppError> {
    let event = crate::event::EventRecord::new(
        "dsh.plugins.audit",
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

    fn request(action: PluginAction, source: &str) -> OperationRequest {
        OperationRequest {
            action,
            source: source.to_owned(),
            runtime_version: Some("0.2.1-rc.2".to_owned()),
            accept_risk: false,
        }
    }

    #[test]
    fn operations_map_to_fixed_dsh_plugin_commands() {
        assert_eq!(
            plugin_args(&request(PluginAction::Install, "@x/bundle@1.0.0")).unwrap(),
            ["plugin", "--profile", "acp", "add", "@x/bundle@1.0.0"]
        );
        assert_eq!(
            plugin_args(&request(PluginAction::Remove, "@x/bundle")).unwrap(),
            ["plugin", "--profile", "acp", "remove", "@x/bundle"]
        );
        assert!(plugin_args(&request(PluginAction::Install, "./local")).is_err());
        assert!(plugin_args(&request(PluginAction::Install, "--global")).is_err());
        // Allowing an incompatible version needs the explicit acceptance.
        assert!(plugin_args(&request(PluginAction::AllowVersion, "@x/b@1.0.0")).is_err());
        let mut allow = request(PluginAction::AllowVersion, "@x/b@1.0.0");
        allow.accept_risk = true;
        assert_eq!(
            plugin_args(&allow).unwrap(),
            [
                "plugin",
                "--profile",
                "acp",
                "allow-version",
                "@x/b@1.0.0",
                "--dsh-version",
                "0.2.1-rc.2",
                "--accept-risk"
            ]
        );
        assert_eq!(
            plugin_args(&request(PluginAction::RevokeVersion, "@x/b@1.0.0")).unwrap(),
            [
                "plugin",
                "--profile",
                "acp",
                "revoke-version",
                "@x/b@1.0.0",
                "--dsh-version",
                "0.2.1-rc.2"
            ]
        );
    }
}
