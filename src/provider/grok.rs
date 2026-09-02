use std::collections::BTreeMap;
use std::path::Path;
use std::time::Duration;

use async_trait::async_trait;
use serde_json::{json, Value};
use tokio::sync::watch;

use crate::config::AgentConfig;
use crate::conversation::ProviderKind;
use crate::error::AppError;
use crate::workspace_trust::WorkspaceTrustPermit;

use super::acp::{run_acp_turn, AcpRuntimeOptions};
use super::process::{
    executable_available, redact_sensitive_text, run_bounded_command, CommandSpec, JsonLineProcess,
};
use super::types::{
    DriverContext, DriverEventSink, DriverPrompt, DriverTurnResult, ProviderCapabilities,
    ProviderCommandDescriptor, ProviderDescriptor, ProviderDriver, ProviderModelDescriptor,
};

const INSPECT_MAX_BYTES: usize = 4 * 1024 * 1024;
const DIAGNOSTIC_TIMEOUT: Duration = Duration::from_secs(8);

pub struct GrokBuildDriver {
    binary: String,
    auth_method: Option<String>,
    env_allowlist: Vec<String>,
}

impl GrokBuildDriver {
    pub fn new(config: &AgentConfig) -> Self {
        Self {
            binary: config.grok_bin.clone(),
            auth_method: config.grok_auth_method.clone(),
            env_allowlist: config.grok_env_allowlist.clone(),
        }
    }

    fn command_spec(&self, workspace: &Path, prompt: Option<&DriverPrompt>) -> CommandSpec {
        grok_command_spec(&self.binary, &self.env_allowlist, workspace, prompt)
    }

    async fn initialize(&self, workspace: &Path) -> Result<Value, AppError> {
        let mut process = JsonLineProcess::spawn(&self.command_spec(workspace, None)).await?;
        let result = async {
            process
                .send(&json!({
                    "jsonrpc": "2.0",
                    "id": "initialize",
                    "method": "initialize",
                    "params": {
                        "protocolVersion": 1,
                        "clientCapabilities": {
                            "fs": { "readTextFile": false, "writeTextFile": false },
                            "terminal": false,
                            "session": { "configOptions": {} }
                        },
                        "clientInfo": {
                            "name": "todex-agentd",
                            "title": "TodeX 2.0",
                            "version": env!("CARGO_PKG_VERSION")
                        }
                    }
                }))
                .await?;
            tokio::time::timeout(DIAGNOSTIC_TIMEOUT, async {
                loop {
                    let Some(message) = process.read().await? else {
                        return Err(AppError::ProviderUnavailable(
                            "Grok Build closed stdout during initialization".to_owned(),
                        ));
                    };
                    if message.get("id").and_then(Value::as_str) == Some("initialize") {
                        if let Some(error) = message.get("error") {
                            return Err(AppError::ProviderUnavailable(format!(
                                "Grok Build initialization failed: {}",
                                safe_message(error)
                            )));
                        }
                        return Ok(message.get("result").cloned().unwrap_or(Value::Null));
                    }
                    if let Some(id) = message.get("id") {
                        process
                            .send(&json!({
                                "jsonrpc": "2.0",
                                "id": id,
                                "error": {
                                    "code": -32601,
                                    "message": "client capability is not supported during discovery"
                                }
                            }))
                            .await?;
                    }
                }
            })
            .await
            .map_err(|_| {
                AppError::ProviderUnavailable("Grok Build initialization timed out".to_owned())
            })?
        }
        .await;
        process.terminate().await;
        result
    }
}

#[async_trait]
impl ProviderDriver for GrokBuildDriver {
    fn descriptor(&self) -> ProviderDescriptor {
        let available = executable_available(&self.binary);
        ProviderDescriptor {
            id: ProviderKind::GrokBuild,
            display_name: "Grok Build",
            available,
            unavailable_reason: (!available).then(|| {
                format!(
                    "executable '{}' was not found; install Grok Build from https://x.ai/cli",
                    self.binary
                )
            }),
            profiles: Vec::new(),
            capabilities: ProviderCapabilities {
                native_resume: true,
                cancel: true,
                permissions: true,
                tool_events: true,
                native_skills: true,
                native_mcp: true,
                managed_mcp: false,
                model_selection: true,
            },
            models: Vec::new(),
        }
    }

    async fn discover_models(
        &self,
        workspace: &Path,
    ) -> Result<Vec<ProviderModelDescriptor>, AppError> {
        Ok(parse_models(&self.initialize(workspace).await?))
    }

    async fn discover_commands(
        &self,
        workspace: &Path,
    ) -> Result<Vec<ProviderCommandDescriptor>, AppError> {
        Ok(parse_commands(&self.initialize(workspace).await?))
    }

