//! DeepSeek Harness (`dsh --profile acp`): a resident ACP agent.
//!
//! dsh serves the standard ACP v1 automation surface: `session/resume`
//! without `session/load`, `model` and `reasoning_effort` config options, and
//! one-shot permission prompts. It has no modes, commands, fork, compaction
//! or steering. TodeX launches it with an overlay patch that turns off the
//! default-on upload of session logs to the official DeepSeek API.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::Duration;

use async_trait::async_trait;
use serde_json::{json, Value};
use tokio::sync::watch;

use crate::config::AgentConfig;
use crate::conversation::ProviderKind;
use crate::error::AppError;
use crate::workspace_trust::WorkspaceTrustPermit;

use super::acp::{AcpQuirks, AcpRuntimeOptions, PLAIN_ACP};
use super::discovery::{DiscoveryCache, DiscoveryKey, DiscoverySnapshot};
use super::process::{executable_available, CommandSpec, JsonLineProcess};
use super::profile::{
    CatalogProfile, ConfigHome, FileAttachmentStyle, McpInjection, ProcessModel, ProviderProfile,
    SkillInjection,
};
use super::resident::{allowlisted_environment, initialize_acp, ResidentLaunch, ResidentSessions};
use super::rpc::{FailureWording, RpcClient, RpcPeer};
use super::types::{
    DriverContext, DriverEventSink, DriverPrompt, DriverTurnResult, ImageInputMode,
    PermissionConfigCapabilities, ProviderControl, ProviderDescriptor, ProviderDriver,
    ProviderModelDescriptor,
};

const CONTROL_TIMEOUT: Duration = Duration::from_secs(20);
const DSH_RPC: RpcPeer = RpcPeer {
    name: "DeepSeek Harness",
    failure: FailureWording::Method,
    closed: "DeepSeek Harness closed stdout during control request",
    decline: Some("client capability is not supported during control request"),
    jsonrpc_field: true,
    result_optional: false,
    classify_error: None,
};
/// A cold probe boots Node and the plugin tree (~2-5s) before `session/new`;
/// each per-model `reasoning_effort` probe then answers in milliseconds.
const DISCOVERY_TIMEOUT: Duration = Duration::from_secs(30);
/// dsh's `reasoning_effort` value for "the provider's own default"; it is not
/// an effort level, so it never appears in the catalog.
const PROVIDER_DEFAULT_EFFORT: &str = "";
/// The overlay TodeX passes with `--patch`. dsh-base mounts
/// `session-log-deepseek`, which uploads the session log with every official
/// DeepSeek API request; TodeX does not opt users into that.
const PATCH_FILE: &str = "todex.patch.yml";
const PATCH: &str = "# Managed by TodeX; rewritten on every launch.\n\
- id: session-log-deepseek\n  disabled: true\n";

/// Where dsh deviates from plain ACP.
pub(super) const ACP_QUIRKS: AcpQuirks = AcpQuirks {
    client_side_approval: true,
    prefer_resume: true,
    recreate_unloadable_session: true,
    control_commands: Some(super::acp::dsh_control_commands),
    lenient_updates: true,
    usage_updates: true,
    ..PLAIN_ACP
};

/// What DeepSeek Harness supports and how TodeX adapts to it.
pub(super) const PROFILE: ProviderProfile = ProviderProfile {
    kind: ProviderKind::DeepseekHarness,
    display_name: "DeepSeek Harness",
    permission_config: PermissionConfigCapabilities {
        modes: &["ask", "auto", "full-access"],
        default_mode: "ask",
        supports_plan: false,
        sandbox_modes: &[],
        approval_policies: &[],
        permission_profiles: &[],
        enforcement: "agent-policy",
        description: "dsh asks for each gated tool over ACP; ask mediates every request, auto/full-access approve client-side",
    },
    native_fork: false,
    native_compact: false,
    native_resume: true,
    cancel: true,
    permissions: true,
    tool_events: true,
    native_skills: false,
    native_mcp: true,
    model_selection: true,
    image_input: false,
    image_input_mode: ImageInputMode::Profile,
    mcp_injection: McpInjection::AcpServers,
    skill_injection: SkillInjection::PromptText,
    file_attachments: FileAttachmentStyle::AtMention,
    profile_required: false,
    recovery_full_scan: false,
    process_model: ProcessModel::Resident {
        idle: Some(Duration::from_secs(300)),
        max_sessions: 32,
    },
    discovery_cache_ttl: Some(Duration::from_secs(300)),
    catalog: CatalogProfile::skills_only(
        ConfigHome {
            env: Some("DSH_HOME"),
            home_relative: ".dsh",
        },
        ".agents/skills",
    ),
};

