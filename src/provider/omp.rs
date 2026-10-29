//! omp (oh-my-pi, a fork of Pi) over `omp acp`.
//!
//! omp's RPC mode has drifted from Pi's (no `--session-id`, `agent_settled`,
//! `get_commands`, ...), so TodeX drives its ACP server instead: one resident
//! process per conversation, as for OpenCode.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::Duration;

use async_trait::async_trait;
use serde_json::{json, Value};
use tokio::sync::watch;

use crate::config::AgentConfig;
use crate::conversation::{ProviderKind, ProviderState};
use crate::error::AppError;

use super::acp::{AcpQuirks, AcpRuntimeOptions, PromptUsage, PLAIN_ACP};
use super::discovery::{DiscoveryCache, DiscoveryKey, DiscoverySnapshot};
use super::opencode::{close_probe_session, probe_efforts};
use super::process::{executable_available, CommandSpec, JsonLineProcess};
use super::profile::{
    CatalogProfile, CatalogSource, ConfigHome, FileAttachmentStyle, McpInjection, ProcessModel,
    ProviderProfile, SkillInjection, UserConfigFile,
};
use super::resident::{allowlisted_environment, initialize_acp, ResidentLaunch, ResidentSessions};
use super::rpc::{FailureWording, RpcClient, RpcPeer};
use super::types::{
    DriverContext, DriverEventSink, DriverPrompt, DriverTurnResult, ImageInputMode,
    PermissionConfigCapabilities, ProviderCommandDescriptor, ProviderControl, ProviderDescriptor,
    ProviderDriver, ProviderModelDescriptor, ProviderPrewarm,
};

const OMP_RPC: RpcPeer = RpcPeer {
    name: "omp",
    failure: FailureWording::Method,
    closed: "omp closed stdout during control request",
    decline: Some("client capability is not supported during control request"),
    jsonrpc_field: true,
    result_optional: false,
    classify_error: None,
};
/// `available_commands_update` follows the `session/new` response after
/// omp's bootstrap delay.
const COMMAND_DRAIN: Duration = Duration::from_millis(2500);
/// A cold probe spawns the Bun binary, loads the model catalog in
/// `session/new` and selects every model once for its thinking levels.
const DISCOVERY_TIMEOUT: Duration = Duration::from_secs(30);
/// Pi's agent directory override; omp reads the same variable, so a value
/// meant for Pi would point omp at Pi's directory.
const PI_AGENT_DIR_ENV: &str = "PI_CODING_AGENT_DIR";
const CONFIG_OVERLAY_FILE: &str = "todex.config.yml";
/// Settings overlay (`omp acp --config`) for TodeX's launches. omp's ACP
/// server forwards nothing once a prompt has returned, so a background task
/// or auto-backgrounded command would finish, and omp would answer on it,
/// where TodeX never sees it: background execution stays in the turn.
const CONFIG_OVERLAY: &str = "\
# Written by TodeX for the omp processes it launches; rewritten before each launch.
async:
  enabled: false
bash:
  autoBackground:
    enabled: false
eval:
  autoBackground:
    enabled: false
";

/// Where omp deviates from plain ACP.
pub(super) const ACP_QUIRKS: AcpQuirks = AcpQuirks {
    client_side_approval: true,
    prefer_resume: true,
    control_commands: Some(super::acp::omp_control_commands),
    prompt_usage: PromptUsage::ResultUsage,
    command_updates: true,
    usage_updates: true,
    mode_updates: true,
    effort_config: "thinking",
    session_mode: Some(super::acp::omp_session_mode),
    steer_acknowledged: super::acp::omp_steer_acknowledged,
    form_elicitation: true,
    task_subagents: true,
    ..PLAIN_ACP
};