    async fn run_turn(
        &self,
        context: DriverContext,
        prompt: DriverPrompt,
        sink: DriverEventSink,
        mut cancel: watch::Receiver<bool>,
        launch_permit: WorkspaceTrustPermit,
    ) -> Result<DriverTurnResult, AppError> {
        let mut process = JsonLineProcess::spawn_trusted(
            &self.command_spec(&context.manifest.workspace, Some(&prompt)),
            launch_permit,
        )
        .await?;
        let result = run_acp_turn(
            &mut process,
            context,
            prompt,
            &sink,
            &mut cancel,
            AcpRuntimeOptions {
                authenticate: true,
                auth_method: self.auth_method.clone(),
                suppress_load_replay: true,
                allow_cli_config_fallback: true,
                request_ask_mode: true,
                legacy_model_state: true,
                nested_config_values: true,
            },
        )
        .await;
        process.terminate().await;
        result
    }
}

pub(crate) async fn inspect_grok(
    config: &AgentConfig,
    workspace: &Path,
) -> Result<Value, AppError> {
    let mut spec = CommandSpec::new(&config.grok_bin, workspace);
    spec.args = vec![
        "--no-auto-update".to_owned(),
        "inspect".to_owned(),
        "--json".to_owned(),
    ];
    spec.env = grok_environment(&config.grok_env_allowlist);
    let output = run_bounded_command(&spec, INSPECT_MAX_BYTES, DIAGNOSTIC_TIMEOUT).await?;
    if !output.success {
        let stderr = redact_sensitive_text(&String::from_utf8_lossy(&output.stderr));
        return Err(AppError::ProviderUnavailable(if stderr.trim().is_empty() {
            "grok inspect failed".to_owned()
        } else {
            format!("grok inspect failed: {}", stderr.trim())
        }));
    }
    serde_json::from_slice(&output.stdout)
        .map_err(|error| AppError::InvalidRequest(format!("invalid grok inspect JSON: {error}")))
}

fn grok_command_spec(
    binary: &str,
    env_allowlist: &[String],
    workspace: &Path,
    prompt: Option<&DriverPrompt>,
) -> CommandSpec {
    let mut spec = CommandSpec::new(binary, workspace);
    spec.args = vec![
        "--no-auto-update".to_owned(),
        "agent".to_owned(),
        "--no-leader".to_owned(),
    ];
    if let Some(model) = prompt.and_then(|prompt| prompt.model.as_deref()) {
        spec.args.push("--model".to_owned());
        spec.args.push(model.to_owned());
    }
    if let Some(effort) = prompt.and_then(|prompt| prompt.reasoning_effort.as_deref()) {
        spec.args.push("--reasoning-effort".to_owned());
        spec.args.push(effort.to_owned());
    }
    spec.args.push("stdio".to_owned());
    spec.env = grok_environment(env_allowlist);
    spec
}

fn grok_environment(allowlist: &[String]) -> BTreeMap<String, String> {
    let mut env = BTreeMap::from([
        ("GROK_DISABLE_AUTOUPDATER".to_owned(), "1".to_owned()),
        ("NO_COLOR".to_owned(), "1".to_owned()),
    ]);
    for key in allowlist {
        if !valid_env_name(key) || key.starts_with("TODEX_AGENTD_") {
            continue;
        }
        if let Ok(value) = std::env::var(key) {
            env.insert(key.clone(), value);
        }
    }
    env
}

fn valid_env_name(value: &str) -> bool {
    let mut chars = value.chars();
    chars
        .next()
        .is_some_and(|ch| ch == '_' || ch.is_ascii_alphabetic())
        && chars.all(|ch| ch == '_' || ch.is_ascii_alphanumeric())
}

fn parse_models(initialize: &Value) -> Vec<ProviderModelDescriptor> {
    let current = initialize
        .pointer("/_meta/modelState/currentModelId")
        .and_then(Value::as_str);
    initialize
        .pointer("/_meta/modelState/availableModels")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|model| {
            let id = model
                .get("modelId")
                .or_else(|| model.get("id"))
                .and_then(Value::as_str)?
                .to_owned();
            let efforts = reasoning_efforts(model);
            Some(ProviderModelDescriptor {
                display_name: model
                    .get("name")
                    .or_else(|| model.get("displayName"))
                    .and_then(Value::as_str)
                    .unwrap_or(&id)
                    .to_owned(),
                description: model
                    .get("description")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_owned(),
                is_default: current == Some(id.as_str()),
                default_reasoning_effort: model
                    .pointer("/_meta/defaultReasoningEffort")
                    .or_else(|| model.pointer("/_meta/reasoningEffort"))
                    .or_else(|| model.get("defaultReasoningEffort"))
                    .and_then(Value::as_str)
                    .map(ToOwned::to_owned),
                context_window: model
                    .pointer("/_meta/totalContextTokens")
                    .or_else(|| model.get("contextWindow"))
                    .and_then(Value::as_u64),
                id,
                supported_reasoning_efforts: efforts,
            })
        })
        .collect()
}