pub struct DeepseekHarnessDriver {
    binary: String,
    env_allowlist: Vec<String>,
    patch_path: PathBuf,
    sessions: ResidentSessions,
    /// Models plus whether the default route takes images.
    discovery: DiscoveryCache<DiscoveryKey, (DiscoverySnapshot, bool)>,
}

impl DeepseekHarnessDriver {
    pub fn new(config: &AgentConfig, data_dir: &Path) -> Self {
        Self {
            binary: config.dsh_bin.clone(),
            env_allowlist: config.dsh_env_allowlist.clone(),
            patch_path: data_dir
                .join("providers")
                .join(ProviderKind::DeepseekHarness.as_str())
                .join(PATCH_FILE),
            sessions: ResidentSessions::new(&PROFILE, "DeepSeek Harness"),
            discovery: DiscoveryCache::new(),
        }
    }

    /// `dsh --profile acp --patch <overlay>`, writing the overlay first so a
    /// deleted or edited file is restored before every launch.
    fn command_spec(&self, workspace: &Path) -> Result<CommandSpec, AppError> {
        write_patch(&self.patch_path)?;
        let mut spec = CommandSpec::new(&self.binary, workspace);
        spec.args = vec![
            "--profile".to_owned(),
            "acp".to_owned(),
            "--patch".to_owned(),
            self.patch_path.to_string_lossy().into_owned(),
        ];
        spec.env = dsh_environment(&self.env_allowlist);
        Ok(spec)
    }

    /// dsh needs no `authenticate`: provider keys come from its environment
    /// and `$DSH_HOME/.credentials.yaml`, and it advertises no auth methods.
    fn runtime_options(&self) -> AcpRuntimeOptions {
        AcpRuntimeOptions::default()
    }

    /// The model catalog lives behind `session/new`, and `reasoning_effort`
    /// is only offered for the selected model, so each model is selected once
    /// in a throwaway session.
    async fn discovery_snapshot(
        &self,
        workspace: &Path,
    ) -> Result<(DiscoverySnapshot, bool), AppError> {
        let key = DiscoveryKey::new(&self.binary, workspace).await;
        self.discovery
            .get_or_fetch(&key, || async {
                let mut process = JsonLineProcess::spawn(&self.command_spec(workspace)?).await?;
                let result = async {
                    let initialize = initialize_acp(&mut process, DSH_RPC, None, None).await?;
                    let session = control_request(
                        &mut process,
                        "session",
                        "session/new",
                        json!({ "cwd": workspace, "mcpServers": [] }),
                        None,
                    )
                    .await?;
                    let mut models = parse_dsh_models(&session);
                    let complete = probe_efforts(&mut process, &session, &mut models).await;
                    close_probe_session(&mut process, &session).await;
                    let image_input = initialize
                        .pointer("/agentCapabilities/promptCapabilities/image")
                        .and_then(Value::as_bool)
                        .unwrap_or(false);
                    Ok::<_, AppError>((
                        DiscoverySnapshot {
                            models,
                            commands: Vec::new(),
                            complete,
                        },
                        image_input,
                    ))
                }
                .await;
                process.terminate().await;
                let (snapshot, image_input) = result?;
                let ttl = snapshot.ttl(PROFILE.discovery_cache_ttl.unwrap());
                Ok(((snapshot, image_input), ttl))
            })
            .await
    }
}

fn write_patch(path: &Path) -> Result<(), AppError> {
    if std::fs::read_to_string(path).is_ok_and(|current| current == PATCH) {
        return Ok(());
    }
    let parent = path.parent().ok_or_else(|| {
        AppError::ProviderUnavailable("DeepSeek Harness patch path has no parent".to_owned())
    })?;
    std::fs::create_dir_all(parent)?;
    let temporary = parent.join(format!(".{PATCH_FILE}.{}", uuid::Uuid::new_v4()));
    std::fs::write(&temporary, PATCH)?;
    std::fs::rename(&temporary, path).inspect_err(|_| {
        let _ = std::fs::remove_file(&temporary);
    })?;
    Ok(())
}