/// What omp supports and how TodeX adapts to it.
pub(super) const PROFILE: ProviderProfile = ProviderProfile {
    kind: ProviderKind::Omp,
    display_name: "omp",
    permission_config: PermissionConfigCapabilities {
        modes: &["ask", "auto", "full-access"],
        default_mode: "auto",
        supports_plan: true,
        sandbox_modes: &[],
        approval_policies: &[],
        permission_profiles: &[],
        enforcement: "agent-policy",
        description: "omp runs with --approval-mode always-ask: ask mediates each write and exec approval, auto/full-access approve client-side; plan maps to omp's plan mode",
    },
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
    discovery_cache_ttl: Some(Duration::from_secs(300)),
    // omp also reads other tools' MCP configs and its `disabledServers`
    // list; the catalog shows and edits only omp's own files.
    catalog: CatalogProfile {
        source: CatalogSource::Filesystem,
        config_home: ConfigHome {
            env: None,
            home_relative: ".omp/agent",
        },
        project_skills: ".omp/skills",
        mcp_user_files: &[UserConfigFile::ConfigHome("mcp.json")],
        mcp_project_files: &[".omp/mcp.json"],
        mcp_user_source: "omp-user",
        mcp_project_source: "omp-project",
    },
};

pub struct OmpDriver {
    binary: String,
    env_allowlist: Vec<String>,
    overlay_path: PathBuf,
    sessions: ResidentSessions,
    discovery: DiscoveryCache<DiscoveryKey, DiscoverySnapshot>,
}

impl OmpDriver {
    pub fn new(config: &AgentConfig, data_dir: &Path) -> Self {
        Self {
            binary: config.omp_bin.clone(),
            env_allowlist: config.omp_env_allowlist.clone(),
            overlay_path: overlay_path(data_dir),
            // omp reads models.yml, plugins and mcp.json only at startup.
            sessions: ResidentSessions::new(&PROFILE, "omp")
                .with_idle_timeout(config.provider_keepalive())
                .with_config_restart(),
            discovery: DiscoveryCache::new(),
        }
    }

    /// How a conversation's resident process is started.
    fn resident_launch(
        &self,
        context: &DriverContext,
        _prompt: &DriverPrompt,
    ) -> Result<ResidentLaunch, AppError> {
        Ok(ResidentLaunch {
            spec: self.command_spec(&context.manifest.workspace)?,
            runtime: AcpRuntimeOptions {
                suppress_load_replay: true,
                ..Default::default()
            },
        })
    }

    /// `always-ask` makes omp request every write and exec, so the TodeX
    /// permission mode of each turn decides them (omp's own default would
    /// run most calls unasked). omp keeps credentials in its own store; its
    /// `agent` auth method only names them, so `authenticate` is never sent.
    fn command_spec(&self, workspace: &Path) -> Result<CommandSpec, AppError> {
        write_overlay(&self.overlay_path)?;
        let mut spec = CommandSpec::new(&self.binary, workspace);
        spec.args = ["acp", "--approval-mode", "always-ask", "--config"]
            .map(str::to_owned)
            .to_vec();
        spec.args
            .push(self.overlay_path.to_string_lossy().into_owned());
        spec.env = omp_environment(&self.env_allowlist);
        spec.env_remove = vec![PI_AGENT_DIR_ENV.to_owned()];
        Ok(spec)
    }

    /// Models and slash commands live behind `session/new`. The session is
    /// left open for the per-model thinking probe, then closed.
    async fn session_probe(
        &self,
        workspace: &Path,
    ) -> Result<(JsonLineProcess, Value, Vec<Value>), AppError> {
        let mut process = JsonLineProcess::spawn(&self.command_spec(workspace)?).await?;
        let result = async {
            initialize_acp(&mut process, OMP_RPC, None, None).await?;
            let mut updates = Vec::new();
            let response = RpcClient::new(&mut process, OMP_RPC)
                .request_collecting(
                    "session",
                    "session/new",
                    json!({ "cwd": workspace, "mcpServers": [] }),
                    None,
                    &mut updates,
                )
                .await?;
            response
                .get("sessionId")
                .and_then(Value::as_str)
                .filter(|id| !id.is_empty())
                .ok_or_else(|| {
                    AppError::InvalidRequest("invalid omp session/new response".to_owned())
                })?;
            let deadline = tokio::time::Instant::now() + COMMAND_DRAIN;
            loop {
                let read = tokio::time::timeout_at(deadline, process.read()).await;
                let Ok(Ok(Some(message))) = read else {
                    break;
                };
                if message.get("method").and_then(Value::as_str) == Some("session/update") {
                    let update = message.get("params").cloned().unwrap_or(Value::Null);
                    let is_commands = update
                        .pointer("/update/sessionUpdate")
                        .and_then(Value::as_str)
                        == Some("available_commands_update");
                    updates.push(update);
                    if is_commands {
                        break;
                    }
                } else if let Some(request_id) =
                    message.get("id").filter(|_| message.get("method").is_some())
                {
                    process
                        .send(&json!({"jsonrpc":"2.0","id":request_id,"error":{"code":-32601,"message":"client capability is not supported during probe"}}))
                        .await?;
                }
            }
            Ok((response, updates))
        }
        .await;
        match result {
            Ok((response, updates)) => Ok((process, response, updates)),
            Err(error) => {
                process.terminate().await;
                Err(error)
            }
        }
    }

