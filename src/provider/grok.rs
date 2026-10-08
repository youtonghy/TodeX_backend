use std::collections::BTreeMap;
use std::path::Path;
use std::time::Duration;

use async_trait::async_trait;
use serde_json::{json, Value};
use tokio::sync::watch;

use crate::config::AgentConfig;
use crate::conversation::{ProviderKind, ProviderState};
use crate::error::AppError;
use crate::workspace_trust::WorkspaceTrustPermit;

use super::acp::{AcpQuirks, AcpRuntimeOptions, ExtensionUpdates, PromptUsage, PLAIN_ACP};
use super::discovery::CatalogCache;
use super::process::{
    executable_available, redact_sensitive_text, run_bounded_command, CommandSpec, JsonLineProcess,
};
use super::profile::{
    CatalogProfile, CatalogSource, ConfigHome, FileAttachmentStyle, McpInjection, ProcessModel,
    ProviderProfile, SkillInjection,
};
use super::resident::{allowlisted_environment, initialize_acp, ResidentLaunch, ResidentSessions};
use super::rpc::{FailureWording, RpcClient, RpcPeer};
use super::types::{
    DriverContext, DriverEventSink, DriverPrompt, DriverTurnResult, ImageInputMode,
    PermissionConfigCapabilities, ProviderCommandDescriptor, ProviderControl, ProviderDescriptor,
    ProviderDriver, ProviderModelDescriptor,
};

const INSPECT_MAX_BYTES: usize = 4 * 1024 * 1024;
const DIAGNOSTIC_TIMEOUT: Duration = Duration::from_secs(8);
const GROK_RPC: RpcPeer = RpcPeer {
    name: "Grok",
    failure: FailureWording::Method,
    closed: "Grok closed stdout during control request",
    decline: Some("client capability is not supported during control request"),
    jsonrpc_field: true,
    result_optional: false,
    classify_error: None,
};

/// Where Grok Build deviates from plain ACP.
pub(super) const ACP_QUIRKS: AcpQuirks = AcpQuirks {
    control_commands: Some(super::acp::grok_control_commands),
    prompt_usage: PromptUsage::GrokMetadata,
    extension_updates: Some(ExtensionUpdates {
        is_update_method: super::acp::is_grok_update_method,
        activity: super::acp::grok_activity_event,
    }),
    lenient_updates: true,
    command_updates: true,
    headless_auth: Some((
        super::acp::is_grok_headless_auth_method,
        "run `grok login` first or configure XAI_API_KEY",
    )),
    ..PLAIN_ACP
};

/// What Grok Build supports and how TodeX adapts to it.
pub(super) const PROFILE: ProviderProfile = ProviderProfile {
    kind: ProviderKind::GrokBuild,
    display_name: "Grok Build",
    permission_config: PermissionConfigCapabilities::unsupported(&["ask"]),
    native_fork: true,
    native_compact: false,
    native_resume: true,
    cancel: true,
    permissions: true,
    tool_events: true,
    native_skills: true,
    native_mcp: true,
    model_selection: true,
    image_input: true,
    image_input_mode: ImageInputMode::Always,
    mcp_injection: McpInjection::AcpServers,
    skill_injection: SkillInjection::PromptText,
    file_attachments: FileAttachmentStyle::AtMention,
    profile_required: false,
    recovery_full_scan: false,
    process_model: ProcessModel::Resident {
        idle: Some(Duration::from_secs(300)),
        max_sessions: 32,
    },
    discovery_cache_ttl: Some(Duration::from_secs(60)),
    catalog: CatalogProfile {
        source: CatalogSource::NativeInspect,
        ..CatalogProfile::skills_only(
            ConfigHome {
                env: Some("GROK_HOME"),
                home_relative: ".grok",
            },
            ".grok/skills",
        )
    },
};

pub struct GrokBuildDriver {
    binary: String,
    auth_method: Option<String>,
    env_allowlist: Vec<String>,
    sessions: ResidentSessions,
    models: CatalogCache<Vec<ProviderModelDescriptor>>,
    commands: CatalogCache<Vec<ProviderCommandDescriptor>>,
}

