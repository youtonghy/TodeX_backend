//! `/v2/providers/omp/plugins*` — omp's user-scope plugins, managed only
//! through omp's own `omp plugin … --json` (TodeX never edits
//! `~/.omp/plugins`). Every write needs a signed device: plugins decide what
//! code omp loads and what it may run on this host.

use std::sync::Mutex;
use std::time::Duration;

use axum::extract::{Path, State};
use axum::http::HeaderMap;
use axum::routing::{get, post, put};
use axum::{Json, Router};
use chrono::Utc;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use tracing::warn;
use uuid::Uuid;

use crate::app_state::AppState;
use crate::error::AppError;
use crate::provider::process::{run_bounded_command, CommandSpec};
use crate::provider::ManagedCli;

use super::agent_providers::require_signed_device;
use super::v2::require_auth;
use super::websocket::{self, AuthContext};

const LIST_TIMEOUT: Duration = Duration::from_secs(30);
const TOGGLE_TIMEOUT: Duration = Duration::from_secs(60);
const OPERATION_TIMEOUT: Duration = Duration::from_secs(10 * 60);
const OUTPUT_BYTES: usize = 1024 * 1024;
const MAX_ERROR_CHARS: usize = 500;
const MAX_SPEC_CHARS: usize = 300;

pub(super) fn routes() -> Router<AppState> {
    Router::new()
        .route("/v2/providers/omp/plugins", get(list))
        .route("/v2/providers/omp/plugins/state", put(set_state))
        .route(
            "/v2/providers/omp/plugins/operations",
            post(start_operation),
        )
        .route(
            "/v2/providers/omp/plugins/operations/{operation_id}",
            get(operation),
        )
}

/// One npm/git plugin from `omp plugin list --json`.
#[derive(Debug, Serialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub(crate) struct OmpPlugin {
    name: String,
    version: String,
    enabled: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    description: Option<String>,
    /// Optional features the manifest offers.
    features: Vec<String>,
    /// Enabled features; `None` means the manifest defaults.
    #[serde(skip_serializing_if = "Option::is_none")]
    enabled_features: Option<Vec<String>>,
}

/// A marketplace install; listed, managed in omp.
#[derive(Debug, Serialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub(crate) struct OmpMarketplacePlugin {
    id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    version: Option<String>,
    scope: String,
}