    /// One cached probe per workspace yields both catalogs.
    async fn discovery_snapshot(&self, workspace: &Path) -> Result<DiscoverySnapshot, AppError> {
        let key = DiscoveryKey::new(&self.binary, workspace).await;
        self.discovery
            .get_or_fetch(&key, || async {
                let (mut process, session, updates) = self.session_probe(workspace).await?;
                let mut models = super::devin::parse_devin_models(&session);
                // Selecting a model over ACP does not persist it to omp's config.
                let complete =
                    probe_efforts(&mut process, OMP_RPC, "thinking", &session, &mut models).await;
                close_probe_session(&mut process, OMP_RPC, &session).await;
                process.terminate().await;
                let commands = updates
                    .iter()
                    .rev()
                    .find_map(|update| {
                        update
                            .pointer("/update/availableCommands")
                            .filter(|commands| commands.is_array())
                            .cloned()
                    })
                    .map(|commands| {
                        super::grok::parse_commands(
                            &json!({ "_meta": { "availableCommands": commands } }),
                        )
                    })
                    .unwrap_or_default();
                let snapshot = DiscoverySnapshot {
                    models,
                    commands,
                    complete,
                };
                let ttl = snapshot.ttl(PROFILE.discovery_cache_ttl.unwrap());
                Ok((snapshot, ttl))
            })
            .await
    }
}