impl GrokBuildDriver {
    pub fn new(config: &AgentConfig) -> Self {
        Self {
            binary: config.grok_bin.clone(),
            auth_method: config.grok_auth_method.clone(),
            env_allowlist: config.grok_env_allowlist.clone(),
            sessions: ResidentSessions::new(&PROFILE, "Grok"),
            models: CatalogCache::new(PROFILE.discovery_cache_ttl),
            commands: CatalogCache::new(PROFILE.discovery_cache_ttl),
        }
    }

    fn command_spec(
        &self,
        workspace: &Path,
        prompt: Option<&DriverPrompt>,
    ) -> Result<CommandSpec, AppError> {
        grok_command_spec(&self.binary, &self.env_allowlist, workspace, prompt)
    }

    async fn initialize(&self, workspace: &Path) -> Result<Value, AppError> {
        let mut process = JsonLineProcess::spawn(&self.command_spec(workspace, None)?).await?;
        let result = initialize_process(&mut process).await;
        process.terminate().await;
        result
    }

    async fn authenticate(
        &self,
        process: &mut JsonLineProcess,
        initialize: &Value,
    ) -> Result<(), AppError> {
        if let Some(method) = super::acp::select_auth_method(
            initialize,
            self.auth_method.as_deref(),
            ProviderKind::GrokBuild,
        )? {
            control_request(
                process,
                "authenticate",
                "authenticate",
                json!({"methodId":method,"_meta":{"headless":true}}),
            )
            .await?;
        }
        Ok(())
    }
}

#[async_trait]
impl ProviderDriver for GrokBuildDriver {
    fn supports_live_controls(&self) -> bool {
        true
    }

    async fn control(
        &self,
        conversation_id: &str,
        expected_turn_id: &str,
        request_id: &str,
        control: ProviderControl,
    ) -> Result<Value, AppError> {
        self.sessions
            .control(conversation_id, expected_turn_id, request_id, control)
            .await
    }

    async fn shutdown_session(&self, conversation_id: &str) {
        self.sessions.stop(conversation_id).await;
    }

    async fn shutdown(&self) {
        self.sessions.stop_all().await;
    }

    fn supports_native_fork(&self) -> bool {
        true
    }

    async fn fork_session(
        &self,
        context: DriverContext,
        launch_permit: WorkspaceTrustPermit,
    ) -> Result<ProviderState, AppError> {
        let source = context
            .provider_state
            .native_session_id
            .as_deref()
            .ok_or_else(|| AppError::InvalidRequest("Grok session has not started".to_owned()))?;
        let mut process = JsonLineProcess::spawn_trusted(
            &self.command_spec(&context.manifest.workspace, None)?,
            launch_permit,
        )
        .await?;
        let result = async {
            let initialize = initialize_process(&mut process).await?;
            self.authenticate(&mut process, &initialize).await?;
            let response = control_request(
                &mut process,
                "fork",
                "_x.ai/session/fork",
                json!({
                    "sourceSessionId": source, "sourceCwd": context.manifest.workspace,
                    "newCwd": context.manifest.workspace,
                }),
            )
            .await?;
            let id = response
                .get("newSessionId")
                .and_then(Value::as_str)
                .filter(|id| !id.is_empty() && *id != source)
                .ok_or_else(|| {
                    AppError::InvalidRequest(
                        "invalid Grok fork response: missing distinct newSessionId".to_owned(),
                    )
                })?;
            let mut state = context.provider_state.clone();
            state.native_session_id = Some(id.to_owned());
            state.recoverable = true;
            state.last_error = None;
            Ok(state)
        }
        .await;
        process.terminate().await;
        result
    }