/// dsh groups its `model` choices by provider route. Values are opaque route
/// keys (currently a JSON `[provider, model]` pair) that must be sent back
/// verbatim, so they become the model id; the group names the family.
fn parse_dsh_models(session: &Value) -> Vec<ProviderModelDescriptor> {
    let Some(option) = config_option(session, "model") else {
        return Vec::new();
    };
    let current = option.get("currentValue").and_then(Value::as_str);
    let mut models = Vec::new();
    for entry in option
        .get("options")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        match entry.get("options").and_then(Value::as_array) {
            Some(grouped) => {
                let family = entry.get("group").and_then(Value::as_str);
                models.extend(
                    grouped
                        .iter()
                        .filter_map(|model| model_entry(model, family, current)),
                );
            }
            None => models.extend(model_entry(entry, None, current)),
        }
    }
    models
}

fn model_entry(
    model: &Value,
    family: Option<&str>,
    current: Option<&str>,
) -> Option<ProviderModelDescriptor> {
    let id = model
        .get("value")
        .and_then(Value::as_str)
        .filter(|id| !id.is_empty())?
        .to_owned();
    Some(ProviderModelDescriptor {
        display_name: model
            .get("name")
            .and_then(Value::as_str)
            .unwrap_or(&id)
            .to_owned(),
        description: model
            .get("description")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned(),
        is_default: current == Some(id.as_str()),
        supported_reasoning_efforts: Vec::new(),
        default_reasoning_effort: None,
        context_window: None,
        image_input: None,
        family: family.map(str::to_owned),
        id,
    })
}

fn config_option<'a>(response: &'a Value, id: &str) -> Option<&'a Value> {
    response
        .get("configOptions")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .find(|option| option.get("id").and_then(Value::as_str) == Some(id))
}

/// Applies the `reasoning_effort` option of one model's config state.
fn apply_efforts(model: &mut ProviderModelDescriptor, response: &Value) {
    let Some(effort) = config_option(response, "reasoning_effort") else {
        model.supported_reasoning_efforts.clear();
        model.default_reasoning_effort = None;
        return;
    };
    model.supported_reasoning_efforts = effort
        .get("options")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|option| option.get("value").and_then(Value::as_str))
        .filter(|value| *value != PROVIDER_DEFAULT_EFFORT)
        .map(str::to_owned)
        .collect();
    model.default_reasoning_effort = effort
        .get("currentValue")
        .and_then(Value::as_str)
        .filter(|value| *value != PROVIDER_DEFAULT_EFFORT)
        .map(str::to_owned);
}

/// Selects each model once to learn its effort levels. The default model is
/// read from the `session/new` state without a round trip. Returns whether
/// every model answered.
async fn probe_efforts(
    process: &mut JsonLineProcess,
    session: &Value,
    models: &mut [ProviderModelDescriptor],
) -> bool {
    let session_id = session
        .get("sessionId")
        .and_then(Value::as_str)
        .unwrap_or_default();
    let mut complete = true;
    for model in models {
        if model.is_default {
            apply_efforts(model, session);
            continue;
        }
        match control_request(
            process,
            "probe:model",
            "session/set_config_option",
            json!({"sessionId": session_id, "configId": "model", "value": model.id}),
            Some(CONTROL_TIMEOUT),
        )
        .await
        {
            Ok(response) => apply_efforts(model, &response),
            Err(_) => complete = false,
        }
    }
    complete
}

/// Best-effort close for a probe session; the process is terminated
/// immediately afterwards either way.
async fn close_probe_session(process: &mut JsonLineProcess, session: &Value) {
    let Some(session_id) = session.get("sessionId").and_then(Value::as_str) else {
        return;
    };
    let _ = control_request(
        process,
        "session:close",
        "session/close",
        json!({ "sessionId": session_id }),
        None,
    )
    .await;
}

#[async_trait]
impl ProviderDriver for DeepseekHarnessDriver {
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