#[async_trait]
impl ProviderDriver for OmpDriver {
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
        Ok(
            json!({"provider":"omp","conversationId":conversation_id,"status":"stopped","reason":"user_closed"}),
        )
    }

    async fn shutdown(&self) {
        self.sessions.stop_all().await;
    }

    fn descriptor(&self) -> ProviderDescriptor {
        let available = executable_available(&self.binary);
        ProviderDescriptor {
            id: ProviderKind::Omp,
            display_name: PROFILE.display_name,
            available,
            unavailable_reason: (!available).then(|| {
                format!(
                    "executable '{}' was not found; install omp from https://omp.sh",
                    self.binary
                )
            }),
            profiles: Vec::new(),
            capabilities: PROFILE.capabilities(),
            models: Vec::new(),
        }
    }

    fn supports_native_fork(&self) -> bool {
        true
    }

    async fn fork_session(&self, context: DriverContext) -> Result<ProviderState, AppError> {
        let source = context
            .provider_state
            .native_session_id
            .as_deref()
            .ok_or_else(|| AppError::InvalidRequest("omp session has not started".to_owned()))?;
        let mut process =
            JsonLineProcess::spawn(&self.command_spec(&context.manifest.workspace)?).await?;
        let result = async {
            initialize_acp(&mut process, OMP_RPC, None, None).await?;
            let response = RpcClient::new(&mut process, OMP_RPC)
                .request(
                    "fork",
                    "session/fork",
                    json!({"sessionId": source, "cwd": context.manifest.workspace, "mcpServers": []}),
                    None,
                )
                .await?;
            let id = response
                .get("sessionId")
                .and_then(Value::as_str)
                .filter(|id| !id.is_empty() && *id != source)
                .ok_or_else(|| {
                    AppError::InvalidRequest(
                        "invalid omp fork response: missing distinct sessionId".to_owned(),
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

    fn discovery_timeout(&self) -> Duration {
        DISCOVERY_TIMEOUT
    }

    async fn discover_models(
        &self,
        workspace: &Path,
    ) -> Result<Vec<ProviderModelDescriptor>, AppError> {
        Ok(self.discovery_snapshot(workspace).await?.models)
    }

    async fn discover_commands(
        &self,
        workspace: &Path,
    ) -> Result<Vec<ProviderCommandDescriptor>, AppError> {
        Ok(self.discovery_snapshot(workspace).await?.commands)
    }

    async fn prewarm(
        &self,
        context: DriverContext,
        settings: DriverPrompt,
        sink: DriverEventSink,
    ) -> Result<ProviderPrewarm, AppError> {
        self.sessions
            .prewarm(context, settings, sink, |context, prompt| {
                self.resident_launch(context, prompt)
            })
            .await
    }

    async fn run_turn(
        &self,
        context: DriverContext,
        prompt: DriverPrompt,
        sink: DriverEventSink,
        cancel: watch::Receiver<bool>,
    ) -> Result<DriverTurnResult, AppError> {
        self.sessions
            .run_turn(context, prompt, sink, cancel, |context, prompt| {
                self.resident_launch(context, prompt)
            })
            .await
    }
}

fn overlay_path(data_dir: &Path) -> PathBuf {
    data_dir
        .join("providers")
        .join(ProviderKind::Omp.as_str())
        .join(CONFIG_OVERLAY_FILE)
}

fn write_overlay(path: &Path) -> Result<(), AppError> {
    let parent = path.parent().ok_or_else(|| {
        AppError::ProviderUnavailable("omp settings overlay path has no parent".to_owned())
    })?;
    crate::secure_fs::ensure_owner_only_dir(parent)?;
    crate::secure_fs::write_owner_only_atomic_if_changed(path, CONFIG_OVERLAY.as_bytes())?;
    Ok(())
}

/// `PI_NO_TITLE` skips the extra model call omp spends on a session title;
/// TodeX titles conversations itself.
fn omp_environment(allowlist: &[String]) -> BTreeMap<String, String> {
    allowlisted_environment(&[("NO_COLOR", "1"), ("PI_NO_TITLE", "1")], allowlist)
}

#[cfg(test)]
mod tests {
    #[cfg(unix)]
    use std::path::PathBuf;

    use super::*;

    #[test]
    fn launch_asks_for_every_write_keeps_work_in_the_turn_and_drops_pi_agent_dir() {
        let data_dir =
            std::env::temp_dir().join(format!("todex-omp-launch-{}", uuid::Uuid::new_v4()));
        let driver = OmpDriver::new(&crate::config::Config::default().agent, &data_dir);
        let spec = driver.command_spec(Path::new("/workspace")).unwrap();
        let overlay = data_dir.join("providers/omp/todex.config.yml");
        assert_eq!(
            spec.args,
            [
                "acp",
                "--approval-mode",
                "always-ask",
                "--config",
                &overlay.to_string_lossy()
            ]
        );
        assert_eq!(std::fs::read_to_string(&overlay).unwrap(), CONFIG_OVERLAY);
        // An edited overlay is restored before the next launch.
        std::fs::write(&overlay, "async:\n  enabled: true\n").unwrap();
        driver.command_spec(Path::new("/workspace")).unwrap();
        assert_eq!(std::fs::read_to_string(&overlay).unwrap(), CONFIG_OVERLAY);
        let parsed: serde_json::Value = serde_yaml_ng::from_str(CONFIG_OVERLAY).unwrap();
        for pointer in [
            "/async/enabled",
            "/bash/autoBackground/enabled",
            "/eval/autoBackground/enabled",
        ] {
            assert_eq!(parsed.pointer(pointer), Some(&json!(false)), "{pointer}");
        }
        assert_eq!(spec.env_remove, [PI_AGENT_DIR_ENV]);
        assert_eq!(spec.env.get("PI_NO_TITLE").map(String::as_str), Some("1"));
        assert_eq!(spec.env.get("NO_COLOR").map(String::as_str), Some("1"));
        std::fs::remove_dir_all(data_dir).unwrap();
    }

    #[test]
    fn steering_counts_only_injected_messages() {
        assert!(super::super::acp::omp_steer_acknowledged(
            &json!({"outcome":"injected","queue":"steering"})
        ));
        assert!(!super::super::acp::omp_steer_acknowledged(
            &json!({"outcome":"promptRequired","queue":"steering"})
        ));
        assert!(!super::super::acp::omp_steer_acknowledged(
            &json!({"status":"queued"})
        ));
    }

    #[cfg(unix)]
    struct Fixture {
        root: PathBuf,
        driver: std::sync::Arc<OmpDriver>,
        store: crate::conversation::ConversationStore,
        manifest: crate::conversation::ConversationManifest,
    }

    #[cfg(unix)]
    impl Fixture {
        async fn new() -> Self {
            use std::os::unix::fs::PermissionsExt;
            let root =
                std::env::temp_dir().join(format!("todex-omp-wire-{}", uuid::Uuid::new_v4()));
            std::fs::create_dir_all(&root).unwrap();
            let root = std::fs::canonicalize(root).unwrap();
            let binary = root.join("omp-fixture");
            std::fs::write(
                &binary,
                include_str!("../../tests/fixtures/omp_acp_fixture.py"),
            )
            .unwrap();
            std::fs::set_permissions(&binary, std::fs::Permissions::from_mode(0o755)).unwrap();
            let driver = std::sync::Arc::new(OmpDriver {
                binary: binary.to_string_lossy().to_string(),
                env_allowlist: vec![],
                overlay_path: overlay_path(&root.join("data")),
                sessions: ResidentSessions::new(&PROFILE, "omp"),
                discovery: DiscoveryCache::new(),
            });
            let store = crate::conversation::ConversationStore::new(root.join("data"))
                .await
                .unwrap();
            let manifest = store
                .create(crate::conversation::ConversationManifest::new(
                    ProviderKind::Omp,
                    root.clone(),
                    None,
                    None,
                ))
                .await
                .unwrap();
            Self {
                root,
                driver,
                store,
                manifest,
            }
        }

        async fn start(
            &self,
            turn_id: &str,
            text: &str,
            model: Option<&str>,
            effort: Option<&str>,
            permission_mode: Option<&str>,
            work_mode: Option<&str>,
        ) -> (
            watch::Sender<bool>,
            tokio::task::JoinHandle<Result<DriverTurnResult, AppError>>,
        ) {
            let context = DriverContext {
                manifest: self.manifest.clone(),
                provider_state: self.store.provider_state(&self.manifest.id).await.unwrap(),
                agent_mcp: None,
                handles: Default::default(),
            };
            let prompt = DriverPrompt {
                turn_id: turn_id.to_owned(),
                text: text.to_owned(),
                content: vec![],
                skills: vec![],
                model: model.map(ToOwned::to_owned),
                reasoning_effort: effort.map(ToOwned::to_owned),
                permission_mode: permission_mode.map(ToOwned::to_owned),
                work_mode: work_mode.map(ToOwned::to_owned),
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
            let driver = self.driver.clone();
            let (cancel, receiver) = watch::channel(false);
            let task =
                tokio::spawn(async move { driver.run_turn(context, prompt, sink, receiver).await });
            (cancel, task)
        }

        async fn finished(
            task: tokio::task::JoinHandle<Result<DriverTurnResult, AppError>>,
        ) -> DriverTurnResult {
            tokio::time::timeout(Duration::from_secs(5), task)
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
            std::fs::read_to_string(self.root.join("omp-requests.jsonl"))
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

        fn config_writes(&self) -> Vec<Value> {
            self.requests()
                .into_iter()
                .filter(|request| request["method"] == "session/set_config_option")
                .collect()
        }

        async fn events(&self) -> Vec<crate::conversation::ConversationEvent> {
            self.store
                .complete_history(&self.manifest.id)
                .await
                .unwrap()
        }

        async fn finish(self) {
            self.driver.shutdown().await;
            std::fs::remove_dir_all(self.root).unwrap();
        }
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn wire_turns_reuse_resident_session_without_authenticate() {
        let fixture = Fixture::new().await;
        for turn in ["turn-first", "turn-second"] {
            let (_cancel, task) = fixture.start(turn, "normal", None, None, None, None).await;
            assert_eq!(Fixture::finished(task).await.stop_reason, "end_turn");
        }
        assert_eq!(fixture.count("initialize"), 1);
        assert_eq!(fixture.count("authenticate"), 0);
        assert_eq!(fixture.count("session/new"), 1);
        let first = &fixture.requests()[0];
        let argv = first["argv"].as_array().unwrap();
        assert_eq!(
            argv[..4],
            [
                json!("acp"),
                json!("--approval-mode"),
                json!("always-ask"),
                json!("--config")
            ]
        );
        assert!(argv[4]
            .as_str()
            .unwrap()
            .ends_with("providers/omp/todex.config.yml"));
        // Tool approvals and questions arrive as form elicitations.
        assert_eq!(
            first["params"]["clientCapabilities"]["elicitation"],
            json!({"form": {}})
        );
        assert_eq!(first["noTitle"], "1");
        // omp already reports the default mode, so nothing is re-sent.
        assert!(fixture.config_writes().is_empty());
        let events = fixture.events().await;
        assert_eq!(
            events
                .iter()
                .filter(|event| event.event_type == "message.delta")
                .count(),
            2
        );
        assert!(events
            .iter()
            .any(|event| event.event_type == "usage.updated"));
        assert!(events
            .iter()
            .any(|event| event.event_type == "turn.configuration"
                && event.payload["effectiveConfig"]["mode"] == "default"));
        fixture.finish().await;
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn wire_applies_model_thinking_and_plan_then_resets_to_default() {
        let fixture = Fixture::new().await;
        let (_cancel, task) = fixture
            .start(
                "turn-plan",
                "normal",
                Some("anthropic/thinker"),
                Some("high"),
                None,
                Some("plan"),
            )
            .await;
        assert_eq!(Fixture::finished(task).await.stop_reason, "end_turn");
        let writes = fixture.config_writes();
        for (id, value) in [
            ("model", "anthropic/thinker"),
            ("thinking", "high"),
            ("mode", "plan"),
        ] {
            assert!(
                writes
                    .iter()
                    .any(|request| request["params"]["configId"] == id
                        && request["params"]["value"] == value),
                "missing {id}={value}"
            );
        }
        assert!(fixture
            .events()
            .await
            .iter()
            .any(|event| event.event_type == "turn.configuration"
                && event.payload["effectiveConfig"]["reasoningEffort"] == "high"));

        let (_cancel, task) = fixture
            .start("turn-implement", "normal", None, None, None, None)
            .await;
        assert_eq!(Fixture::finished(task).await.stop_reason, "end_turn");
        assert_eq!(
            fixture
                .config_writes()
                .iter()
                .filter(|request| request["params"]["configId"] == "mode"
                    && request["params"]["value"] == "default")
                .count(),
            1
        );
        fixture.finish().await;
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn wire_auto_mode_answers_permission_request_zero_client_side() {
        for mode in ["auto", "full-access"] {
            let fixture = Fixture::new().await;
            let (_cancel, task) = fixture
                .start("turn-auto", "permission", None, None, Some(mode), None)
                .await;
            assert_eq!(Fixture::finished(task).await.stop_reason, "end_turn");
            // omp's first request has JSON-RPC id 0.
            let response = fixture
                .requests()
                .into_iter()
                .find(|request| request["id"] == 0 && request.get("result").is_some())
                .expect("permission response recorded");
            assert_eq!(
                response["result"]["outcome"]["optionId"].as_str(),
                // Not `allow_always`: omp would keep that grant for later
                // turns of the resident session, including `ask` ones.
                Some("allow_once")
            );
            assert!(fixture
                .events()
                .await
                .iter()
                .any(|event| event.event_type == "permission.resolved"
                    && event.payload["autoApproved"] == true));
            fixture.finish().await;
        }
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn wire_ask_mode_brokers_permission_requests() {
        let fixture = Fixture::new().await;
        let (cancel, task) = fixture
            .start("turn-ask", "permission", None, None, Some("ask"), None)
            .await;
        fixture.wait_for_method("session/prompt").await;
        tokio::time::sleep(Duration::from_millis(200)).await;
        assert!(fixture
            .requests()
            .iter()
            .all(|request| request["id"] != 0 || request.get("result").is_none()));
        assert!(fixture
            .events()
            .await
            .iter()
            .any(|event| event.event_type == "permission.requested"));
        cancel.send(true).unwrap();
        assert!(task.await.unwrap().unwrap().cancelled);
        fixture.finish().await;
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn wire_live_steer_and_thinking_are_provider_confirmed() {
        let fixture = Fixture::new().await;
        let (_cancel, task) = fixture
            .start(
                "turn-live",
                "hold",
                Some("anthropic/thinker"),
                None,
                None,
                None,
            )
            .await;
        fixture.wait_for_method("session/prompt").await;
        let thinking = fixture
            .driver
            .control(
                &fixture.manifest.id,
                "turn-live",
                "config-1",
                ProviderControl::Configure {
                    model: None,
                    reasoning_effort: Some("low".to_owned()),
                },
            )
            .await
            .unwrap();
        assert_eq!(thinking["effectiveConfig"]["reasoningEffort"], "low");
        let steer = fixture
            .driver
            .control(
                &fixture.manifest.id,
                "turn-live",
                "steer-1",
                ProviderControl::Steer {
                    text: "also check tests".to_owned(),
                },
            )
            .await
            .unwrap();
        assert_eq!(steer["result"]["outcome"], "injected");
        assert!(fixture
            .requests()
            .iter()
            .any(|request| request["method"] == "_session/steering"
                && request["params"]["prompt"][0]["text"] == "also check tests"));
        assert_eq!(Fixture::finished(task).await.stop_reason, "end_turn");
        fixture.finish().await;
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn wire_probe_discovers_models_thinking_levels_and_commands() {
        let fixture = Fixture::new().await;
        let models = fixture.driver.discover_models(&fixture.root).await.unwrap();
        assert_eq!(models.len(), 2);
        assert_eq!(models[0].id, "mock/fixture");
        assert!(models[0].is_default);
        assert_eq!(models[0].supported_reasoning_efforts, ["off", "auto"]);
        assert_eq!(
            models[1].supported_reasoning_efforts,
            ["off", "low", "high", "auto"]
        );
        let commands = fixture
            .driver
            .discover_commands(&fixture.root)
            .await
            .unwrap();
        assert!(commands.iter().any(|command| command.name == "compact"));
        assert_eq!(fixture.count("session/close"), 1);
        assert_eq!(fixture.count("initialize"), 1);
        fixture.finish().await;
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn wire_resume_and_fork_use_native_sessions() {
        let fixture = Fixture::new().await;
        let (_cancel, task) = fixture
            .start("turn-source", "normal", None, None, None, None)
            .await;
        assert_eq!(Fixture::finished(task).await.stop_reason, "end_turn");
        fixture.driver.shutdown_session(&fixture.manifest.id).await;
        let (_cancel, task) = fixture
            .start("turn-resume", "normal", None, None, None, None)
            .await;
        assert_eq!(Fixture::finished(task).await.stop_reason, "end_turn");
        assert_eq!(fixture.count("session/resume"), 1);
        assert_eq!(fixture.count("session/load"), 0);

        let context = DriverContext {
            manifest: fixture.manifest.clone(),
            provider_state: fixture
                .store
                .provider_state(&fixture.manifest.id)
                .await
                .unwrap(),
            agent_mcp: None,
            handles: Default::default(),
        };
        let state = fixture.driver.fork_session(context).await.unwrap();
        assert_eq!(state.native_session_id.as_deref(), Some("01a1816a-forked"));
        assert!(fixture
            .requests()
            .iter()
            .any(|request| request["method"] == "session/fork"
                && request["params"]["sessionId"] == "01a1816a-fixture"));
        fixture.finish().await;
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn wire_approval_elicitations_follow_the_permission_mode() {
        for mode in ["auto", "full-access"] {
            let fixture = Fixture::new().await;
            let (_cancel, task) = fixture
                .start("turn-approve", "approve", None, None, Some(mode), None)
                .await;
            assert_eq!(Fixture::finished(task).await.stop_reason, "end_turn");
            let answer = fixture
                .requests()
                .into_iter()
                .find(|request| request["id"] == 1 && request.get("result").is_some())
                .expect("elicitation answer recorded");
            // `Approve` only: the session-wide grant would outlive a switch to ask.
            assert_eq!(
                answer["result"],
                json!({"action": "accept", "content": {"value": "Approve"}})
            );
            assert!(fixture
                .events()
                .await
                .iter()
                .any(|event| event.event_type == "permission.resolved"
                    && event.payload["autoApproved"] == true));
            fixture.finish().await;
        }
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn wire_ask_mode_brokers_approval_elicitations_and_cancel_answers_cancel() {
        let fixture = Fixture::new().await;
        let (cancel, task) = fixture
            .start("turn-approve-ask", "approve", None, None, Some("ask"), None)
            .await;
        fixture.wait_for_method("session/prompt").await;
        tokio::time::sleep(Duration::from_millis(200)).await;
        let requested = fixture
            .events()
            .await
            .into_iter()
            .find(|event| event.event_type == "permission.requested")
            .expect("approval brokered");
        let kinds: Vec<_> = requested.payload["options"]
            .as_array()
            .unwrap()
            .iter()
            .map(|option| (option["optionId"].clone(), option["kind"].clone()))
            .collect();
        assert_eq!(
            kinds,
            [
                (json!("Approve"), json!("allow_once")),
                (
                    json!("Approve write for this session"),
                    json!("allow_always")
                ),
                (json!("Deny"), json!("reject_once")),
            ]
        );
        cancel.send(true).unwrap();
        assert!(task.await.unwrap().unwrap().cancelled);
        let answer = fixture
            .requests()
            .into_iter()
            .find(|request| request["id"] == 1 && request.get("result").is_some())
            .expect("elicitation answer recorded");
        assert_eq!(answer["result"], json!({"action": "cancel"}));
        fixture.finish().await;
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn wire_questions_go_to_the_user_even_in_full_access() {
        let fixture = Fixture::new().await;
        let (cancel, task) = fixture
            .start(
                "turn-question",
                "question",
                None,
                None,
                Some("full-access"),
                None,
            )
            .await;
        fixture.wait_for_method("session/prompt").await;
        tokio::time::sleep(Duration::from_millis(200)).await;
        assert!(fixture
            .requests()
            .iter()
            .all(|request| request["id"] != 1 || request.get("result").is_none()));
        assert!(fixture
            .events()
            .await
            .iter()
            .any(|event| event.event_type == "permission.requested"
                && event.payload["kind"] == "elicitation"));
        cancel.send(true).unwrap();
        assert!(task.await.unwrap().unwrap().cancelled);
        fixture.finish().await;
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn wire_task_subagents_become_subagent_runs_beside_the_tool_card() {
        let fixture = Fixture::new().await;
        let (_cancel, task) = fixture
            .start("turn-spawn", "spawn", None, None, None, None)
            .await;
        assert_eq!(Fixture::finished(task).await.stop_reason, "end_turn");
        let events = fixture.events().await;
        let subagent: Vec<(String, String, String)> = events
            .iter()
            .filter(|event| event.event_type.starts_with("subagent."))
            .map(|event| {
                (
                    event.event_type.clone(),
                    event.payload["subagentId"].as_str().unwrap().to_owned(),
                    event.payload["status"]
                        .as_str()
                        .unwrap_or_default()
                        .to_owned(),
                )
            })
            .collect();
        let s = |event: &str, id: &str, status: &str| {
            (
                event.to_owned(),
                format!("call_task/{id}"),
                status.to_owned(),
            )
        };
        assert_eq!(
            subagent,
            [
                s("subagent.started", "Alpha", "running"),
                s("subagent.updated", "Alpha", "running"),
                s("subagent.started", "Beta", "running"),
                s("subagent.updated", "Beta", "running"),
                // The repeated progress frame emits nothing.
                s("subagent.updated", "Alpha", "running"),
                s("subagent.updated", "Beta", "running"),
                s("subagent.completed", "Alpha", "completed"),
                s("subagent.failed", "Beta", "failed"),
            ]
        );
        let started = events
            .iter()
            .find(|event| event.event_type == "subagent.started")
            .unwrap();
        assert_eq!(started.payload["title"], "Alpha");
        assert_eq!(started.payload["task"], "List files");
        assert_eq!(started.payload["providerItemId"], "call_task");
        let completed = events
            .iter()
            .find(|event| event.event_type == "subagent.completed")
            .unwrap();
        assert_eq!(completed.payload["result"], "a.txt b.txt");
        let failed = events
            .iter()
            .find(|event| event.event_type == "subagent.failed")
            .unwrap();
        assert_eq!(failed.payload["error"], "model refused");
        // The task card itself is still journalled.
        assert!(events.iter().any(|event| event.event_type == "tool.started"
            && event.payload["toolCallId"] == "call_task"));
        fixture.finish().await;
    }
}