    fn descriptor(&self) -> ProviderDescriptor {
        let available = executable_available(&self.binary);
        ProviderDescriptor {
            id: ProviderKind::GrokBuild,
            display_name: PROFILE.display_name,
            available,
            unavailable_reason: (!available).then(|| {
                format!(
                    "executable '{}' was not found; install Grok Build from https://x.ai/cli",
                    self.binary
                )
            }),
            profiles: Vec::new(),
            capabilities: PROFILE.capabilities(),
            models: Vec::new(),
        }
    }

    async fn discover_models(
        &self,
        workspace: &Path,
    ) -> Result<Vec<ProviderModelDescriptor>, AppError> {
        let fetch = async { Ok(parse_models(&self.initialize(workspace).await?)) };
        self.models.fetch(&self.binary, workspace, fetch).await
    }

    async fn discover_commands(
        &self,
        workspace: &Path,
    ) -> Result<Vec<ProviderCommandDescriptor>, AppError> {
        let fetch = async {
            let mut process = JsonLineProcess::spawn(&self.command_spec(workspace, None)?).await?;
            let result = async {
                let initialize = initialize_process(&mut process).await?;
                self.authenticate(&mut process, &initialize).await?;
                let response = control_request(
                    &mut process,
                    "commands",
                    "_x.ai/commands/list",
                    json!({"cwd":workspace}),
                )
                .await?;
                let commands = response
                    .get("commands")
                    .and_then(Value::as_array)
                    .ok_or_else(|| {
                        AppError::InvalidRequest("invalid Grok commands response".to_owned())
                    })?;
                Ok(parse_commands(
                    &json!({"_meta":{"availableCommands":commands}}),
                ))
            }
            .await;
            process.terminate().await;
            result
        };
        self.commands.fetch(&self.binary, workspace, fetch).await
    }

    async fn run_turn(
        &self,
        context: DriverContext,
        prompt: DriverPrompt,
        sink: DriverEventSink,
        cancel: watch::Receiver<bool>,
        launch_permit: WorkspaceTrustPermit,
    ) -> Result<DriverTurnResult, AppError> {
        self.sessions
            .run_turn(
                context,
                prompt,
                sink,
                cancel,
                launch_permit,
                |context, prompt| {
                    Ok(ResidentLaunch {
                        spec: self.command_spec(&context.manifest.workspace, Some(prompt))?,
                        runtime: AcpRuntimeOptions {
                            authenticate: true,
                            auth_method: self.auth_method.clone(),
                            auth_meta: Some(json!({ "headless": true })),
                            auth_timeout: None,
                            suppress_load_replay: true,
                            allow_cli_config_fallback: true,
                            request_ask_mode: true,
                            legacy_model_state: true,
                            allow_unadvertised_images: true,
                            snake_case_image_mime: false,
                        },
                    })
                },
            )
            .await
    }
}

async fn initialize_process(process: &mut JsonLineProcess) -> Result<Value, AppError> {
    initialize_acp(process, GROK_RPC, None, Some(DIAGNOSTIC_TIMEOUT)).await
}

async fn control_request(
    process: &mut JsonLineProcess,
    id: &str,
    method: &str,
    params: Value,
) -> Result<Value, AppError> {
    RpcClient::new(process, GROK_RPC)
        .request(id, method, params, Some(DIAGNOSTIC_TIMEOUT))
        .await
}

/// `grok inspect` reports skills and MCP servers together and clients load
/// both catalogs at once; one spawn serves them for a short while.
const INSPECT_CACHE_TTL: Duration = Duration::from_secs(15);
static INSPECT_CACHE: std::sync::LazyLock<CatalogCache<Value>> =
    std::sync::LazyLock::new(|| CatalogCache::new(Some(INSPECT_CACHE_TTL)));

/// `grok inspect --json` for `workspace`.
pub(crate) async fn inspect_grok(
    config: &AgentConfig,
    workspace: &Path,
) -> Result<Value, AppError> {
    INSPECT_CACHE
        .fetch(
            &config.grok_bin,
            workspace,
            inspect_grok_uncached(config, workspace),
        )
        .await
}