#[derive(Debug, Default, Serialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub(crate) struct OmpPluginList {
    plugins: Vec<OmpPlugin>,
    marketplace: Vec<OmpMarketplacePlugin>,
    /// Why omp could not list its plugins (not installed, failed).
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<String>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct StateRequest {
    name: String,
    enabled: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
enum PluginAction {
    Install,
    Uninstall,
    Upgrade,
}

impl PluginAction {
    const fn verb(self) -> &'static str {
        match self {
            Self::Install => "install",
            Self::Uninstall => "uninstall",
            Self::Upgrade => "upgrade",
        }
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct OperationRequest {
    action: PluginAction,
    /// An npm spec (`pkg`, `@scope/pkg@1.2.3`) or git URL to install, or an
    /// installed plugin's name.
    spec: String,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct PluginOperation {
    id: String,
    action: PluginAction,
    spec: String,
    /// `running`, `succeeded` or `failed`.
    status: &'static str,
    started_at: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    finished_at: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<String>,
}

/// The latest plugin operation; one runs at a time.
static OPERATION: Mutex<Option<PluginOperation>> = Mutex::new(None);

/// `omp <args>` without a shell, from the data directory. Pi's
/// `PI_CODING_AGENT_DIR` would point omp at Pi's directory.
fn omp_command(state: &AppState, args: &[&str]) -> CommandSpec {
    let mut spec = CommandSpec::new(&state.config.agent.omp_bin, &state.config.data_dir);
    spec.args = args.iter().map(|arg| (*arg).to_owned()).collect();
    spec.env.insert("NO_COLOR".to_owned(), "1".to_owned());
    spec.env.insert("FORCE_COLOR".to_owned(), "0".to_owned());
    spec.env_remove = vec!["PI_CODING_AGENT_DIR".to_owned()];
    spec
}

async fn run(state: &AppState, args: &[&str], timeout: Duration) -> Result<String, String> {
    match run_bounded_command(&omp_command(state, args), OUTPUT_BYTES, timeout).await {
        Ok(output) if output.success => Ok(String::from_utf8_lossy(&output.stdout).into_owned()),
        Ok(output) => Err(failure_reason(&output.stderr, &output.stdout)),
        Err(error) => Err(error.to_string()),
    }
}

async fn list(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<Json<OmpPluginList>, AppError> {
    require_auth(&state, &headers)?;
    Ok(Json(read_list(&state).await))
}

async fn read_list(state: &AppState) -> OmpPluginList {
    match run(state, &["plugin", "list", "--json"], LIST_TIMEOUT).await {
        Ok(stdout) => parse_list(&stdout).unwrap_or_else(|error| OmpPluginList {
            error: Some(error),
            ..OmpPluginList::default()
        }),
        Err(error) => OmpPluginList {
            error: Some(error),
            ..OmpPluginList::default()
        },
    }
}

/// `omp plugin list --json`: `{ "npm": [InstalledPlugin], "marketplace":
/// [InstalledMarketplacePlugin] }` (omp 18.13).
pub(crate) fn parse_list(stdout: &str) -> Result<OmpPluginList, String> {
    let value: Value = serde_json::from_str(stdout.trim())
        .map_err(|error| format!("unexpected `omp plugin list` output: {error}"))?;
    let strings = |value: Option<&Value>| -> Option<Vec<String>> {
        value?.as_array().map(|items| {
            items
                .iter()
                .filter_map(|item| item.as_str().map(str::to_owned))
                .collect()
        })
    };
    let plugins = value
        .get("npm")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|plugin| {
            Some(OmpPlugin {
                name: plugin.get("name")?.as_str()?.to_owned(),
                version: plugin
                    .get("version")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_owned(),
                enabled: plugin
                    .get("enabled")
                    .and_then(Value::as_bool)
                    .unwrap_or(true),
                description: plugin
                    .pointer("/manifest/description")
                    .and_then(Value::as_str)
                    .map(str::to_owned),
                features: plugin
                    .pointer("/manifest/features")
                    .and_then(Value::as_object)
                    .map(|features| features.keys().cloned().collect())
                    .unwrap_or_default(),
                enabled_features: strings(plugin.get("enabledFeatures")),
            })
        })
        .collect();
    let marketplace = value
        .get("marketplace")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|plugin| {
            Some(OmpMarketplacePlugin {
                id: plugin.get("id")?.as_str()?.to_owned(),
                version: plugin
                    .pointer("/entries/0/version")
                    .and_then(Value::as_str)
                    .map(str::to_owned),
                scope: plugin
                    .get("scope")
                    .and_then(Value::as_str)
                    .unwrap_or("user")
                    .to_owned(),
            })
        })
        .collect();
    Ok(OmpPluginList {
        plugins,
        marketplace,
        error: None,
    })
}

/// An npm spec, git URL or installed plugin name. Local paths are refused:
/// `omp plugin install <path>` links a directory instead of installing.
fn validate_spec(spec: &str, action: PluginAction) -> Result<(), AppError> {
    let invalid = |reason: &str| {
        Err(AppError::InvalidRequest(format!(
            "invalid plugin: {reason}"
        )))
    };
    if spec.is_empty() || spec.chars().count() > MAX_SPEC_CHARS {
        return invalid("empty or too long");
    }
    if spec.starts_with('-') {
        return invalid("must not start with '-'");
    }
    if spec.chars().any(|ch| ch.is_whitespace() || ch.is_control()) {
        return invalid("must not contain whitespace");
    }
    if action == PluginAction::Install
        && (spec.starts_with(['.', '/', '~', '\\'])
            || spec.starts_with("file:")
            || spec.starts_with("link:")
            || spec.contains('\\'))
    {
        return invalid("local paths are not installed from TodeX");
    }
    Ok(())
}

async fn set_state(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(body): Json<Value>,
) -> Result<Json<OmpPluginList>, AppError> {
    let auth = require_signed_device(&state, &headers, "changing omp plugins")?;
    let request: StateRequest = serde_json::from_value(body)
        .map_err(|error| AppError::InvalidRequest(format!("invalid plugin state: {error}")))?;
    let name = request.name.trim().to_owned();
    validate_spec(&name, PluginAction::Upgrade)?;
    let verb = if request.enabled { "enable" } else { "disable" };
    let Some(_execution_guard) = state.cli_manager.locks().try_write(ManagedCli::Omp) else {
        return Err(AppError::Conflict(
            "an omp start, upgrade or plugin operation is in progress; try again".to_owned(),
        ));
    };
    let result = run(&state, &["plugin", verb, &name, "--json"], TOGGLE_TIMEOUT).await;
    crate::agent_providers::bump_config_revision();
    audit(
        &state,
        &auth,
        verb,
        &name,
        None,
        if result.is_ok() { "success" } else { "failure" },
        result.as_ref().err().map(|_| "OMP_PLUGIN_TOGGLE_FAILED"),
    )
    .await?;
    result.map_err(AppError::ProviderUnavailable)?;
    Ok(Json(read_list(&state).await))
}

/// Starts `omp plugin install|uninstall|upgrade` in the background. It waits
/// for no running omp turn and holds omp's CLI lock, so no omp process
/// starts while plugins change under it.
async fn start_operation(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(body): Json<Value>,
) -> Result<Json<PluginOperation>, AppError> {
    let auth = require_signed_device(&state, &headers, "installing or removing omp plugins")?;
    let request: OperationRequest = serde_json::from_value(body)
        .map_err(|error| AppError::InvalidRequest(format!("invalid plugin operation: {error}")))?;
    let spec = request.spec.trim().to_owned();
    validate_spec(&spec, request.action)?;
    let verb = request.action.verb();
    let deny = |reason: &'static str| audit(&state, &auth, verb, &spec, None, "deny", Some(reason));
    let Some(execution_guard) = state.cli_manager.locks().try_write(ManagedCli::Omp) else {
        deny("CLI_EXECUTION_BUSY").await?;
        return Err(AppError::Conflict(
            "an omp start, upgrade or plugin operation is in progress; try again".to_owned(),
        ));
    };
    if state
        .conversations
        .has_active_turns_for_cli(ManagedCli::Omp)
    {
        deny("AGENT_ACTIVE").await?;
        return Err(AppError::Conflict(
            "finish active omp tasks before changing plugins".to_owned(),
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
                id: format!("omppkg_{}", Uuid::new_v4().simple()),
                action: request.action,
                spec: spec.clone(),
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
            "another omp plugin operation is running".to_owned(),
        ));
    };
    if let Err(error) = audit(
        &state,
        &auth,
        verb,
        &spec,
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
        let verb = started.action.verb();
        let result = run(
            &background,
            &["plugin", verb, &started.spec, "--json"],
            OPERATION_TIMEOUT,
        )
        .await
        .map(|_| ());
        let decision = if result.is_ok() { "success" } else { "failure" };
        let reason = result.is_err().then_some("OMP_PLUGIN_OPERATION_FAILED");
        finish(&started.id, result);
        crate::agent_providers::bump_config_revision();
        audit(
            &background,
            &auth,
            verb,
            &started.spec,
            Some(&started.id),
            decision,
            reason,
        )
        .await
        .unwrap_or_else(
            |error| warn!(error = %error, "failed to persist final omp plugin audit event"),
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
        .ok_or_else(|| AppError::NotFound("omp plugin operation not found".to_owned()))
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

/// The last meaningful line omp printed, which names what went wrong.
fn failure_reason(stderr: &[u8], stdout: &[u8]) -> String {
    let pick = |bytes: &[u8]| {
        crate::pi_plugins::strip_ansi(&String::from_utf8_lossy(bytes))
            .lines()
            .rev()
            .map(str::trim)
            .find(|line| !line.is_empty())
            .map(|line| line.chars().take(MAX_ERROR_CHARS).collect::<String>())
    };
    pick(stderr)
        .or_else(|| pick(stdout))
        .unwrap_or_else(|| "omp exited with an error".to_owned())
}

async fn audit(
    state: &AppState,
    auth: &AuthContext,
    action: &str,
    spec: &str,
    operation_id: Option<&str>,
    decision: &str,
    reason: Option<&str>,
) -> Result<(), AppError> {
    let event = crate::event::EventRecord::new(
        "omp.plugins.audit",
        None,
        None,
        None,
        json!({
            "principal_id": auth.principal_id,
            "tenant_id": auth.tenant_id,
            "token_id": auth.token_id,
            "operation_id": operation_id,
            "action": action,
            "spec": spec,
            "decision": decision,
            "reason": reason,
        }),
    );
    websocket::append_audit_event(state, &event).await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plugin_list_json_maps_npm_and_marketplace_plugins() {
        let list = parse_list(
            r#"{
  "npm": [{"name": "@acme/omp-tools", "version": "1.2.0", "path": "/x", "enabled": false,
           "manifest": {"description": "Acme tools", "features": {"lsp": {}, "web": {}}},
           "enabledFeatures": ["lsp"]},
          {"name": "plain", "version": "0.1.0", "path": "/y", "enabled": true, "manifest": {}, "enabledFeatures": null}],
  "marketplace": [{"id": "review@official", "scope": "user", "entries": [{"version": "2.0.0"}]}]
}"#,
        )
        .unwrap();
        assert_eq!(list.plugins.len(), 2);
        assert_eq!(
            list.plugins[0],
            OmpPlugin {
                name: "@acme/omp-tools".to_owned(),
                version: "1.2.0".to_owned(),
                enabled: false,
                description: Some("Acme tools".to_owned()),
                features: vec!["lsp".to_owned(), "web".to_owned()],
                enabled_features: Some(vec!["lsp".to_owned()]),
            }
        );
        assert_eq!(list.plugins[1].enabled_features, None);
        assert_eq!(list.marketplace[0].id, "review@official");
        assert_eq!(list.marketplace[0].version.as_deref(), Some("2.0.0"));
        assert_eq!(
            parse_list(r#"{"npm": [], "marketplace": []}"#).unwrap(),
            OmpPluginList::default()
        );
        assert!(parse_list("No plugins installed").is_err());
    }

    #[test]
    fn specs_refuse_options_whitespace_and_local_paths() {
        for ok in [
            "omp-plugin",
            "@scope/pkg@1.2.3",
            "github:acme/tools",
            "https://github.com/acme/tools.git",
        ] {
            assert!(validate_spec(ok, PluginAction::Install).is_ok(), "{ok}");
        }
        for bad in [
            "",
            "--force",
            "a b",
            "./local",
            "/abs",
            "~/x",
            "file:../x",
            "link:x",
            "a\\b",
        ] {
            assert!(validate_spec(bad, PluginAction::Install).is_err(), "{bad}");
        }
        // Names of installed plugins are not paths.
        assert!(validate_spec("@scope/pkg", PluginAction::Uninstall).is_ok());
    }

    #[test]
    fn failure_reason_prefers_the_last_stderr_line() {
        assert_eq!(
            failure_reason(b"step\n\x1b[31mbun install failed: 404\x1b[39m\n", b"ok"),
            "bun install failed: 404"
        );
        assert_eq!(failure_reason(b"", b""), "omp exited with an error");
    }
}