    async fn shutdown_session_with_reason(&self, conversation_id: &str, _reason: &str) {
        self.shutdown_session(conversation_id).await;
    }

    fn supports_runtime_stop(&self) -> bool {
        true
    }

    async fn stop_runtime(&self, conversation_id: &str) -> Result<Value, AppError> {
        self.shutdown_session(conversation_id).await;
        Ok(json!({
            "provider": ProviderKind::DeepseekHarness.as_str(),
            "conversationId": conversation_id,
            "status": "stopped",
            "reason": "user_closed",
        }))
    }

    async fn shutdown(&self) {
        self.sessions.stop_all().await;
    }

    fn descriptor(&self) -> ProviderDescriptor {
        let available = executable_available(&self.binary);
        ProviderDescriptor {
            id: ProviderKind::DeepseekHarness,
            display_name: PROFILE.display_name,
            available,
            unavailable_reason: (!available).then(|| {
                format!(
                    "executable '{}' was not found; install it with `npm install -g @deepseek-ai/dsh` (requires Node.js)",
                    self.binary
                )
            }),
            profiles: Vec::new(),
            capabilities: PROFILE.capabilities(),
            models: Vec::new(),
        }
    }

    fn discovery_timeout(&self) -> Duration {
        DISCOVERY_TIMEOUT
    }

    async fn discover_models(
        &self,
        workspace: &Path,
    ) -> Result<Vec<ProviderModelDescriptor>, AppError> {
        Ok(self.discovery_snapshot(workspace).await?.0.models)
    }

    async fn discover_image_input(
        &self,
        workspace: &Path,
        _profile: Option<&str>,
    ) -> Result<bool, AppError> {
        Ok(self.discovery_snapshot(workspace).await?.1)
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
                |context, _prompt| {
                    Ok(ResidentLaunch {
                        spec: self.command_spec(&context.manifest.workspace)?,
                        runtime: self.runtime_options(),
                    })
                },
            )
            .await
    }
}

async fn control_request(
    process: &mut JsonLineProcess,
    id: &str,
    method: &str,
    params: Value,
    timeout: Option<Duration>,
) -> Result<Value, AppError> {
    RpcClient::new(process, DSH_RPC)
        .request(id, method, params, timeout)
        .await
}

fn dsh_environment(allowlist: &[String]) -> BTreeMap<String, String> {
    allowlisted_environment(&[("NO_COLOR", "1")], allowlist)
}

#[cfg(test)]
mod tests {
    #[cfg(unix)]
    use std::path::PathBuf;

    use super::*;

    const FLASH: &str = r#"["deepseek-official","deepseek-v4-flash"]"#;
    const PRO: &str = r#"["deepseek-official","deepseek-v4-pro"]"#;
    const PLAIN: &str = r#"["gateway","plain-chat"]"#;

    #[test]
    fn grouped_model_options_flatten_with_route_family() {
        let session = json!({"configOptions": [{
            "id": "model", "type": "select", "currentValue": PRO,
            "options": [
                {"group": "deepseek-official", "name": "DeepSeek", "options": [
                    {"value": FLASH, "name": "deepseek-v4-flash"},
                    {"value": PRO, "name": "DeepSeek-V4-Pro", "description": "Stronger"}]},
                {"value": PLAIN, "name": "plain-chat"},
                {"value": "", "name": "ignored"}
            ]
        }]});
        let models = parse_dsh_models(&session);
        let ids: Vec<_> = models.iter().map(|model| model.id.as_str()).collect();
        assert_eq!(ids, [FLASH, PRO, PLAIN]);
        assert_eq!(models[0].family.as_deref(), Some("deepseek-official"));
        assert_eq!(models[1].display_name, "DeepSeek-V4-Pro");
        assert_eq!(models[1].description, "Stronger");
        assert!(models[1].is_default && !models[0].is_default);
        assert_eq!(models[2].family, None);
    }