async fn inspect_grok_uncached(config: &AgentConfig, workspace: &Path) -> Result<Value, AppError> {
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
) -> Result<CommandSpec, AppError> {
    let mut spec = CommandSpec::new(binary, workspace);
    spec.args = vec![
        "--no-auto-update".to_owned(),
        "agent".to_owned(),
        "--no-leader".to_owned(),
    ];
    if let Some(model) = prompt.and_then(|prompt| prompt.model.as_deref()) {
        spec.push_flag_value("--model", model)?;
    }
    if let Some(effort) = prompt.and_then(|prompt| prompt.reasoning_effort.as_deref()) {
        spec.push_flag_value("--reasoning-effort", effort)?;
    }
    spec.args.push("stdio".to_owned());
    spec.env = grok_environment(env_allowlist);
    Ok(spec)
}

fn grok_environment(allowlist: &[String]) -> BTreeMap<String, String> {
    allowlisted_environment(
        &[("GROK_DISABLE_AUTOUPDATER", "1"), ("NO_COLOR", "1")],
        allowlist,
    )
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
                image_input: Some(true),
                family: None,
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

pub(super) fn parse_commands(initialize: &Value) -> Vec<ProviderCommandDescriptor> {
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
                package_name: None,
                package_version: None,
                name,
            })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    #[cfg(unix)]
    use std::path::PathBuf;

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
    fn command_spec_matches_supported_stdio_argv() {
        let prompt = DriverPrompt {
            turn_id: "turn-1".to_owned(),
            text: "hello".to_owned(),
            content: Vec::new(),
            skills: Vec::new(),
            model: Some("grok-4.5".to_owned()),
            reasoning_effort: Some("high".to_owned()),
            permission_mode: None,
            work_mode: None,
            permission_profile: None,
            sandbox_mode: None,
            approval_policy: None,
        };
        let spec = grok_command_spec("grok", &[], Path::new("/tmp"), Some(&prompt)).unwrap();
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
    #[cfg(unix)]
    struct Fixture {
        root: PathBuf,
        driver: std::sync::Arc<GrokBuildDriver>,
        store: crate::conversation::ConversationStore,
        manifest: crate::conversation::ConversationManifest,
        trust: crate::workspace_trust::WorkspaceTrustStore,
    }

    #[cfg(unix)]
    impl Fixture {
        async fn new() -> Self {
            use std::os::unix::fs::PermissionsExt;
            let root =
                std::env::temp_dir().join(format!("todex-grok-wire-{}", uuid::Uuid::new_v4()));
            std::fs::create_dir_all(&root).unwrap();
            let root = std::fs::canonicalize(root).unwrap();
            let binary = root.join("grok-fixture");
            std::fs::write(
                &binary,
                include_str!("../../tests/fixtures/grok_acp_fixture.py"),
            )
            .unwrap();
            std::fs::set_permissions(&binary, std::fs::Permissions::from_mode(0o755)).unwrap();
            let driver = std::sync::Arc::new(GrokBuildDriver {
                binary: binary.to_string_lossy().to_string(),
                auth_method: None,
                env_allowlist: vec![],
                sessions: ResidentSessions::new(&PROFILE, "Grok"),
                models: CatalogCache::new(None),
                commands: CatalogCache::new(None),
            });
            let store = crate::conversation::ConversationStore::new(root.join("data"))
                .await
                .unwrap();
            let manifest = store
                .create(crate::conversation::ConversationManifest::new(
                    ProviderKind::GrokBuild,
                    root.clone(),
                    None,
                    None,
                ))
                .await
                .unwrap();
            let trust = crate::workspace_trust::WorkspaceTrustStore::new(
                root.join("data"),
                vec![root.clone()],
            )
            .await
            .unwrap();
            trust.set_owned("local", &root, true).await.unwrap();
            Self {
                root,
                driver,
                store,
                manifest,
                trust,
            }
        }

        async fn start(
            &self,
            turn_id: &str,
            text: &str,
        ) -> (
            watch::Sender<bool>,
            tokio::task::JoinHandle<Result<DriverTurnResult, AppError>>,
        ) {
            self.start_with_model(turn_id, text, None).await
        }

        async fn start_with_model(
            &self,
            turn_id: &str,
            text: &str,
            model: Option<&str>,
        ) -> (
            watch::Sender<bool>,
            tokio::task::JoinHandle<Result<DriverTurnResult, AppError>>,
        ) {
            let context = DriverContext {
                manifest: self.manifest.clone(),
                provider_state: self.store.provider_state(&self.manifest.id).await.unwrap(),
                agent_mcp: None,
            };
            let prompt = DriverPrompt {
                turn_id: turn_id.to_owned(),
                text: text.to_owned(),
                content: vec![],
                skills: vec![],
                model: model.map(ToOwned::to_owned),
                reasoning_effort: None,
                permission_mode: None,
                work_mode: None,
                permission_profile: None,
                sandbox_mode: None,
                approval_policy: None,
            };
            let sink = DriverEventSink::new(
                self.store.clone(),
                crate::conversation::ConversationEventHub::default(),
                super::super::types::PermissionBroker::default(),
                &self.manifest.id,
            )
            .with_turn_id(turn_id);
            let permit = self.trust.acquire_owned("local", &self.root).await.unwrap();
            let driver = self.driver.clone();
            let (cancel, receiver) = watch::channel(false);
            let task = tokio::spawn(async move {
                driver
                    .run_turn(context, prompt, sink, receiver, permit)
                    .await
            });
            (cancel, task)
        }

        async fn wait_for_method(&self, method: &str) {
            tokio::time::timeout(Duration::from_secs(5), async {
                loop {
                    if self
                        .requests()
                        .iter()
                        .any(|request| request["method"] == method)
                    {
                        return;
                    }
                    tokio::time::sleep(Duration::from_millis(10)).await;
                }
            })
            .await
            .expect("fixture request deadline");
        }

        fn requests(&self) -> Vec<Value> {
            std::fs::read_to_string(self.root.join("grok-requests.jsonl"))
                .unwrap_or_default()
                .lines()
                .filter_map(|line| serde_json::from_str(line).ok())
                .collect()
        }

        async fn finish(self) {
            self.driver.shutdown().await;
            std::fs::remove_dir_all(self.root).unwrap();
        }
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn wire_routes_extensions_preserves_final_metadata_and_reuses_session() {
        let fixture = Fixture::new().await;
        for turn in ["turn-first", "turn-second"] {
            let (_cancel, task) = fixture.start(turn, "normal").await;
            let result = tokio::time::timeout(Duration::from_secs(5), task)
                .await
                .unwrap()
                .unwrap()
                .unwrap();
            assert_eq!(result.stop_reason, "end_turn");
        }
        let requests = fixture.requests();
        assert_eq!(
            requests
                .iter()
                .filter(|request| request["method"] == "initialize")
                .count(),
            1
        );
        assert_eq!(
            requests
                .iter()
                .filter(|request| request["method"] == "session/new")
                .count(),
            1
        );
        assert_eq!(
            requests
                .iter()
                .filter(|request| request["method"] == "session/load")
                .count(),
            0
        );
        let events = fixture
            .store
            .complete_history(&fixture.manifest.id)
            .await
            .unwrap();
        for kind in [
            "subagent.started",
            "subagent.completed",
            "compaction.started",
            "compaction.completed",
            "usage.updated",
        ] {
            assert_eq!(
                events
                    .iter()
                    .filter(|event| event.event_type == kind)
                    .count(),
                2,
                "{kind}"
            );
        }
        assert!(events.iter().any(
            |event| event.payload.pointer("/metadata/update/sessionUpdate")
                == Some(&json!("future_vendor_update"))
        ));
        assert!(events.iter().any(
            |event| event.payload.pointer("/metadata/structured_output/ok") == Some(&json!(true))
        ));
        assert!(events
            .iter()
            .filter(|event| event.event_type == "usage.updated")
            .all(|event| event.payload["usage"]["last"]["input"] == 100
                && event.payload["aggregation"] == "snapshot"));
        fixture.finish().await;
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn wire_live_controls_acknowledge_effective_config_even_after_prompt_terminal() {
        let fixture = Fixture::new().await;
        let (_cancel, task) = fixture.start("turn-live", "hold").await;
        fixture.wait_for_method("session/prompt").await;
        let stale = fixture
            .driver
            .control(
                &fixture.manifest.id,
                "old-turn",
                "stale",
                ProviderControl::Steer {
                    text: "ignored".to_owned(),
                },
            )
            .await;
        assert!(stale.is_err());
        let steer = fixture
            .driver
            .control(
                &fixture.manifest.id,
                "turn-live",
                "steer-1",
                ProviderControl::Steer {
                    text: "continue".to_owned(),
                },
            )
            .await
            .unwrap();
        assert_eq!(steer["result"]["status"], "queued");
        let config = fixture
            .driver
            .control(
                &fixture.manifest.id,
                "turn-live",
                "config-1",
                ProviderControl::Configure {
                    model: Some("grok-other".to_owned()),
                    reasoning_effort: Some("high".to_owned()),
                },
            )
            .await
            .unwrap();
        assert_eq!(config["source"], "provider-confirmed");
        assert_eq!(
            config["effectiveConfig"],
            json!({"model":"grok-other", "reasoningEffort":"high", "source":"provider-confirmed"})
        );
        assert!(!task.await.unwrap().unwrap().cancelled);
        assert_eq!(
            fixture
                .requests()
                .iter()
                .filter(|request| request["method"] == "_x.ai/interject")
                .count(),
            1
        );
        fixture.finish().await;
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn wire_cancel_drains_metadata_then_reload_keeps_ask_policy() {
        let fixture = Fixture::new().await;
        let (cancel, task) = fixture.start("turn-cancel", "cancel").await;
        fixture.wait_for_method("session/prompt").await;
        cancel.send(true).unwrap();
        assert!(task.await.unwrap().unwrap().cancelled);
        let (_cancel, task) = fixture.start("turn-resume", "normal").await;
        assert_eq!(task.await.unwrap().unwrap().stop_reason, "end_turn");
        let load = fixture
            .requests()
            .into_iter()
            .find(|request| request["method"] == "session/load")
            .unwrap();
        assert_eq!(
            load["params"]["_meta"],
            json!({"noReplay":true,"yoloMode":false,"autoMode":false})
        );
        let events = fixture
            .store
            .complete_history(&fixture.manifest.id)
            .await
            .unwrap();
        assert!(events.iter().any(|event| event.payload["providerMethod"]
            == "session/cancel/result"
            && event.payload["metadata"]["graceful"] == true));
        assert_eq!(
            events
                .iter()
                .filter(|event| event.event_type == "usage.updated")
                .count(),
            2
        );
        fixture.finish().await;
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn wire_turn_queued_behind_a_discarded_session_runs_on_a_new_one() {
        let fixture = Fixture::new().await;
        let (cancel, first) = fixture.start("turn-cancel", "cancel").await;
        fixture.wait_for_method("session/prompt").await;
        let (_cancel, queued) = fixture.start("turn-queued", "normal").await;
        tokio::time::timeout(Duration::from_secs(5), async {
            while !fixture
                .driver
                .sessions
                .has_queued_turn(&fixture.manifest.id)
                .await
            {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .expect("second turn was never queued");
        // A cancelled turn discards the process; the queued turn must not be
        // dropped with it.
        cancel.send(true).unwrap();
        assert!(first.await.unwrap().unwrap().cancelled);
        let queued = tokio::time::timeout(Duration::from_secs(15), queued)
            .await
            .expect("queued turn hung")
            .unwrap()
            .unwrap();
        assert_eq!(queued.stop_reason, "end_turn");
        assert_eq!(
            fixture
                .requests()
                .iter()
                .filter(|request| request["method"] == "initialize")
                .count(),
            2
        );
        fixture.finish().await;
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn wire_discovers_project_commands_and_forks_native_history() {
        let fixture = Fixture::new().await;
        let commands = fixture
            .driver
            .discover_commands(&fixture.root)
            .await
            .unwrap();
        assert_eq!(commands[0].name, "project:review");
        let mut state = ProviderState::new(ProviderKind::GrokBuild);
        state.native_session_id = Some("source-session".to_owned());
        let forked = fixture
            .driver
            .fork_session(
                DriverContext {
                    manifest: fixture.manifest.clone(),
                    provider_state: state,
                    agent_mcp: None,
                },
                fixture
                    .trust
                    .acquire_owned("local", &fixture.root)
                    .await
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(forked.native_session_id.as_deref(), Some("forked-session"));
        assert!(forked.recoverable);
        fixture.finish().await;
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn wire_control_remains_responsive_during_permission_request() {
        let fixture = Fixture::new().await;
        let (cancel, task) = fixture.start("turn-permission", "permission").await;
        fixture.wait_for_method("session/prompt").await;
        // The reverse permission request is pending while steering is acknowledged.
        let response = fixture
            .driver
            .control(
                &fixture.manifest.id,
                "turn-permission",
                "steer-permission",
                ProviderControl::Steer {
                    text: "continue".to_owned(),
                },
            )
            .await
            .unwrap();
        assert_eq!(response["result"]["status"], "queued");
        cancel.send(true).unwrap();
        assert!(task.await.unwrap().unwrap().cancelled);
        fixture.finish().await;
    }
    #[cfg(unix)]
    #[tokio::test]
    async fn wire_rejected_configuration_does_not_report_success_or_end_the_prompt() {
        let fixture = Fixture::new().await;
        let (_cancel, task) = fixture.start("turn-reject", "hold").await;
        fixture.wait_for_method("session/prompt").await;
        let rejected = fixture
            .driver
            .control(
                &fixture.manifest.id,
                "turn-reject",
                "reject-config",
                ProviderControl::Configure {
                    model: Some("reject".to_owned()),
                    reasoning_effort: None,
                },
            )
            .await;
        assert!(matches!(rejected, Err(AppError::InvalidRequest(_))));
        assert!(!task.is_finished());
        fixture
            .driver
            .control(
                &fixture.manifest.id,
                "turn-reject",
                "finish-reject",
                ProviderControl::Steer {
                    text: "finish".to_owned(),
                },
            )
            .await
            .unwrap();
        assert_eq!(task.await.unwrap().unwrap().stop_reason, "end_turn");
        fixture.finish().await;
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn wire_control_timeout_discards_the_resident_process_before_next_turn() {
        let fixture = Fixture::new().await;
        let (_cancel, task) = fixture.start("turn-timeout", "hold").await;
        fixture.wait_for_method("session/prompt").await;
        let rejected = tokio::time::timeout(
            Duration::from_secs(12),
            fixture.driver.control(
                &fixture.manifest.id,
                "turn-timeout",
                "hang-config",
                ProviderControl::Configure {
                    model: Some("hang".to_owned()),
                    reasoning_effort: None,
                },
            ),
        )
        .await
        .unwrap();
        assert!(rejected.is_err());
        assert!(task.await.unwrap().is_err());
        let (_cancel, next) = fixture.start("turn-after-timeout", "normal").await;
        assert_eq!(next.await.unwrap().unwrap().stop_reason, "end_turn");
        assert_eq!(
            fixture
                .requests()
                .iter()
                .filter(|request| request["method"] == "initialize")
                .count(),
            2
        );
        fixture.finish().await;
    }
    #[cfg(unix)]
    #[tokio::test]
    async fn wire_warm_session_reports_the_last_acknowledged_model_without_resending_it() {
        let fixture = Fixture::new().await;
        let (_cancel, first) = fixture
            .start_with_model("turn-configured", "normal", Some("grok-other"))
            .await;
        first.await.unwrap().unwrap();
        let (_cancel, second) = fixture.start("turn-warm", "normal").await;
        second.await.unwrap().unwrap();
        let events = fixture
            .store
            .complete_history(&fixture.manifest.id)
            .await
            .unwrap();
        assert!(events
            .iter()
            .any(|event| event.event_type == "turn.configuration"
                && event.payload["turnId"] == "turn-warm"
                && event.payload["effectiveConfig"]["model"] == "grok-other"));
        assert_eq!(
            fixture
                .requests()
                .iter()
                .filter(|request| request["method"] == "session/set_config_option")
                .count(),
            1
        );
        fixture.finish().await;
    }
    #[cfg(unix)]
    #[tokio::test]
    async fn wire_cancel_after_prompt_terminal_keeps_metadata_without_waiting_for_it_again() {
        let fixture = Fixture::new().await;
        let (cancel, task) = fixture.start("turn-cached-terminal", "hold").await;
        fixture.wait_for_method("session/prompt").await;
        let driver = fixture.driver.clone();
        let id = fixture.manifest.id.clone();
        let control = tokio::spawn(async move {
            driver
                .control(
                    &id,
                    "turn-cached-terminal",
                    "cached-config",
                    ProviderControl::Configure {
                        model: Some("hang-terminal".to_owned()),
                        reasoning_effort: None,
                    },
                )
                .await
        });
        tokio::time::timeout(Duration::from_secs(3), async {
            loop {
                if fixture
                    .store
                    .complete_history(&fixture.manifest.id)
                    .await
                    .unwrap()
                    .iter()
                    .any(|event| event.event_type == "usage.updated")
                {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap();
        cancel.send(true).unwrap();
        assert!(
            tokio::time::timeout(Duration::from_secs(1), task)
                .await
                .unwrap()
                .unwrap()
                .unwrap()
                .cancelled
        );
        assert!(control.await.unwrap().is_err());
        let events = fixture
            .store
            .complete_history(&fixture.manifest.id)
            .await
            .unwrap();
        assert_eq!(
            events
                .iter()
                .filter(|event| event.event_type == "usage.updated")
                .count(),
            1
        );
        assert!(events.iter().any(|event| event.payload["providerMethod"]
            == "session/cancel/result"
            && event.payload["metadata"]["graceful"] == true));
        fixture.finish().await;
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn wire_cancel_drain_never_replies_to_a_late_control_response() {
        let fixture = Fixture::new().await;
        let (cancel, task) = fixture.start("turn-late", "hold").await;
        fixture.wait_for_method("session/prompt").await;
        let driver = fixture.driver.clone();
        let id = fixture.manifest.id.clone();
        let control = tokio::spawn(async move {
            driver
                .control(
                    &id,
                    "turn-late",
                    "late-config",
                    ProviderControl::Configure {
                        model: Some("late-ack".to_owned()),
                        reasoning_effort: None,
                    },
                )
                .await
        });
        fixture.wait_for_method("session/set_config_option").await;
        cancel.send(true).unwrap();
        assert!(task.await.unwrap().unwrap().cancelled);
        assert!(control.await.unwrap().is_err());
        let events = fixture
            .store
            .complete_history(&fixture.manifest.id)
            .await
            .unwrap();
        assert!(events.iter().any(|event| event.payload["providerMethod"]
            == "session/cancel/result"
            && event.payload["metadata"]["graceful"] == true));
        fixture.finish().await;
    }
    #[cfg(unix)]
    #[tokio::test]
    async fn wire_malformed_configuration_ack_is_unknown_and_discards_the_process() {
        let fixture = Fixture::new().await;
        let (_cancel, task) = fixture.start("turn-malformed", "hold").await;
        fixture.wait_for_method("session/prompt").await;
        let outcome = fixture
            .driver
            .control(
                &fixture.manifest.id,
                "turn-malformed",
                "malformed-config",
                ProviderControl::Configure {
                    model: Some("malformed".to_owned()),
                    reasoning_effort: None,
                },
            )
            .await;
        assert!(matches!(outcome, Err(AppError::ProviderUnavailable(_))));
        assert!(task.await.unwrap().is_err());
        fixture.finish().await;
    }
}