fn reasoning_efforts(model: &Value) -> Vec<String> {
    [
        model.get("supportedReasoningEfforts"),
        model.get("reasoningEfforts"),
        model.pointer("/_meta/supportedReasoningEfforts"),
        model.pointer("/_meta/reasoningEfforts"),
    ]
    .into_iter()
    .flatten()
    .find_map(Value::as_array)
    .into_iter()
    .flatten()
    .filter_map(|effort| {
        effort
            .as_str()
            .or_else(|| effort.get("value").and_then(Value::as_str))
            .or_else(|| effort.get("id").and_then(Value::as_str))
            .map(ToOwned::to_owned)
    })
    .collect()
}

fn parse_commands(initialize: &Value) -> Vec<ProviderCommandDescriptor> {
    initialize
        .pointer("/_meta/availableCommands")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|command| {
            let name = command.get("name").and_then(Value::as_str)?.to_owned();
            Some(ProviderCommandDescriptor {
                source: if name.contains(':') {
                    "skill-or-plugin".to_owned()
                } else {
                    "builtin".to_owned()
                },
                source_info: command.get("sourceInfo").cloned(),
                description: command
                    .get("description")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_owned(),
                argument_hint: command
                    .pointer("/input/hint")
                    .and_then(Value::as_str)
                    .map(ToOwned::to_owned),
                invocation: "provider-prompt".to_owned(),
                name,
            })
        })
        .collect()
}

fn safe_message(value: &Value) -> String {
    redact_sensitive_text(
        value
            .get("message")
            .and_then(Value::as_str)
            .unwrap_or("provider returned an error"),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_runtime_models_and_commands_without_hard_coding_catalog_values() {
        let initialize = json!({
            "_meta": {
                "modelState": {
                    "currentModelId": "grok-current",
                    "availableModels": [{
                        "modelId": "grok-current",
                        "name": "Current Grok",
                        "description": "fixture",
                        "_meta": {
                            "totalContextTokens": 1000000,
                            "reasoningEfforts": ["low", "high"]
                        }
                    }]
                },
                "availableCommands": [{
                    "name": "repo:review",
                    "description": "Review changes",
                    "input": { "hint": "[path]" }
                }]
            }
        });
        let models = parse_models(&initialize);
        assert_eq!(models.len(), 1);
        assert!(models[0].is_default);
        assert_eq!(models[0].supported_reasoning_efforts, ["low", "high"]);
        assert_eq!(models[0].context_window, Some(1_000_000));
        let commands = parse_commands(&initialize);
        assert_eq!(commands[0].name, "repo:review");
        assert_eq!(commands[0].argument_hint.as_deref(), Some("[path]"));
    }

    #[test]
    fn environment_names_are_strict_and_todex_configuration_is_not_forwarded() {
        assert!(valid_env_name("XAI_API_KEY"));
        assert!(!valid_env_name("XAI-API-KEY"));
        assert!(!valid_env_name("1SECRET"));
    }

    #[test]
    fn command_spec_matches_supported_stdio_argv() {
        let prompt = DriverPrompt {
            turn_id: "turn-1".to_owned(),
            text: "hello".to_owned(),
            content: Vec::new(),
            skills: Vec::new(),
            model: Some("grok-4.5".to_owned()),
            reasoning_effort: Some("high".to_owned()),
        };
        let spec = grok_command_spec("grok", &[], Path::new("/tmp"), Some(&prompt));
        assert_eq!(
            spec.args,
            [
                "--no-auto-update",
                "agent",
                "--no-leader",
                "--model",
                "grok-4.5",
                "--reasoning-effort",
                "high",
                "stdio",
            ]
        );
    }

    #[test]
    fn parses_legacy_reasoning_metadata() {
        let models = parse_models(&json!({
            "_meta": { "modelState": {
                "currentModelId": "grok-4.5",
                "availableModels": [{
                    "modelId": "grok-4.5",
                    "name": "Grok 4.5",
                    "_meta": {
                        "reasoningEffort": "high",
                        "reasoningEfforts": [
                            { "id": "high", "label": "High" },
                            { "id": "low", "label": "Low" }
                        ]
                    }
                }]
            }}
        }));
        assert_eq!(models[0].default_reasoning_effort.as_deref(), Some("high"));
        assert_eq!(models[0].supported_reasoning_efforts, ["high", "low"]);
    }
}