    #[test]
    fn provider_default_effort_is_not_a_level() {
        let mut model = parse_dsh_models(&json!({"configOptions": [{
            "id": "model", "currentValue": FLASH,
            "options": [{"value": FLASH, "name": "flash"}]
        }]}))
        .remove(0);
        apply_efforts(
            &mut model,
            &json!({"configOptions": [{
                "id": "reasoning_effort", "currentValue": "",
                "options": [{"value": ""}, {"value": "off"}, {"value": "max"}]
            }]}),
        );
        assert_eq!(model.supported_reasoning_efforts, ["off", "max"]);
        assert_eq!(model.default_reasoning_effort, None);
        // A model without reasoning has no effort option at all.
        apply_efforts(&mut model, &json!({"configOptions": []}));
        assert!(model.supported_reasoning_efforts.is_empty());
    }

    #[test]
    fn launch_disables_session_log_upload_through_patch_overlay() {
        let root = std::env::temp_dir().join(format!("todex-dsh-patch-{}", uuid::Uuid::new_v4()));
        let mut config = crate::config::Config::default().agent;
        config.dsh_bin = "dsh-test".to_owned();
        let driver = DeepseekHarnessDriver::new(&config, &root);
        let patch = root
            .join("providers")
            .join("deepseek-harness")
            .join("todex.patch.yml");
        std::fs::create_dir_all(patch.parent().unwrap()).unwrap();
        std::fs::write(&patch, "- id: session-log-deepseek\n  disabled: false\n").unwrap();
        let spec = driver.command_spec(&root).unwrap();
        assert_eq!(
            spec.args,
            ["--profile", "acp", "--patch", patch.to_str().unwrap()]
        );
        // An edited overlay is restored before launch.
        assert_eq!(std::fs::read_to_string(&patch).unwrap(), PATCH);
        assert!(PATCH.contains("- id: session-log-deepseek\n  disabled: true\n"));
        assert_eq!(spec.env.get("NO_COLOR").map(String::as_str), Some("1"));
        std::fs::remove_dir_all(root).unwrap();
    }

    #[cfg(unix)]
    struct Fixture {
        root: PathBuf,
        driver: std::sync::Arc<DeepseekHarnessDriver>,
        store: crate::conversation::ConversationStore,
        manifest: crate::conversation::ConversationManifest,
        trust: crate::workspace_trust::WorkspaceTrustStore,
    }

    #[cfg(unix)]
    impl Fixture {
        async fn new() -> Self {
            use std::os::unix::fs::PermissionsExt;
            let root =
                std::env::temp_dir().join(format!("todex-dsh-wire-{}", uuid::Uuid::new_v4()));
            std::fs::create_dir_all(&root).unwrap();
            let root = std::fs::canonicalize(root).unwrap();
            let binary = root.join("dsh-fixture");
            std::fs::write(
                &binary,
                include_str!("../../tests/fixtures/dsh_acp_fixture.py"),
            )
            .unwrap();
            std::fs::set_permissions(&binary, std::fs::Permissions::from_mode(0o755)).unwrap();
            let mut config = crate::config::Config::default().agent;
            config.dsh_bin = binary.to_string_lossy().into_owned();
            config.dsh_env_allowlist = Vec::new();
            let driver =
                std::sync::Arc::new(DeepseekHarnessDriver::new(&config, &root.join("data")));
            let store = crate::conversation::ConversationStore::new(root.join("data"))
                .await
                .unwrap();
            let manifest = store
                .create(crate::conversation::ConversationManifest::new(
                    ProviderKind::DeepseekHarness,
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
            model: Option<&str>,
            effort: Option<&str>,
            permission_mode: Option<&str>,
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
                reasoning_effort: effort.map(ToOwned::to_owned),
                permission_mode: permission_mode.map(ToOwned::to_owned),
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

        async fn run(
            &self,
            turn_id: &str,
            text: &str,
            model: Option<&str>,
            effort: Option<&str>,
            permission_mode: Option<&str>,
        ) -> DriverTurnResult {
            let (_cancel, task) = self
                .start(turn_id, text, model, effort, permission_mode)
                .await;
            tokio::time::timeout(Duration::from_secs(10), task)
                .await
                .unwrap()
                .unwrap()
                .unwrap()
        }

        async fn wait_for_method(&self, method: &str) {
            tokio::time::timeout(Duration::from_secs(5), async {
                while !self
                    .requests()
                    .iter()
                    .any(|request| request["method"] == method)
                {
                    tokio::time::sleep(Duration::from_millis(10)).await;
                }
            })
            .await
            .expect("fixture request deadline");
        }

        fn requests(&self) -> Vec<Value> {
            std::fs::read_to_string(self.root.join("dsh-requests.jsonl"))
                .unwrap_or_default()
                .lines()
                .filter_map(|line| serde_json::from_str(line).ok())
                .collect()
        }

        fn count(&self, method: &str) -> usize {
            self.requests()
                .iter()
                .filter(|request| request["method"] == method)
                .count()
        }

        async fn finish(self) {
            self.driver.shutdown().await;
            std::fs::remove_dir_all(self.root).unwrap();
        }
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn wire_turns_resume_across_process_restarts_without_session_load() {
        let fixture = Fixture::new().await;
        assert_eq!(
            fixture
                .run("turn-1", "hello", None, None, None)
                .await
                .stop_reason,
            "end_turn"
        );
        // Same resident process: the session is reused as-is.
        fixture.run("turn-2", "again", None, None, None).await;
        assert_eq!(fixture.count("session/new"), 1);
        assert_eq!(fixture.count("initialize"), 1);
        // A fresh process (idle reap, daemon restart) reattaches with resume.
        fixture.driver.shutdown_session(&fixture.manifest.id).await;
        fixture
            .run("turn-3", "after restart", None, None, None)
            .await;
        assert_eq!(fixture.count("initialize"), 2);
        assert_eq!(fixture.count("session/new"), 1);
        assert_eq!(fixture.count("session/load"), 0);
        assert_eq!(fixture.count("authenticate"), 0);
        let resume = fixture
            .requests()
            .into_iter()
            .find(|request| request["method"] == "session/resume")
            .unwrap();
        assert_eq!(resume["params"]["sessionId"], "dsh-session-1");
        assert_eq!(
            resume["argv"],
            json!([
                "--profile",
                "acp",
                "--patch",
                fixture
                    .root
                    .join("data/providers/deepseek-harness/todex.patch.yml")
            ])
        );
        let state = fixture
            .store
            .provider_state(&fixture.manifest.id)
            .await
            .unwrap();
        assert_eq!(state.native_session_id.as_deref(), Some("dsh-session-1"));
        assert!(state.recoverable);
        let events = fixture
            .store
            .complete_history(&fixture.manifest.id)
            .await
            .unwrap();
        assert_eq!(
            events
                .iter()
                .filter(|event| event.event_type == "message.delta")
                .count(),
            3
        );
        assert!(events
            .iter()
            .any(|event| event.event_type == "usage.updated"));
        fixture.finish().await;
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn wire_deleted_session_is_recreated() {
        let fixture = Fixture::new().await;
        let mut state = fixture
            .store
            .provider_state(&fixture.manifest.id)
            .await
            .unwrap();
        state.native_session_id = Some("gone".to_owned());
        fixture
            .store
            .save_provider_state(&fixture.manifest.id, state)
            .await
            .unwrap();
        assert_eq!(
            fixture
                .run("turn-1", "hello", None, None, None)
                .await
                .stop_reason,
            "end_turn"
        );
        assert_eq!(fixture.count("session/new"), 1);
        let state = fixture
            .store
            .provider_state(&fixture.manifest.id)
            .await
            .unwrap();
        assert_eq!(state.native_session_id.as_deref(), Some("dsh-session-1"));
        let events = fixture
            .store
            .complete_history(&fixture.manifest.id)
            .await
            .unwrap();
        assert!(events
            .iter()
            .any(|event| event.event_type == "provider.event"
                && event.payload["providerMethod"] == "session/recreated"));
        fixture.finish().await;
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn wire_applies_model_then_reasoning_effort() {
        let fixture = Fixture::new().await;
        fixture
            .run("turn-1", "hello", Some(PRO), Some("max"), None)
            .await;
        let writes: Vec<_> = fixture
            .requests()
            .into_iter()
            .filter(|request| request["method"] == "session/set_config_option")
            .map(|request| {
                (
                    request["params"]["configId"].clone(),
                    request["params"]["value"].clone(),
                )
            })
            .collect();
        assert_eq!(
            writes,
            [
                (json!("model"), json!(PRO)),
                (json!("reasoning_effort"), json!("max"))
            ]
        );
        let events = fixture
            .store
            .complete_history(&fixture.manifest.id)
            .await
            .unwrap();
        assert!(events
            .iter()
            .any(|event| event.event_type == "turn.configuration"
                && event.payload["effectiveConfig"]["model"] == PRO
                && event.payload["effectiveConfig"]["reasoningEffort"] == "max"));
        fixture.finish().await;
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn wire_probe_discovers_models_efforts_and_image_input() {
        let fixture = Fixture::new().await;
        let models = fixture.driver.discover_models(&fixture.root).await.unwrap();
        let ids: Vec<_> = models.iter().map(|model| model.id.as_str()).collect();
        assert_eq!(ids, [FLASH, PRO, PLAIN]);
        assert!(models[0].is_default);
        assert_eq!(
            models[0].supported_reasoning_efforts,
            ["off", "high", "max"]
        );
        assert_eq!(models[0].default_reasoning_effort.as_deref(), Some("high"));
        assert_eq!(
            models[1].supported_reasoning_efforts,
            ["off", "high", "max"]
        );
        assert!(models[2].supported_reasoning_efforts.is_empty());
        assert_eq!(models[2].family.as_deref(), Some("gateway"));
        assert!(!fixture
            .driver
            .discover_image_input(&fixture.root, None)
            .await
            .unwrap());
        // One cached probe answers both catalogs and closes its session.
        assert_eq!(fixture.count("initialize"), 1);
        assert_eq!(fixture.count("session/close"), 1);
        // The default model's efforts come from session/new without a write.
        assert_eq!(fixture.count("session/set_config_option"), 2);
        fixture.finish().await;
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn wire_auto_mode_answers_permission_requests_client_side() {
        let fixture = Fixture::new().await;
        fixture
            .run("turn-auto", "permission", None, None, Some("auto"))
            .await;
        let decision: Value = serde_json::from_str(
            &std::fs::read_to_string(fixture.root.join("dsh-permission.json")).unwrap(),
        )
        .unwrap();
        assert_eq!(decision["optionId"], "allow-once");
        let events = fixture
            .store
            .complete_history(&fixture.manifest.id)
            .await
            .unwrap();
        assert!(events
            .iter()
            .any(|event| event.event_type == "permission.resolved"
                && event.payload["autoApproved"] == true));
        fixture.finish().await;
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn wire_ask_mode_waits_for_the_user_and_cancel_ends_the_turn() {
        let fixture = Fixture::new().await;
        let (cancel, task) = fixture
            .start("turn-ask", "permission", None, None, Some("ask"))
            .await;
        fixture.wait_for_method("session/prompt").await;
        tokio::time::sleep(Duration::from_millis(200)).await;
        assert!(!fixture.root.join("dsh-permission.json").exists());
        let events = fixture
            .store
            .complete_history(&fixture.manifest.id)
            .await
            .unwrap();
        assert!(events
            .iter()
            .any(|event| event.event_type == "permission.requested"));
        cancel.send(true).unwrap();
        assert!(task.await.unwrap().unwrap().cancelled);
        fixture.finish().await;
    }

    #[tokio::test]
    #[ignore = "opt-in: runs the real dsh CLI (no credentials needed); TODEX_DSH_LIVE_BIN selects it"]
    async fn live_dsh_discovery_lists_models_with_efforts() {
        let root = std::env::temp_dir().join(format!("todex-dsh-live-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&root).unwrap();
        let mut config = crate::config::Config::default().agent;
        if let Ok(binary) = std::env::var("TODEX_DSH_LIVE_BIN") {
            config.dsh_bin = binary;
        }
        let driver = DeepseekHarnessDriver::new(&config, &root);
        let models = driver.discover_models(&root).await.unwrap();
        eprintln!("{}", serde_json::to_string_pretty(&models).unwrap());
        assert!(models.iter().any(|model| model.is_default));
        assert!(models
            .iter()
            .any(|model| !model.supported_reasoning_efforts.is_empty()));
        assert!(models
            .iter()
            .flat_map(|model| &model.supported_reasoning_efforts)
            .all(|effort| !effort.is_empty()));
        std::fs::remove_dir_all(root).unwrap();
    }
}
