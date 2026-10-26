use async_trait::async_trait;
use chrono::{DateTime, Datelike, Days, LocalResult, TimeZone, Utc, Weekday};
use chrono_tz::Tz;
use serde_json::{json, Map, Value};
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;
use tokio::io::AsyncBufReadExt;
use tokio::sync::{mpsc, watch, Mutex};
use tokio::time::Instant;

use crate::config::AgentConfig;
use crate::conversation::ProviderKind;
use crate::error::AppError;

use super::discovery::CatalogCache;
use super::process::{executable_available, provider_exit_error, CommandSpec, JsonLineProcess};
use super::profile::{
    CatalogProfile, CatalogSource, ConfigHome, FileAttachmentStyle, McpInjection, ProcessModel,
    ProviderProfile, SkillInjection, UserConfigFile,
};
use super::types::{
    DriverContext, DriverEventSink, DriverPrompt, DriverTurnResult, ImageInputMode,
    PermissionConfigCapabilities, PermissionDecision, PermissionOutcome, ProviderCommandDescriptor,
    ProviderDescriptor, ProviderDriver, ProviderSessionCommands,
};

/// What Claude Code supports and how TodeX adapts to it.
pub(super) const PROFILE: ProviderProfile = ProviderProfile {
    kind: ProviderKind::ClaudeCode,
    display_name: "Claude Code",
    permission_config: PermissionConfigCapabilities {
        modes: &["ask", "accept-edits", "auto", "full-access"],
        default_mode: "auto",
        supports_plan: true,
        sandbox_modes: &["read-only", "workspace-write", "danger-full-access"],
        approval_policies: &["on-request", "never"],
        permission_profiles: &["read-only", "workspace-write", "danger-full-access"],
        enforcement: "agent-policy",
        description: "Claude default / acceptEdits / auto / bypassPermissions and independent plan mode; not an operating-system sandbox. Legacy combinations remain validated.",
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
    mcp_injection: McpInjection::ClaudeArgs,
    skill_injection: SkillInjection::PromptText,
    file_attachments: FileAttachmentStyle::AtMention,
    profile_required: false,
    recovery_full_scan: false,
    // One resident process per conversation; see `claude/runtime.rs`.
    process_model: ProcessModel::Resident {
        idle: Some(Duration::from_secs(300)),
        max_sessions: 16,
    },
    discovery_cache_ttl: Some(Duration::from_secs(60)),
    catalog: CatalogProfile {
        source: CatalogSource::Filesystem,
        config_home: ConfigHome {
            env: None,
            home_relative: ".claude",
        },
        project_skills: ".claude/skills",
        mcp_user_files: &[
            UserConfigFile::Home(".claude.json"),
            UserConfigFile::Home(".claude/settings.json"),
        ],
        mcp_project_files: &[".mcp.json", ".claude/settings.json"],
        mcp_user_source: "claude-user",
        mcp_project_source: "claude-project",
    },
};

mod permission_mode;
mod runtime;
mod session_rules;
mod session_work;

/// Slash-command catalogs captured from each process's `initialize` control
/// response, keyed by conversation id. Kept apart from the resident process
/// so the catalog survives its idle exit.
type Catalogs = Arc<Mutex<HashMap<String, ProviderSessionCommands>>>;

pub struct ClaudeDriver {
    binary: String,
    catalogs: Catalogs,
    sessions: runtime::Sessions,
    models: CatalogCache<Vec<super::types::ProviderModelDescriptor>>,
}

/// Claude Code accepts both family aliases (`opus`) and concrete model ids
/// (`claude-opus-4-6`) for `--model`. This catalog mirrors the list baked into
/// the CLI so the picker can offer "latest" plus pinned versions per family.
/// Models with an empty `efforts` slice predate the CLI's effort option and
/// disable the slider in clients.
///
/// `ultracode` is listed like an effort rung the way Claude's own `/effort`
/// menu presents it; it activates xhigh plus dynamic-workflow orchestration and
/// only engages when workflows are enabled on an xhigh-capable model. Lower
/// rungs fall back to the model's ceiling, so gating it here (instead of the
/// CLI) keeps the picker honest.
const CLAUDE_EFFORTS_MAX: &[&str] = &["low", "medium", "high", "max"];
const CLAUDE_EFFORTS_ULTRACODE: &[&str] = &["low", "medium", "high", "xhigh", "max", "ultracode"];

fn claude_model(
    id: &str,
    display_name: &str,
    description: &str,
    family: Option<&str>,
    efforts: &[&str],
    default_effort: Option<&str>,
    context_window: Option<u64>,
) -> super::types::ProviderModelDescriptor {
    super::types::ProviderModelDescriptor {
        id: id.to_owned(),
        display_name: display_name.to_owned(),
        description: description.to_owned(),
        is_default: false,
        supported_reasoning_efforts: efforts.iter().map(|effort| effort.to_string()).collect(),
        default_reasoning_effort: default_effort.map(str::to_owned),
        context_window,
        image_input: Some(true),
        family: family.map(str::to_owned),
    }
}

// (id, display name, family, supported efforts, default effort, context window).
// Newest first within each family.
type ClaudeModelSpec = (
    &'static str,
    &'static str,
    &'static str,
    &'static [&'static str],
    Option<&'static str>,
    Option<u64>,
);
const CLAUDE_MODELS: &[ClaudeModelSpec] = &[
    (
        "claude-opus-5-5",
        "Opus 5.5",
        "opus",
        CLAUDE_EFFORTS_ULTRACODE,
        Some("medium"),
        Some(1_000_000),
    ),
    (
        "claude-opus-5",
        "Opus 5",
        "opus",
        CLAUDE_EFFORTS_ULTRACODE,
        Some("high"),
        Some(1_000_000),
    ),
    (
        "claude-opus-4-8",
        "Opus 4.8",
        "opus",
        CLAUDE_EFFORTS_ULTRACODE,
        Some("high"),
        Some(1_000_000),
    ),
    (
        "claude-opus-4-7",
        "Opus 4.7",
        "opus",
        CLAUDE_EFFORTS_ULTRACODE,
        Some("xhigh"),
        Some(1_000_000),
    ),
    (
        "claude-opus-4-6",
        "Opus 4.6",
        "opus",
        CLAUDE_EFFORTS_MAX,
        None,
        Some(200_000),
    ),
    (
        "claude-opus-4-5-20251101",
        "Opus 4.5",
        "opus",
        &[],
        None,
        Some(200_000),
    ),
    (
        "claude-opus-4-1-20250805",
        "Opus 4.1",
        "opus",
        &[],
        None,
        Some(200_000),
    ),
    (
        "claude-opus-4-20250514",
        "Opus 4",
        "opus",
        &[],
        None,
        Some(200_000),
    ),
    (
        "claude-sonnet-5-5",
        "Sonnet 5.5",
        "sonnet",
        CLAUDE_EFFORTS_ULTRACODE,
        Some("medium"),
        Some(1_000_000),
    ),
    (
        "claude-sonnet-5",
        "Sonnet 5",
        "sonnet",
        CLAUDE_EFFORTS_ULTRACODE,
        Some("high"),
        Some(1_000_000),
    ),
    (
        "claude-sonnet-4-6",
        "Sonnet 4.6",
        "sonnet",
        CLAUDE_EFFORTS_MAX,
        None,
        Some(200_000),
    ),
    (
        "claude-sonnet-4-5-20250929",
        "Sonnet 4.5",
        "sonnet",
        &[],
        None,
        Some(200_000),
    ),
    (
        "claude-sonnet-4-20250514",
        "Sonnet 4",
        "sonnet",
        &[],
        None,
        Some(200_000),
    ),
    (
        "claude-3-7-sonnet-20250219",
        "Sonnet 3.7",
        "sonnet",
        &[],
        None,
        None,
    ),
    (
        "claude-3-5-sonnet-20241022",
        "Sonnet 3.5",
        "sonnet",
        &[],
        None,
        None,
    ),
    (
        "claude-fable-5-1",
        "Fable 5.1",
        "fable",
        CLAUDE_EFFORTS_ULTRACODE,
        Some("high"),
        Some(1_000_000),
    ),
    (
        "claude-fable-5",
        "Fable 5",
        "fable",
        CLAUDE_EFFORTS_ULTRACODE,
        Some("high"),
        Some(1_000_000),
    ),
    (
        "claude-haiku-4-5-20251001",
        "Haiku 4.5",
        "haiku",
        &[],
        None,
        Some(200_000),
    ),
    (
        "claude-3-5-haiku-20241022",
        "Haiku 3.5",
        "haiku",
        &[],
        None,
        None,
    ),
];

// Family aliases resolve to the newest entry; clients render them as the
// family's "latest" option. (alias id, resolved display name)
const CLAUDE_FAMILY_ALIASES: &[(&str, &str)] = &[
    ("opus", "Opus 5.5"),
    ("sonnet", "Sonnet 5.5"),
    ("fable", "Fable 5.1"),
    ("haiku", "Haiku 4.5"),
];

fn claude_model_aliases() -> Vec<super::types::ProviderModelDescriptor> {
    let mut models = vec![claude_model(
        "default",
        "default",
        "Opus 5",
        None,
        CLAUDE_EFFORTS_ULTRACODE,
        None,
        None,
    )];
    models[0].is_default = true;
    for (alias, resolves_to) in CLAUDE_FAMILY_ALIASES {
        // Haiku tops out below xhigh; its alias keeps effort selection but not
        // the tiers the family cannot run.
        let alias_efforts = if *alias == "haiku" {
            CLAUDE_EFFORTS_MAX
        } else {
            CLAUDE_EFFORTS_ULTRACODE
        };
        models.push(claude_model(
            alias,
            alias,
            resolves_to,
            Some(alias),
            alias_efforts,
            None,
            None,
        ));
        models.extend(CLAUDE_MODELS.iter().filter(|entry| entry.2 == *alias).map(
            |(id, name, family, efforts, default_effort, context_window)| {
                claude_model(
                    id,
                    name,
                    "",
                    Some(*family),
                    efforts,
                    *default_effort,
                    *context_window,
                )
            },
        ));
    }
    models
}

/// Effort rungs for a gateway-discovered id: pinned entries reuse their
/// spec, and unknown ids in xhigh-capable families postdate the pinned list.
fn claude_discovered_efforts(id: &str) -> &'static [&'static str] {
    if let Some(spec) = CLAUDE_MODELS.iter().find(|entry| entry.0 == id) {
        return spec.3;
    }
    match claude_model_family(id).as_deref() {
        Some("opus") | Some("sonnet") | Some("fable") => CLAUDE_EFFORTS_ULTRACODE,
        _ => CLAUDE_EFFORTS_MAX,
    }
}

/// Maps a Claude model id to its picker family so discovered catalogs (for
/// example a gateway's `/v1/models`) group the same way the built-in list does.
fn claude_model_family(id: &str) -> Option<String> {
    let lower = id.to_ascii_lowercase();
    ["sonnet", "opus", "haiku", "fable", "mythos"]
        .into_iter()
        .find(|family| lower.contains(family))
        .map(str::to_owned)
}

/// Upper bound for the gateway `/v1/models` request; discovery falls back to
/// the built-in aliases rather than holding up the model picker.
const CLAUDE_DISCOVERY_TIMEOUT: Duration = Duration::from_secs(10);

/// The managed provider (live settings.json env) wins over the daemon's own
/// process environment so the catalog follows the active account.
fn claude_discovery_env(key: &str) -> Option<String> {
    crate::agent_providers::claude_live_env(key).or_else(|| {
        std::env::var(key)
            .ok()
            .filter(|value| !value.trim().is_empty())
    })
}

/// `{base}/v1/models`, or `None` when discovery must not send credentials
/// there: anything but https is only allowed for a loopback gateway.
fn claude_discovery_url(base: &str) -> Option<reqwest::Url> {
    let url = match reqwest::Url::parse(&format!("{}/v1/models", base.trim().trim_end_matches('/')))
    {
        Ok(url) => url,
        Err(error) => {
            tracing::warn!(%error, "ignoring invalid ANTHROPIC_BASE_URL for model discovery");
            return None;
        }
    };
    let loopback = url
        .host_str()
        .is_some_and(crate::listen_addrs::is_loopback_host);
    match url.scheme() {
        "https" => Some(url),
        "http" if loopback => Some(url),
        scheme => {
            // Host and scheme only: the URL itself may carry userinfo.
            tracing::warn!(
                scheme,
                host = url.host_str().unwrap_or_default(),
                "skipping Claude model discovery: a non-loopback ANTHROPIC_BASE_URL must use https"
            );
            None
        }
    }
}

impl ClaudeDriver {
    pub fn new(config: &AgentConfig) -> Self {
        Self {
            binary: config.claude_bin.clone(),
            catalogs: Catalogs::default(),
            sessions: runtime::Sessions::new(
                config.provider_keepalive(),
                PROFILE
                    .process_model
                    .max_sessions()
                    .expect("Claude declares a session limit"),
            ),
            models: CatalogCache::new(PROFILE.discovery_cache_ttl),
        }
    }
}

#[async_trait]
impl ProviderDriver for ClaudeDriver {
    fn descriptor(&self) -> ProviderDescriptor {
        let available = executable_available(&self.binary);
        ProviderDescriptor {
            id: ProviderKind::ClaudeCode,
            display_name: PROFILE.display_name,
            available,
            unavailable_reason: (!available)
                .then(|| format!("executable '{}' was not found", self.binary)),
            profiles: Vec::new(),
            capabilities: PROFILE.capabilities(),
            models: claude_model_aliases(),
        }
    }

    async fn discover_models(
        &self,
        workspace: &Path,
    ) -> Result<Vec<super::types::ProviderModelDescriptor>, AppError> {
        let fetch = async {
            let Some(base) = claude_discovery_env("ANTHROPIC_BASE_URL") else {
                return Ok(claude_model_aliases());
            };
            let Some(url) = claude_discovery_url(&base) else {
                return Ok(claude_model_aliases());
            };
            let request = crate::agent_providers::http_client()?
                .get(url)
                .timeout(CLAUDE_DISCOVERY_TIMEOUT);
            // Same headers Claude Code itself sends: ANTHROPIC_AUTH_TOKEN as a
            // bearer token, taking precedence over ANTHROPIC_API_KEY (x-api-key).
            let request = if let Some(token) = claude_discovery_env("ANTHROPIC_AUTH_TOKEN") {
                request.bearer_auth(token)
            } else if let Some(key) = claude_discovery_env("ANTHROPIC_API_KEY") {
                request.header("x-api-key", key)
            } else {
                request
            };
            let response = request
                .header("anthropic-version", "2023-06-01")
                .send()
                .await
                .map_err(|error| {
                    AppError::ProviderUnavailable(format!("Claude model discovery failed: {error}"))
                })?;
            let payload: Value = response.json().await.map_err(|error| {
                AppError::ProviderUnavailable(format!("Claude model catalog invalid: {error}"))
            })?;
            let models = payload
                .get("data")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
                .filter_map(|item| {
                    let id = item.get("id").and_then(Value::as_str)?.to_owned();
                    let family = claude_model_family(&id);
                    let efforts = claude_discovered_efforts(&id);
                    Some(super::types::ProviderModelDescriptor {
                        display_name: item
                            .get("display_name")
                            .or_else(|| item.get("displayName"))
                            .and_then(Value::as_str)
                            .unwrap_or(&id)
                            .to_owned(),
                        id,
                        description: "Claude gateway model".to_owned(),
                        is_default: false,
                        supported_reasoning_efforts: efforts
                            .iter()
                            .map(|effort| effort.to_string())
                            .collect(),
                        default_reasoning_effort: None,
                        context_window: item
                            .get("context_window")
                            .or_else(|| item.get("contextWindow"))
                            .and_then(Value::as_u64),
                        image_input: Some(true),
                        family,
                    })
                })
                .collect::<Vec<_>>();
            Ok(if models.is_empty() {
                claude_model_aliases()
            } else {
                models
            })
        };
        self.models.fetch(&self.binary, workspace, fetch).await
    }

    // Claude startup can wait on MCP servers before answering initialize, so
    // the default 8s catalog budget is too tight for a cold probe.
    fn discovery_timeout(&self) -> Duration {
        Duration::from_secs(20)
    }

    async fn session_commands(
        &self,
        conversation_id: &str,
    ) -> Result<Option<ProviderSessionCommands>, AppError> {
        Ok(self.catalogs.lock().await.get(conversation_id).cloned())
    }

    async fn discover_commands(
        &self,
        workspace: &Path,
    ) -> Result<Vec<ProviderCommandDescriptor>, AppError> {
        // The initialize control response carries the slash-command catalog,
        // so a session-less probe gets builtins, workspace commands, plugin
        // commands and skills without ever reaching a model call.
        let mut spec = CommandSpec::new(&self.binary, workspace);
        spec.args = vec![
            "-p".to_owned(),
            "--input-format".to_owned(),
            "stream-json".to_owned(),
            "--output-format".to_owned(),
            "stream-json".to_owned(),
            "--verbose".to_owned(),
            "--no-session-persistence".to_owned(),
            "--permission-prompts".to_owned(),
            "host".to_owned(),
            "--permission-prompt-tool".to_owned(),
            "stdio".to_owned(),
            "--permission-mode".to_owned(),
            "default".to_owned(),
        ];
        let mut process = JsonLineProcess::spawn(&spec).await?;
        // The sender must outlive initialize: `cancel.changed()` resolves as
        // soon as every sender drops and would masquerade as a cancellation.
        let (_cancel, mut receiver) = watch::channel(false);
        let commands = initialize_claude(&mut process, &mut receiver, None).await;
        process.terminate().await;
        commands.map(|(commands, _)| commands)
    }

    fn supports_native_fork(&self) -> bool {
        true
    }

    async fn fork_session(
        &self,
        context: DriverContext,
    ) -> Result<crate::conversation::ProviderState, AppError> {
        let source = context
            .provider_state
            .native_session_id
            .clone()
            .unwrap_or_else(|| context.manifest.id.clone());
        let config_dir = crate::agent_providers::claude_config_dir();
        let source_path =
            claude_session_path_at(&config_dir, &context.manifest.workspace, &source).await;
        if !tokio::fs::try_exists(&source_path).await.unwrap_or(false) {
            return Err(AppError::Unsupported(
                "conversation has no Claude transcript to fork".to_owned(),
            ));
        }
        let forked = uuid::Uuid::new_v4().to_string();
        fork_claude_transcript(
            &source_path,
            &config_dir,
            &context.manifest.workspace,
            &forked,
        )
        .await?;
        let mut state = crate::conversation::ProviderState::new(ProviderKind::ClaudeCode);
        state.native_session_id = Some(forked);
        state.recoverable = true;
        Ok(state)
    }

    async fn prewarm(
        &self,
        context: DriverContext,
        settings: DriverPrompt,
        _sink: DriverEventSink,
    ) -> Result<super::types::ProviderPrewarm, AppError> {
        self.sessions
            .prewarm(
                runtime::Launcher {
                    binary: self.binary.clone(),
                    catalogs: self.catalogs.clone(),
                },
                runtime::Warm { context, settings },
            )
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
            .run(
                runtime::Launcher {
                    binary: self.binary.clone(),
                    catalogs: self.catalogs.clone(),
                },
                runtime::Run {
                    context,
                    prompt,
                    sink,
                    cancel,
                },
            )
            .await
    }

    async fn shutdown_session(&self, conversation_id: &str) {
        self.sessions.stop(conversation_id).await;
    }

    async fn shutdown(&self) {
        self.sessions.stop_all().await;
    }
}

/// The native session a turn runs on: the recorded one, else the
/// conversation id (`--session-id` creates it under that id).
fn claude_session_id(context: &DriverContext) -> String {
    context
        .provider_state
        .native_session_id
        .clone()
        .unwrap_or_else(|| context.manifest.id.clone())
}

/// The command line of a Claude process serving `context` with `prompt`'s
/// launch settings. Called once per spawn: it moves an unloadable
/// transcript aside and writes the agent MCP config.
async fn claude_launch_spec(
    binary: &str,
    context: &DriverContext,
    prompt: &DriverPrompt,
) -> Result<(CommandSpec, String), AppError> {
    let controls = super::types::resolve_execution_config(
        context.manifest.provider,
        prompt.permission_mode.as_deref(),
        prompt.work_mode.as_deref(),
        prompt.permission_profile.as_deref(),
        prompt.sandbox_mode.as_deref(),
        prompt.approval_policy.as_deref(),
    )?;
    let requested_session_id = claude_session_id(context);
    let mut spec = CommandSpec::new(binary, &context.manifest.workspace);
    spec.args = vec![
        "-p".to_owned(),
        "--input-format".to_owned(),
        "stream-json".to_owned(),
        "--output-format".to_owned(),
        "stream-json".to_owned(),
        "--verbose".to_owned(),
        "--include-partial-messages".to_owned(),
        "--replay-user-messages".to_owned(),
        "--permission-prompts".to_owned(),
        "host".to_owned(),
        // Match the official Agent SDK transport: host alone does not
        // connect can_use_tool requests to the stream-json control channel.
        "--permission-prompt-tool".to_owned(),
        "stdio".to_owned(),
        "--permission-mode".to_owned(),
        controls
            .provider_mode
            .expect("Claude permission mode validated"),
    ];
    // `session_state_changed: idle` marks the real end of a turn; see
    // `run_claude_turn`.
    spec.env.insert(
        "CLAUDE_CODE_EMIT_SESSION_STATE_EVENTS".to_owned(),
        "1".to_owned(),
    );
    // `--resume` replays a transcript Claude already wrote; it fails
    // initialization outright ("No conversation found") when the file is
    // missing or unloadable — transcript cleanup, manual deletion, or a
    // `CLAUDE_CONFIG_DIR` change all orphan a recorded `native_session_id`
    // this way, and retrying the same `--resume` can never recover. Only a
    // transcript that exists and parses justifies `--resume`.
    let session_path = claude_session_path_at(
        &crate::agent_providers::claude_config_dir(),
        &context.manifest.workspace,
        &requested_session_id,
    )
    .await;
    if claude_transcript_loadable(&session_path).await {
        // A turn that dies after Claude creates its transcript but before
        // the id reaches provider state (rate limit, crash, cancel) leaves
        // no recorded session: `--session-id` would then deadlock on
        // "Session ID is already in use", so resume the file instead.
        spec.args.push("--resume".to_owned());
    } else {
        // A transcript file that fails to load still blocks `--session-id`
        // ("already in use"), so move it aside rather than deleting it and
        // start a fresh transcript under the same id.
        if tokio::fs::try_exists(&session_path).await.unwrap_or(false) {
            let stale = session_path.with_extension("jsonl.todex-stale");
            if let Err(error) = tokio::fs::rename(&session_path, &stale).await {
                tracing::warn!(
                    path = %session_path.display(),
                    %error,
                    "could not move aside unloadable Claude transcript"
                );
            }
        }
        spec.args.push("--session-id".to_owned());
    }
    spec.args.push(requested_session_id.clone());
    if let Some(model) = &prompt.model {
        spec.push_flag_value("--model", model)?;
    }
    if let Some(effort) = &prompt.reasoning_effort {
        spec.push_flag_value("--effort", effort)?;
    }
    if let Some(launch) = &context.agent_mcp {
        spec.args
            .extend(super::mcp_injection::claude_args(launch).await?);
    }
    // Files uploaded with a prompt sit outside the workspace; without this
    // Claude asks before every read of them.
    if let Some(uploads) = crate::prompt_uploads::readable_root().await {
        spec.push_flag_value("--add-dir", &uploads.to_string_lossy())?;
    }
    Ok((spec, requested_session_id))
}

/// Spawns Claude for `context` with `prompt`'s launch settings. Returns the
/// process and its session id; [`initialize_session`] completes the start.
async fn spawn_claude(
    binary: &str,
    context: &DriverContext,
    prompt: &DriverPrompt,
) -> Result<(JsonLineProcess, String), AppError> {
    let (spec, session_id) = claude_launch_spec(binary, context, prompt).await?;
    let process = JsonLineProcess::spawn(&spec).await?;
    Ok((process, session_id))
}

/// Completes the `initialize` handshake of a process from [`spawn_claude`],
/// recording the session's slash-command catalog, and returns what the
/// process reported about its permission mode (`expected_mode` is the one it
/// was launched with). On failure the caller stops the process, so whoever
/// owns the conversation knows when it is gone.
async fn initialize_session(
    process: &mut JsonLineProcess,
    catalogs: &Catalogs,
    context: &DriverContext,
    runtime_id: String,
    expected_mode: Option<&str>,
    cancel: &mut watch::Receiver<bool>,
) -> Result<permission_mode::ModeReport, AppError> {
    let (commands, report) = initialize_claude(process, cancel, expected_mode).await?;
    catalogs.lock().await.insert(
        context.manifest.id.clone(),
        ProviderSessionCommands {
            commands,
            runtime_id,
        },
    );
    Ok(report)
}

/// Whether the transcript at `path` can back a `--resume`: the file must
/// exist and open with a JSON object on its first line. Claude stores
/// sessions at `<config>/projects/<dir>/<id>.jsonl` and answers `--resume`
/// for a missing or truncated file with a hard "No conversation found"
/// initialization error, so existence alone is not enough.
async fn claude_transcript_loadable(path: &Path) -> bool {
    let Ok(file) = tokio::fs::File::open(path).await else {
        return false;
    };
    let mut first_line = String::new();
    let Ok(read) = tokio::io::BufReader::new(file)
        .read_line(&mut first_line)
        .await
    else {
        return false;
    };
    read > 0 && serde_json::from_str::<Value>(first_line.trim()).is_ok_and(|line| line.is_object())
}

async fn claude_session_path_at(config_dir: &Path, workspace: &Path, session_id: &str) -> PathBuf {
    let workspace = tokio::fs::canonicalize(workspace)
        .await
        .unwrap_or_else(|_| workspace.to_path_buf());
    config_dir
        .join("projects")
        .join(claude_project_dir_name(&workspace))
        .join(format!("{session_id}.jsonl"))
}

/// Fork a Claude session by copying its transcript under a fresh session id:
/// every `sessionId` field is rewritten so `--resume <id>` loads the copy as
/// its own session. `claude --resume --fork-session` achieves the same result
/// but only by spending a turn on a throwaway prompt; the transcript rewrite
/// keeps the fork free of side effects.
async fn fork_claude_transcript(
    source_path: &Path,
    config_dir: &Path,
    workspace: &Path,
    forked_id: &str,
) -> Result<(), AppError> {
    let raw = tokio::fs::read(source_path).await?;
    let text = String::from_utf8(raw)
        .map_err(|_| AppError::InvalidRequest("Claude transcript is not UTF-8".to_owned()))?;
    let mut lines = Vec::with_capacity(text.lines().count());
    for line in text.lines() {
        match serde_json::from_str::<Value>(line) {
            Ok(mut value) => {
                if let Some(object) = value.as_object_mut() {
                    if object.get("sessionId").and_then(Value::as_str).is_some() {
                        object.insert("sessionId".to_owned(), json!(forked_id));
                    }
                }
                lines.push(serde_json::to_string(&value)?);
            }
            // Keep an unparseable line verbatim rather than corrupting the copy.
            Err(_) => lines.push(line.to_owned()),
        }
    }
    let destination = claude_session_path_at(config_dir, workspace, forked_id).await;
    if tokio::fs::try_exists(&destination).await.unwrap_or(false) {
        return Err(AppError::Conflict(
            "Claude fork destination already exists".to_owned(),
        ));
    }
    // Created new and owner-only from the start: the transcript holds the
    // conversation, and an existing file is never replaced.
    let bytes = format!("{}\n", lines.join("\n")).into_bytes();
    let written = destination.clone();
    match tokio::task::spawn_blocking(move || crate::secure_fs::create_owner_only(&written, &bytes))
        .await
        .map_err(|error| AppError::Anyhow(error.into()))?
    {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => Err(AppError::Conflict(
            "Claude fork destination already exists".to_owned(),
        )),
        Err(error) => Err(error.into()),
    }
}

/// The per-project directory name Claude Code derives from its cwd:
/// `cwd.replace(/[^a-zA-Z0-9]/g, "-")` over UTF-16 code units, and when that
/// exceeds 200 characters, `sanitized[..200] + "-" + abs(javaHash(cwd))` in
/// base36 (`NY`/`Le`/`gT` in the bundled CLI).
fn claude_project_dir_name(workspace: &Path) -> String {
    const MAX_PROJECT_DIR_CHARS: usize = 200;
    let raw = workspace.as_os_str().to_string_lossy();
    let sanitized: String = raw
        .encode_utf16()
        .map(|unit| {
            match u8::try_from(unit)
                .ok()
                .filter(|u| u.is_ascii_alphanumeric())
            {
                Some(u) => u as char,
                None => '-',
            }
        })
        .collect();
    if sanitized.len() <= MAX_PROJECT_DIR_CHARS {
        return sanitized;
    }
    let hash = raw.encode_utf16().fold(0_i32, |acc, unit| {
        acc.wrapping_mul(31).wrapping_add(unit as i32)
    });
    format!(
        "{}-{}",
        &sanitized[..MAX_PROJECT_DIR_CHARS],
        base36(hash.unsigned_abs() as u64)
    )
}

fn base36(mut value: u64) -> String {
    const DIGITS: &[u8; 36] = b"0123456789abcdefghijklmnopqrstuvwxyz";
    if value == 0 {
        return "0".to_owned();
    }
    let mut out = Vec::new();
    while value > 0 {
        out.push(DIGITS[(value % 36) as usize]);
        value /= 36;
    }
    out.iter().rev().map(|&b| b as char).collect()
}

fn claude_user_content(prompt: &DriverPrompt) -> Value {
    let images = prompt
        .content
        .iter()
        .filter_map(|content| match content {
            super::types::DriverPromptContent::Image {
                data, mime_type, ..
            } => Some(json!({
                "type": "image",
                "source": {
                    "type": "base64",
                    "media_type": mime_type,
                    "data": data,
                },
            })),
            super::types::DriverPromptContent::File { .. } => None,
        })
        .collect::<Vec<_>>();
    if images.is_empty() {
        return Value::String(prompt.text.clone());
    }

    let mut content = Vec::with_capacity(images.len() + usize::from(!prompt.text.is_empty()));
    if !prompt.text.is_empty() {
        content.push(json!({ "type": "text", "text": prompt.text }));
    }
    content.extend(images);
    Value::Array(content)
}

#[cfg(test)]
mod tests {
    use serde_json::json;
    use std::path::Path;

    #[test]
    fn only_auto_reported_as_default_falls_back_to_ask() {
        use super::auto_mode_fell_back;
        assert!(auto_mode_fell_back(Some("auto"), "default"));
        for (expected, actual) in [
            (Some("auto"), "plan"),
            (Some("bypassPermissions"), "default"),
            (Some("default"), "auto"),
            (None, "default"),
            (Some("auto"), "auto"),
        ] {
            assert!(
                !auto_mode_fell_back(expected, actual),
                "{expected:?} {actual}"
            );
        }
        assert!(auto_mode_fell_back(Some("auto"), "acceptEdits"));
        let event = super::auto_mode_unavailable_event(None, "ask", None);
        assert_eq!(event["providerMethod"], "auto_mode_unavailable");
        assert!(event["metadata"]["model"].is_null());
        assert_eq!(event["metadata"]["cause"], "unavailable");
        assert!(event["metadata"]["settingsSource"].is_null());
        let event =
            super::auto_mode_unavailable_event(Some("m"), "accept-edits", Some("userSettings"));
        assert_eq!(event["metadata"]["cause"], "settings");
        assert_eq!(event["metadata"]["settingsSource"], "userSettings");
        assert_eq!(event["metadata"]["activeMode"], "accept-edits");
    }

    use super::{
        claude_command_catalog, claude_discovery_url, claude_frame_limit_reset,
        claude_model_aliases, claude_model_family, claude_permission_details,
        claude_question_details, claude_question_response, claude_quota_event, claude_result_error,
        claude_transcript_loadable, claude_user_content, handle_stream_event, usage_limit_reset,
        BackgroundTasks, ClaudeSubagents, ClaudeToolCalls,
    };
    use crate::conversation::{
        ConversationEventHub, ConversationManifest, ConversationStore, ProviderKind,
    };
    use crate::provider::types::{DriverEventSink, PermissionBroker};
    use crate::provider::types::{
        DriverPrompt, DriverPromptContent, PermissionDecision, PermissionOutcome,
    };

    #[test]
    fn allow_for_session_is_offered_and_answered_with_a_session_rule() {
        use super::{claude_tool_options, claude_tool_response, session_rules::session_rule};
        let ids = |options: serde_json::Value| {
            options
                .as_array()
                .unwrap()
                .iter()
                .map(|option| option["id"].as_str().unwrap().to_owned())
                .collect::<Vec<_>>()
        };
        assert_eq!(
            ids(claude_tool_options(true)),
            ["allow_once", "allow_always", "reject_once"]
        );
        assert_eq!(
            ids(claude_tool_options(false)),
            ["allow_once", "reject_once"]
        );

        let request = json!({
            "subtype": "can_use_tool",
            "tool_name": "Bash",
            "input": { "command": "cargo test" },
            // Never echoed back: it would write .claude/settings.local.json.
            "permission_suggestions": [{ "type": "addRules", "destination": "localSettings" }],
        });
        let decide = |outcome| PermissionDecision {
            outcome,
            option_id: None,
            data: None,
        };
        let rule = session_rule(&request, Path::new("/work"));
        assert_eq!(
            claude_tool_response(
                &request,
                rule.clone(),
                &decide(PermissionOutcome::AllowAlways)
            ),
            json!({
                "behavior": "allow",
                "updatedInput": { "command": "cargo test" },
                "updatedPermissions": [{
                    "type": "addRules",
                    "rules": [{ "toolName": "Bash", "ruleContent": "cargo test" }],
                    "behavior": "allow",
                    "destination": "session",
                }],
            })
        );
        assert_eq!(
            claude_tool_response(&request, rule, &decide(PermissionOutcome::AllowOnce)),
            json!({ "behavior": "allow", "updatedInput": { "command": "cargo test" } })
        );
        assert_eq!(
            claude_tool_response(&request, None, &decide(PermissionOutcome::RejectOnce))
                ["behavior"],
            "deny"
        );
    }

    #[test]
    fn model_discovery_requires_https_except_on_loopback() {
        assert_eq!(
            claude_discovery_url("https://gw.example.com/")
                .unwrap()
                .as_str(),
            "https://gw.example.com/v1/models"
        );
        for base in [
            "http://127.0.0.1:8080",
            "http://localhost:4000",
            "http://[::1]:9",
        ] {
            assert!(claude_discovery_url(base).is_some(), "{base}");
        }
        for base in [
            "http://gw.example.com",
            "http://192.168.1.20:8080",
            "ftp://gw.example.com",
            "not a url",
        ] {
            assert!(claude_discovery_url(base).is_none(), "{base}");
        }
    }

    #[tokio::test]
    async fn stream_text_and_tool_json_merge_per_content_block() {
        let root = std::env::temp_dir().join(format!(
            "todex-claude-deltas-{}",
            uuid::Uuid::new_v4().simple()
        ));
        let store = ConversationStore::new(root.join("data")).await.unwrap();
        let manifest = store
            .create(ConversationManifest::new(
                ProviderKind::ClaudeCode,
                root.clone(),
                None,
                None,
            ))
            .await
            .unwrap();
        let sink = DriverEventSink::new(
            store.clone(),
            ConversationEventHub::default(),
            PermissionBroker::default(),
            &manifest.id,
        )
        .with_turn_id("turn-1");
        let mut tools = ClaudeToolCalls::default();
        let delta = |index: u64, delta: serde_json::Value| {
            json!({"type":"stream_event","event":{
                "type":"content_block_delta","index":index,"delta":delta
            }})
        };
        for message in [
            delta(0, json!({"type":"thinking_delta","thinking":"let me "})),
            delta(0, json!({"type":"thinking_delta","thinking":"see"})),
            delta(1, json!({"type":"text_delta","text":"Hel"})),
            delta(1, json!({"type":"text_delta","text":"lo"})),
            delta(2, json!({"type":"text_delta","text":" there"})),
            delta(
                3,
                json!({"type":"input_json_delta","partial_json":"{\"a\""}),
            ),
            delta(3, json!({"type":"input_json_delta","partial_json":":1"})),
            // Another tool block interleaves; neither block absorbs the other.
            delta(
                4,
                json!({"type":"input_json_delta","partial_json":"{\"b\""}),
            ),
            delta(3, json!({"type":"input_json_delta","partial_json":"}"})),
            delta(4, json!({"type":"input_json_delta","partial_json":":2}"})),
            delta(4, json!({"type":"signature_delta","signature":"s1"})),
            delta(4, json!({"type":"signature_delta","signature":"s2"})),
        ] {
            handle_stream_event(&message, "turn-1", &mut tools, &sink)
                .await
                .unwrap();
        }
        sink.emit("message.completed", json!({"provider":"claude-code"}))
            .await
            .unwrap();

        let history = store.complete_history(&manifest.id).await.unwrap();
        let journalled: Vec<_> = history
            .iter()
            .map(|event| (event.event_type.as_str(), event.payload["delta"].clone()))
            .collect();
        assert_eq!(
            journalled,
            [
                (
                    "thought.delta",
                    json!({"type":"thinking_delta","thinking":"let me see"})
                ),
                ("message.delta", json!({"type":"text_delta","text":"Hello"})),
                (
                    "message.delta",
                    json!({"type":"text_delta","text":" there"})
                ),
                (
                    "message.delta",
                    json!({"type":"input_json_delta","partial_json":"{\"a\":1"})
                ),
                (
                    "message.delta",
                    json!({"type":"input_json_delta","partial_json":"{\"b\""})
                ),
                (
                    "message.delta",
                    json!({"type":"input_json_delta","partial_json":"}"})
                ),
                (
                    "message.delta",
                    json!({"type":"input_json_delta","partial_json":":2}"})
                ),
                (
                    "message.delta",
                    json!({"type":"signature_delta","signature":"s1"})
                ),
                (
                    "message.delta",
                    json!({"type":"signature_delta","signature":"s2"})
                ),
                ("message.completed", serde_json::Value::Null),
            ]
        );
        let _ = std::fs::remove_dir_all(root);
    }

    fn ask_user_question_input() -> serde_json::Value {
        json!({
            "questions": [
                {
                    "question": "Which file?",
                    "header": "Target",
                    "options": [
                        { "label": "web", "description": "TodeX_web/AGENTS.md" },
                        { "label": "desktop", "description": "TodeX_desktop/AGENTS.md" }
                    ],
                    "multiSelect": false
                },
                {
                    "question": "Which steps?",
                    "header": "Steps",
                    "options": [{ "label": "commit" }, { "label": "push" }],
                    "multiSelect": true
                }
            ]
        })
    }

    #[test]
    fn tool_snapshots_accumulate_name_input_and_result_under_one_block() {
        let mut tools = ClaudeToolCalls::default();
        let id = tools
            .record(
                &json!({ "type": "tool_use", "id": "toolu_1", "name": "Bash", "input": {} }),
                None,
            )
            .unwrap();
        let started = tools.payload(&id, "turn-1", "started");
        assert_eq!(started["toolName"], "Bash");
        assert_eq!(started["block"]["id"], "toolu_1");
        assert_eq!(started["block"]["category"], "tool");
        assert!(started["subagentId"].is_null());

        tools.record(
            &json!({
                "type": "tool_use", "id": "toolu_1", "name": "Bash",
                "input": { "command": "echo ok" }
            }),
            None,
        );
        // A late empty-input block never erases the complete arguments.
        tools.record(
            &json!({ "type": "tool_use", "id": "toolu_1", "input": {} }),
            None,
        );
        assert_eq!(
            tools.payload(&id, "turn-1", "delta")["arguments"]["command"],
            "echo ok"
        );

        tools.complete(
            &json!({
                "type": "tool_result", "tool_use_id": "toolu_1",
                "content": [{ "type": "text", "text": "ok" }], "is_error": false
            }),
            None,
        );
        let completed = tools.payload(&id, "turn-1", "completed");
        assert_eq!(completed["toolName"], "Bash");
        assert_eq!(completed["arguments"]["command"], "echo ok");
        assert_eq!(completed["result"], "ok");
        assert_eq!(completed["isError"], false);
        assert_eq!(completed["block"]["phase"], "completed");
    }

    #[test]
    fn tool_snapshots_tag_subagent_inner_calls_with_their_parent() {
        let mut tools = ClaudeToolCalls::default();
        let inner = tools
            .record(
                &json!({ "type": "tool_use", "id": "toolu_inner", "name": "Read", "input": { "file": "a" } }),
                Some("toolu_task"),
            )
            .unwrap();
        assert_eq!(
            tools.payload(&inner, "turn-1", "started")["subagentId"],
            "toolu_task"
        );
        tools.complete(
            &json!({ "type": "tool_result", "tool_use_id": "toolu_inner", "content": "data" }),
            Some("toolu_task"),
        );
        assert_eq!(
            tools.payload(&inner, "turn-1", "completed")["subagentId"],
            "toolu_task"
        );
        // A call first seen without a parent keeps the attribution a later
        // frame supplies; the main thread's own calls stay untagged.
        tools.name_if_unknown("toolu_late", None, Some("toolu_task"));
        assert_eq!(
            tools.payload("toolu_late", "turn-1", "delta")["subagentId"],
            "toolu_task"
        );
        let main = tools
            .record(
                &json!({ "type": "tool_use", "id": "toolu_main", "name": "Bash" }),
                None,
            )
            .unwrap();
        assert!(tools.payload(&main, "turn-1", "started")["subagentId"].is_null());
    }

    #[test]
    fn ask_user_question_maps_to_the_shared_user_input_shape() {
        let details = claude_question_details(&ask_user_question_input()).unwrap();
        assert_eq!(details["questions"][0]["id"], "q0");
        assert_eq!(details["questions"][0]["question"], "Which file?");
        assert_eq!(details["questions"][0]["options"][1]["label"], "desktop");
        assert_eq!(details["questions"][1]["multiSelect"], true);
        assert_eq!(details["questions"][0]["isOther"], true);
        assert!(claude_question_details(&json!({ "questions": [] })).is_none());
        assert!(claude_question_details(&json!({ "questions": [{ "header": "x" }] })).is_none());
    }

    #[test]
    fn tool_permission_details_surface_command_and_safety_reason() {
        let request = json!({
            "subtype": "can_use_tool",
            "tool_name": "Bash",
            "input": { "command": "rm -f $L/*", "description": "Clean logs" },
            "decision_reason": "Dangerous rm operation on possibly-empty variable path",
            "decision_reason_type": "safetyCheck",
        });
        let details = claude_permission_details(&request);
        assert_eq!(details["command"], "rm -f $L/*");
        assert_eq!(
            details["reason"],
            "Dangerous rm operation on possibly-empty variable path"
        );
        assert_eq!(details["decision_reason_type"], "safetyCheck");

        let edit = claude_permission_details(&json!({
            "tool_name": "Edit",
            "input": { "file_path": "/tmp/a" },
            "decision_reason": "  ",
        }));
        assert!(edit.get("command").is_none());
        assert!(edit.get("reason").is_none());
    }

    #[test]
    fn ask_user_question_answers_are_keyed_by_question_text() {
        let input = ask_user_question_input();
        let answered = claude_question_response(
            &input,
            &PermissionDecision {
                outcome: PermissionOutcome::Answer,
                option_id: Some("answer".to_owned()),
                data: Some(json!({ "answers": {
                    "q0": { "answers": ["web"] },
                    "q1": { "answers": ["commit", "push"] }
                } })),
            },
        );
        assert_eq!(answered["behavior"], "allow");
        assert_eq!(answered["updatedInput"]["questions"], input["questions"]);
        assert_eq!(
            answered["updatedInput"]["answers"],
            json!({ "Which file?": "web", "Which steps?": "commit, push" })
        );

        let skipped = claude_question_response(
            &input,
            &PermissionDecision {
                outcome: PermissionOutcome::RejectOnce,
                option_id: Some("reject_once".to_owned()),
                data: None,
            },
        );
        assert_eq!(skipped["behavior"], "deny");
    }

    #[test]
    fn project_dir_name_matches_claude_transcript_layout() {
        assert_eq!(
            super::claude_project_dir_name(Path::new("/Volumes/BIGDISK/github/Nitrous")),
            "-Volumes-BIGDISK-github-Nitrous"
        );
        // `.`, `_`, `-` are all non-alphanumeric in Claude's mapping.
        assert_eq!(
            super::claude_project_dir_name(Path::new("/Users/a.b_c/d-e")),
            "-Users-a-b-c-d-e"
        );
    }

    #[test]
    fn project_dir_name_truncates_long_paths_with_hash_suffix() {
        let long = format!("/{}", "a".repeat(300));
        let name = super::claude_project_dir_name(Path::new(&long));
        let (prefix, hash) = name.rsplit_once('-').unwrap();
        assert_eq!(prefix.len(), 200);
        // Pinned against Claude Code's `NY`/`Le` (Java string hash, base36).
        assert_eq!(hash, "vtkmfl");
        assert_eq!(
            super::claude_project_dir_name(Path::new(&long)),
            name,
            "hash suffix must be deterministic"
        );
    }

    #[tokio::test]
    async fn session_lookup_resolves_transcript_under_project_dir() {
        let root = std::env::temp_dir().join(format!(
            "todex-claude-session-lookup-{}",
            uuid::Uuid::new_v4().simple()
        ));
        let workspace = root.join("ws");
        let session = "b88ca64f-3ff5-46a3-989b-ff8188fb2d8d";
        let transcript = root
            .join("projects")
            .join(super::claude_project_dir_name(&workspace))
            .join(format!("{session}.jsonl"));
        std::fs::create_dir_all(transcript.parent().unwrap()).unwrap();
        std::fs::write(&transcript, "{}").unwrap();

        assert_eq!(
            super::claude_session_path_at(&root, &workspace, session).await,
            transcript
        );
        assert!(super::claude_transcript_loadable(&transcript).await);
        assert!(
            !super::claude_transcript_loadable(&root.join("projects").join("missing.jsonl")).await
        );
        let _ = std::fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn transcript_fork_rewrites_session_ids_and_keeps_unparseable_lines() {
        let root = std::env::temp_dir().join(format!(
            "todex-claude-fork-{}",
            uuid::Uuid::new_v4().simple()
        ));
        let workspace = root.join("ws");
        let source = "05811362-61b5-4b88-adec-0c75b9603987";
        let source_path = super::claude_session_path_at(&root, &workspace, source).await;
        std::fs::create_dir_all(source_path.parent().unwrap()).unwrap();
        std::fs::write(
            &source_path,
            format!(
                concat!(
                    "{{\"type\":\"user\",\"sessionId\":\"{0}\",\"uuid\":\"u1\",\"parentUuid\":null,\"message\":{{\"role\":\"user\",\"content\":\"hi\"}}}}\n",
                    "{{\"type\":\"assistant\",\"sessionId\":\"{0}\",\"uuid\":\"u2\",\"parentUuid\":\"u1\"}}\n",
                    "{{\"type\":\"summary\",\"summary\":\"half\"}}\n",
                    "not-a-json-line\n"
                ),
                source
            ),
        )
        .unwrap();

        let forked = "3d6e6184-90a2-403c-9b10-8bcdfc46cbbe";
        super::fork_claude_transcript(&source_path, &root, &workspace, forked)
            .await
            .unwrap();

        let forked_path = super::claude_session_path_at(&root, &workspace, forked).await;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&forked_path)
                .unwrap()
                .permissions()
                .mode()
                & 0o777;
            assert_eq!(mode, 0o600, "the forked transcript is owner-only");
        }
        let copied = std::fs::read_to_string(&forked_path).unwrap();
        let lines: Vec<&str> = copied.lines().collect();
        assert_eq!(lines.len(), 4);
        for line in &lines[..2] {
            let value: serde_json::Value = serde_json::from_str(line).unwrap();
            assert_eq!(value["sessionId"], json!(forked));
        }
        // Lines without a sessionId keep their payload; unparseable lines pass through.
        let summary: serde_json::Value = serde_json::from_str(lines[2]).unwrap();
        assert_eq!(summary["summary"], "half");
        assert_eq!(lines[3], "not-a-json-line");
        let user: serde_json::Value = serde_json::from_str(lines[0]).unwrap();
        assert_eq!(user["message"]["content"], "hi");

        // The source transcript is untouched and a second fork to the same id conflicts.
        let original = std::fs::read_to_string(&source_path).unwrap();
        assert!(original.contains(source));
        assert!(matches!(
            super::fork_claude_transcript(&source_path, &root, &workspace, forked).await,
            Err(crate::error::AppError::Conflict(_))
        ));
        assert!(super::fork_claude_transcript(
            &root.join("missing.jsonl"),
            &root,
            &workspace,
            "another-fork"
        )
        .await
        .is_err());
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn built_in_model_aliases_are_selectable_without_gateway_discovery() {
        let models = claude_model_aliases();
        let ids = models
            .iter()
            .map(|model| model.id.as_str())
            .collect::<Vec<_>>();

        assert!(ids.contains(&"default"));
        for family in ["opus", "sonnet", "haiku", "fable"] {
            assert!(ids.contains(&family), "missing {family} alias");
            assert!(
                ids.iter()
                    .any(|id| id.starts_with(&format!("claude-{family}"))),
                "missing {family} versions"
            );
        }
        assert!(models[0].is_default);
        assert_eq!(models[0].id, "default");
        assert_eq!(
            models[0].supported_reasoning_efforts,
            ["low", "medium", "high", "xhigh", "max", "ultracode"]
        );
        // Versioned entries tag their family; aliases sit inside their own
        // family so clients can render "latest" plus pinned versions.
        let opus = models.iter().find(|model| model.id == "opus").unwrap();
        assert_eq!(opus.family.as_deref(), Some("opus"));
        let opus_45 = models
            .iter()
            .find(|model| model.id == "claude-opus-4-5-20251101")
            .unwrap();
        assert_eq!(opus_45.family.as_deref(), Some("opus"));
        assert!(opus_45.supported_reasoning_efforts.is_empty());
        // ultracode needs xhigh: max-capped models stop below it.
        let opus_46 = models
            .iter()
            .find(|model| model.id == "claude-opus-4-6")
            .unwrap();
        assert_eq!(
            opus_46.supported_reasoning_efforts,
            ["low", "medium", "high", "max"]
        );
        let haiku = models.iter().find(|model| model.id == "haiku").unwrap();
        assert!(!haiku
            .supported_reasoning_efforts
            .iter()
            .any(|effort| effort == "ultracode" || effort == "xhigh"));
        let opus_55 = models
            .iter()
            .find(|model| model.id == "claude-opus-5-5")
            .unwrap();
        assert_eq!(opus_55.default_reasoning_effort.as_deref(), Some("medium"));
        assert_eq!(opus_55.context_window, Some(1_000_000));
        assert_eq!(
            claude_model_family("us.anthropic.claude-sonnet-4-5").as_deref(),
            Some("sonnet")
        );
        assert_eq!(
            claude_model_family("claude-3-opus-20240229").as_deref(),
            Some("opus")
        );
        assert_eq!(claude_model_family("gpt-5.5"), None);
    }

    #[test]
    fn background_tasks_track_live_set_until_notifications() {
        let mut tasks = BackgroundTasks::default();
        assert!(tasks.ids().is_empty());

        tasks.apply(&json!({ "subtype": "task_started", "task_id": "a1" }));
        tasks
            .apply(&json!({ "subtype": "task_started", "task_id": "b2", "is_backgrounded": true }));
        assert_eq!(tasks.ids().len(), 2);

        // Foreground task starts are not background work that outlives a turn.
        tasks.apply(
            &json!({ "subtype": "task_started", "task_id": "fg", "is_backgrounded": false }),
        );
        tasks.apply(
            &json!({ "subtype": "task_notification", "task_id": "a1", "status": "completed" }),
        );
        assert_eq!(tasks.ids().len(), 1);

        // background_tasks_changed replaces the whole live set.
        tasks.apply(&json!({
            "subtype": "background_tasks_changed",
            "tasks": [{ "task_id": "c3" }, { "task_id": "d4" }]
        }));
        let mut ids = tasks
            .ids()
            .into_iter()
            .filter_map(|id| id.as_str().map(str::to_owned))
            .collect::<Vec<_>>();
        ids.sort_unstable();
        assert_eq!(ids, ["c3", "d4"]);

        tasks.apply(&json!({ "subtype": "background_tasks_changed", "tasks": [] }));
        assert!(tasks.ids().is_empty());
    }

    #[test]
    fn background_tasks_report_details_for_clients() {
        let mut tasks = BackgroundTasks::default();
        tasks.apply(&json!({
            "subtype": "task_started", "task_id": "sh-1", "task_type": "local_bash",
            "description": "Watch CI run", "tool_use_id": "toolu_1"
        }));
        tasks.apply(&json!({
            "subtype": "task_progress", "task_id": "sh-1", "description": "Still watching CI"
        }));
        // Progress for an unknown task does not resurrect it.
        tasks.apply(&json!({ "subtype": "task_progress", "task_id": "gone" }));

        let listed = tasks.tasks();
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0]["taskId"], "sh-1");
        assert_eq!(listed[0]["description"], "Still watching CI");
        assert_eq!(listed[0]["taskType"], "local_bash");
        assert_eq!(listed[0]["toolUseId"], "toolu_1");
        assert!(listed[0]["startedAt"].is_string());

        // A replacement set keeps known details and takes new tasks' own.
        tasks.apply(&json!({
            "subtype": "background_tasks_changed",
            "tasks": [{ "task_id": "sh-1" }, { "task_id": "ag-2", "description": "Review" }]
        }));
        let listed = tasks.tasks();
        assert_eq!(listed[0]["description"], "Still watching CI");
        assert_eq!(listed[1]["taskId"], "ag-2");
        assert_eq!(listed[1]["description"], "Review");
        assert!(listed[1].get("startedAt").is_none());
        assert_eq!(tasks.ids(), [json!("sh-1"), json!("ag-2")]);
    }

    #[test]
    fn foreground_tasks_moved_to_background_preserve_metadata_and_track_main() {
        let mut tasks = BackgroundTasks::default();
        tasks.apply(&json!({
            "subtype": "task_started",
            "task_id": "sh-fg",
            "task_type": "local_bash",
            "description": "Run long CI",
            "tool_use_id": "toolu_ci",
            "is_backgrounded": false
        }));
        assert!(tasks.ids().is_empty());
        assert!(!tasks.pending());

        // Moved to background via background_tasks_changed
        tasks.apply(&json!({
            "subtype": "background_tasks_changed",
            "tasks": [{ "task_id": "sh-fg", "description": "Still running CI" }]
        }));
        assert_eq!(tasks.ids(), [json!("sh-fg")]);
        assert!(tasks.pending());
        let listed = tasks.tasks();
        assert_eq!(listed[0]["taskId"], "sh-fg");
        assert_eq!(listed[0]["toolUseId"], "toolu_ci");
        assert_eq!(listed[0]["taskType"], "local_bash");
        assert_eq!(listed[0]["description"], "Still running CI");
        assert!(listed[0].get("startedAt").is_some());

        // Completing it owes a follow-up because it was main session
        tasks.apply(&json!({
            "subtype": "task_notification",
            "task_id": "sh-fg",
            "status": "completed"
        }));
        tasks.apply(&json!({ "subtype": "background_tasks_changed", "tasks": [] }));
        assert!(tasks.ids().is_empty());
        assert!(
            tasks.pending(),
            "owes follow-up because main-session foreground was backgrounded"
        );
    }

    #[test]
    fn a_notification_owes_a_result_from_the_next_invocation() {
        let mut tasks = BackgroundTasks::default();
        assert!(!tasks.pending());
        tasks
            .apply(&json!({ "subtype": "task_started", "task_id": "a1", "is_backgrounded": true }));
        assert!(tasks.pending());

        // Finishing mid-invocation empties the set but keeps the turn open
        // through the current `result`, until the follow-up starts.
        tasks.apply(
            &json!({ "subtype": "task_notification", "task_id": "a1", "status": "completed" }),
        );
        tasks.apply(&json!({ "subtype": "background_tasks_changed", "tasks": [] }));
        assert!(tasks.ids().is_empty());
        assert!(tasks.pending());
        tasks.apply(&json!({ "subtype": "init" }));
        assert!(!tasks.pending());

        // A notification the model already received in the same invocation
        // (replayed user message) owes no follow-up; one still queued does.
        for (id, run) in [("b1", "run-b1"), ("c1", "run-c1"), ("d1", "run-d1")] {
            tasks.apply(
                &json!({ "subtype": "task_started", "task_id": id, "run_id": run,
                "is_backgrounded": true }),
            );
            tasks.apply(&json!({ "subtype": "task_notification", "task_id": id, "run_id": run }));
        }
        tasks.delivered(&json!({ "type": "user", "isReplay": true,
            "origin": { "kind": "task-notification", "runId": "run-b1" },
            "message": { "role": "user", "content": "<task-notification>\n<task-id>b1</task-id>\n</task-notification>" } }));
        assert!(tasks.pending());
        // Matched by the task id in the content when the run id is absent.
        tasks.delivered(
            &json!({ "type": "user", "origin": { "kind": "task-notification" },
            "message": { "role": "user", "content": [{ "type": "text",
                "text": "<task-notification>\n<task-id>c1</task-id>\n</task-notification>" }] } }),
        );
        assert!(tasks.pending());
        // Ordinary user messages never settle a notification.
        tasks.delivered(&json!({ "type": "user",
            "message": { "role": "user", "content": "<task-id>d1</task-id>" } }));
        assert!(tasks.pending());
        tasks.delivered(
            &json!({ "type": "user", "origin": { "kind": "task-notification", "runId": "run-d1" },
            "message": { "role": "user", "content": "" } }),
        );
        assert!(!tasks.pending());

        // A subagent's own tasks report back to that subagent; foreground
        // tasks were never tracked.
        tasks.apply(&json!({ "subtype": "task_started", "task_id": "sub",
            "is_backgrounded": true, "owned_by_subagent": true }));
        tasks.apply(&json!({ "subtype": "task_started", "task_id": "fg",
            "is_backgrounded": false, "owned_by_subagent": true }));
        tasks.apply(&json!({ "subtype": "task_notification", "task_id": "sub" }));
        tasks.apply(&json!({ "subtype": "task_notification", "task_id": "fg" }));
        assert!(!tasks.pending());
    }

    #[test]
    fn task_tool_call_opens_a_subagent_and_task_frames_update_it() {
        let mut subagents = ClaudeSubagents::default();

        let started = subagents
            .start_from_tool(
                &json!({
                    "type": "tool_use", "id": "toolu_1", "name": "Task",
                    "input": { "description": "Map contracts", "subagent_type": "Explore",
                        "prompt": "inspect the workspace" }
                }),
                "turn-1",
            )
            .unwrap();
        assert_eq!(started["subagentId"], "toolu_1");
        assert_eq!(started["providerItemId"], "toolu_1");
        assert_eq!(started["agentKind"], "Explore");
        assert_eq!(started["task"], "inspect the workspace");
        assert!(subagents
            .start_from_tool(
                &json!({ "type": "tool_use", "id": "toolu_1", "name": "Task", "input": {} }),
                "turn-1",
            )
            .is_none());

        // task_started only links the task id back to the surfaced run.
        assert!(subagents
            .system_event(
                &json!({ "subtype": "task_started", "task_id": "agent-9",
                    "task_type": "local_agent", "tool_use_id": "toolu_1" }),
                "turn-1",
            )
            .is_none());

        let progress = subagents
            .system_event(
                &json!({ "subtype": "task_progress", "task_id": "agent-9",
                    "status": "running" }),
                "turn-1",
            )
            .unwrap();
        assert_eq!(progress.0, "subagent.updated");
        assert_eq!(progress.1["subagentId"], "toolu_1");

        let done = subagents
            .system_event(
                &json!({ "subtype": "task_notification", "task_id": "agent-9",
                    "status": "completed", "summary": "mapped",
                    "agent_id": "aabe", "output_file": "/tmp/out.md" }),
                "turn-1",
            )
            .unwrap();
        assert_eq!(done.0, "subagent.completed");
        assert_eq!(done.1["subagentId"], "toolu_1");
        assert_eq!(done.1["providerItemId"], "toolu_1");
        assert_eq!(done.1["agentId"], "aabe");
        assert_eq!(done.1["result"], "mapped");
        assert_eq!(done.1["metadata"]["outputFile"], "/tmp/out.md");

        // A trailing tool_result cannot reopen a notified run.
        assert!(subagents
            .finish_from_tool("toolu_1", &json!({}), &json!("late"), false, "turn-1")
            .is_none());
    }

    #[test]
    fn foreground_agent_finishes_with_its_tool_result() {
        let mut subagents = ClaudeSubagents::default();
        subagents.start_from_tool(
            &json!({ "type": "tool_use", "id": "toolu_2", "name": "Agent",
                "input": { "description": "run tests" } }),
            "turn-1",
        );

        let done = subagents
            .finish_from_tool("toolu_2", &json!({}), &json!("all green"), false, "turn-1")
            .unwrap();
        assert_eq!(done.0, "subagent.completed");
        assert_eq!(done.1["result"], "all green");
    }

    #[test]
    fn async_tool_result_keeps_the_subagent_running() {
        let mut subagents = ClaudeSubagents::default();
        subagents.start_from_tool(
            &json!({ "type": "tool_use", "id": "toolu_3", "name": "Task",
                "input": { "description": "background sweep" } }),
            "turn-1",
        );

        let update = subagents
            .finish_from_tool(
                "toolu_3",
                &json!({ "tool_use_result": {
                    "isAsync": true, "status": "async_launched", "agentId": "a1" } }),
                &json!("Async agent launched"),
                false,
                "turn-1",
            )
            .unwrap();
        assert_eq!(update.0, "subagent.updated");
        assert_eq!(update.1["agentId"], "a1");
        assert_eq!(update.1["status"], "running");

        // Non-agent task kinds never enter the subagent surface.
        assert!(subagents
            .system_event(
                &json!({ "subtype": "task_started", "task_id": "sh-1",
                    "task_type": "local_shell", "tool_use_id": "toolu_sh" }),
                "turn-1",
            )
            .is_none());
    }

    #[test]
    fn text_only_prompt_keeps_the_existing_string_shape() {
        let prompt = DriverPrompt {
            turn_id: "turn-1".to_owned(),
            text: "hello".to_owned(),
            content: Vec::new(),
            skills: Vec::new(),
            model: None,
            reasoning_effort: None,
            permission_mode: None,
            work_mode: None,
            permission_profile: None,
            sandbox_mode: None,
            approval_policy: None,
        };

        assert_eq!(claude_user_content(&prompt), json!("hello"));
    }

    #[test]
    fn image_prompt_uses_claude_streaming_content_blocks() {
        let prompt = DriverPrompt {
            turn_id: "turn-2".to_owned(),
            text: "describe this".to_owned(),
            content: vec![DriverPromptContent::Image {
                path: None,
                data: "cG5n".to_owned(),
                mime_type: "image/png".to_owned(),
            }],
            skills: Vec::new(),
            model: None,
            reasoning_effort: None,
            permission_mode: None,
            work_mode: None,
            permission_profile: None,
            sandbox_mode: None,
            approval_policy: None,
        };

        assert_eq!(
            claude_user_content(&prompt),
            json!([
                { "type": "text", "text": "describe this" },
                {
                    "type": "image",
                    "source": {
                        "type": "base64",
                        "media_type": "image/png",
                        "data": "cG5n",
                    },
                },
            ])
        );
    }

    #[test]
    fn initialize_response_commands_become_prompt_invocations() {
        let response = json!({
            "type": "control_response",
            "response": {
                "subtype": "success",
                "request_id": "todex-initialize",
                "response": {
                    "commands": [
                        {
                            "name": "compact",
                            "description": "Free up context by summarizing the conversation so far",
                            "argumentHint": "<optional custom summarization instructions>",
                            "builtin": true,
                        },
                        {
                            "name": "docs",
                            "description": "living docs",
                            "argumentHint": "",
                            "aliases": ["anthropic-skills:docs"],
                        },
                        { "name": "my-skill", "description": "user skill", "argumentHint": "" },
                        { "name": "__remote-workflow", "description": "internal", "argumentHint": "" },
                        { "name": "compact", "description": "duplicate", "argumentHint": "" },
                    ],
                },
            },
        });

        let commands = claude_command_catalog(&response);
        assert_eq!(commands.len(), 3);
        assert_eq!(commands[0].name, "compact");
        assert_eq!(commands[0].source, "builtin");
        assert_eq!(commands[0].invocation, "prompt");
        assert_eq!(
            commands[0].argument_hint.as_deref(),
            Some("<optional custom summarization instructions>")
        );
        assert_eq!(commands[1].name, "docs");
        assert_eq!(commands[1].source, "plugin");
        assert_eq!(commands[1].argument_hint, None);
        assert_eq!(commands[2].name, "my-skill");
        assert_eq!(commands[2].source, "user");

        assert!(claude_command_catalog(&json!({"response": {"response": {}}})).is_empty());
    }

    #[test]
    fn rate_limit_event_normalizes_unified_windows() {
        let message = json!({
            "type": "rate_limit_event",
            "rate_limit_info": {
                "status": "allowed",
                "rateLimitType": "five_hour",
                "resetsAt": 1791034800,
                "isUsingOverage": false,
                "unifiedWindows": {
                    "five_hour": { "utilization": 0.26, "resetsAt": 1791034800 },
                    "seven_day": { "utilization": 0.11, "resetsAt": 1791493200 },
                },
            },
        });

        let quota = claude_quota_event(&message).expect("quota payload");
        assert_eq!(quota["provider"], "claude-code");
        assert_eq!(quota["scope"], "account");
        assert_eq!(quota["windows"].as_array().unwrap().len(), 2);
        let five_hour = &quota["windows"][0];
        assert_eq!(five_hour["id"], "five_hour");
        assert_eq!(five_hour["usedPercent"], 26.0);
        assert_eq!(five_hour["resetsAt"], 1791034800);
        assert_eq!(quota["windows"][1]["id"], "seven_day");
    }

    #[test]
    fn result_error_reads_errors_array_when_result_is_absent() {
        // Claude ≥2.x reports a failed `--resume` with `errors[]` and no
        // `result` string; surfacing that text is what makes a stale
        // transcript diagnosable.
        let missing_session = json!({
            "type": "result",
            "subtype": "error_during_execution",
            "is_error": true,
            "errors": ["No conversation found with session ID: 11111111-1111-1111-1111-111111111111"],
        });
        assert_eq!(
            claude_result_error(&missing_session).as_deref(),
            Some("No conversation found with session ID: 11111111-1111-1111-1111-111111111111")
        );
        assert_eq!(
            claude_result_error(&json!({
                "result": "Invalid API key · Please run /login",
                "errors": ["ignored"],
            }))
            .as_deref(),
            Some("Invalid API key · Please run /login")
        );
        assert_eq!(
            claude_result_error(&json!({"result": " ", "errors": ["a", "b"]})).as_deref(),
            Some("a; b")
        );
        assert!(claude_result_error(&json!({"errors": []})).is_none());
        assert!(claude_result_error(&json!({"errors": [1, null]})).is_none());
        assert!(claude_result_error(&json!({})).is_none());
    }

    #[tokio::test]
    async fn transcript_loadable_requires_a_parseable_first_line() {
        let root = std::env::temp_dir().join(format!(
            "todex-claude-transcript-{}",
            uuid::Uuid::new_v4().simple()
        ));
        tokio::fs::create_dir_all(&root).await.unwrap();
        let path = root.join("session.jsonl");
        assert!(!claude_transcript_loadable(&path).await);

        tokio::fs::write(&path, "").await.unwrap();
        assert!(!claude_transcript_loadable(&path).await);

        tokio::fs::write(&path, "not json\n").await.unwrap();
        assert!(!claude_transcript_loadable(&path).await);

        tokio::fs::write(&path, "[1, 2]\n").await.unwrap();
        assert!(!claude_transcript_loadable(&path).await);

        tokio::fs::write(
            &path,
            "{\"type\":\"system\",\"subtype\":\"init\"}\nnot-json-later-lines-do-not-matter\n",
        )
        .await
        .unwrap();
        assert!(claude_transcript_loadable(&path).await);
        let _ = tokio::fs::remove_dir_all(&root).await;
    }

    #[test]
    fn rate_limit_event_without_windows_falls_back_to_top_level() {
        let message = json!({
            "type": "rate_limit_event",
            "rate_limit_info": {
                "status": "allowed",
                "rateLimitType": "five_hour",
                "resetsAt": 1791034800,
            },
        });

        let quota = claude_quota_event(&message).expect("quota payload");
        assert_eq!(quota["windows"][0]["id"], "five_hour");
        assert_eq!(quota["windows"][0]["resetsAt"], 1791034800);
        assert!(
            quota["windows"][0].get("usedPercent").is_none()
                || quota["windows"][0]["usedPercent"].is_null()
        );
        assert!(claude_quota_event(&json!({"type": "rate_limit_event"})).is_none());
    }

    #[test]
    fn turn_usage_is_the_process_ledger_difference_across_results() {
        let result = |input: u64, cache_read: u64, cache_write: u64, output: u64, cost: f64| {
            json!({
                "usage": { "input_tokens": 1, "output_tokens": 1 },
                "total_cost_usd": cost,
                "modelUsage": { "claude-haiku-5-5": {
                    "inputTokens": input, "outputTokens": output,
                    "cacheReadInputTokens": cache_read, "cacheCreationInputTokens": cache_write,
                }},
            })
        };
        let mut ledger = super::ClaudeUsageLedger::default();
        // A turn with a subagent, then a follow-up invocation in the same turn.
        let first = ledger.advance(&result(6, 32597, 34135, 369, 0.01)).unwrap();
        let second = ledger
            .advance(&result(8, 60712, 34850, 492, 0.015))
            .unwrap();
        let mut turn = super::ClaudeUsage::default();
        turn.add(&first);
        turn.add(&second);
        assert_eq!(
            turn.counters(),
            json!({ "input_tokens": 8, "output_tokens": 492, "cache_read_input_tokens": 60712, "cache_creation_input_tokens": 34850 })
        );
        assert!((turn.cost_usd - 0.015).abs() < 1e-9);
        // The next turn on the same process counts only its own calls.
        let next = ledger
            .advance(&result(10, 88827, 35565, 495, 0.016))
            .unwrap();
        assert_eq!(next.counters()["cache_read_input_tokens"], 28115);
        // Older CLIs without `modelUsage` fall back to the frame's usage.
        let legacy = ledger
            .advance(&json!({ "usage": { "input_tokens": 4, "output_tokens": 2 } }))
            .unwrap();
        assert_eq!(legacy.counters()["input_tokens"], 4);
        assert!(ledger.advance(&json!({})).is_none());
    }

    #[test]
    fn context_window_comes_from_the_main_model_usage() {
        let result = json!({ "modelUsage": {
            "claude-opus-5-5": { "contextWindow": 1_000_000 },
            "claude-haiku-5-5": { "contextWindow": 200_000 },
        }});
        assert_eq!(
            super::claude_context_window(&result, Some("claude-opus-5-5")),
            Some(1_000_000)
        );
        assert_eq!(
            super::claude_context_window(&result, Some("claude-haiku-5-5")),
            Some(200_000)
        );
        assert_eq!(super::claude_context_window(&result, None), None);
        let single = json!({ "modelUsage": { "claude-haiku-5-5": { "contextWindow": 200_000 } } });
        assert_eq!(super::claude_context_window(&single, None), Some(200_000));
        assert_eq!(super::claude_context_window(&json!({}), None), None);
    }

    #[test]
    fn usage_limit_text_resolves_the_reported_zone_and_weekday() {
        use chrono::{TimeZone, Utc};

        let now = Utc.with_ymd_and_hms(2026, 10, 6, 8, 22, 17).unwrap();
        let session = usage_limit_reset(
            "You've hit your session limit · resets 5:30pm (Australia/Perth)",
            now,
        )
        .expect("session reset");
        // Perth is UTC+8 with no daylight-saving; 5:30pm is 09:30 UTC the same day.
        assert_eq!(
            session,
            Utc.with_ymd_and_hms(2026, 10, 6, 9, 30, 0).unwrap()
        );

        // 2026-10-06 is a Tuesday, so Monday 12:00am is the following Monday.
        let weekly = usage_limit_reset(
            "You've hit your weekly limit · resets Mon 12:00am (UTC)",
            now,
        )
        .expect("weekly reset");
        assert_eq!(weekly, Utc.with_ymd_and_hms(2026, 10, 12, 0, 0, 0).unwrap());

        assert!(usage_limit_reset("fixture failure", now).is_none());
        assert!(usage_limit_reset("You've hit your session limit", now).is_none());
        // Non-ASCII text after "reset" must not split a character.
        assert!(usage_limit_reset("usage limit reset 时间", now).is_none());
        assert!(usage_limit_reset("resets 5:30 下午", now).is_none());
    }

    #[test]
    fn a_usage_limit_reset_that_just_passed_is_now() {
        use chrono::{TimeZone, Utc};

        let now = Utc.with_ymd_and_hms(2026, 10, 6, 9, 35, 0).unwrap();
        let text = |clock: &str| format!("You've hit your session limit · resets {clock} (UTC)");
        // Five minutes ago: the reset just passed, not tomorrow's.
        assert_eq!(usage_limit_reset(&text("9:30am"), now), Some(now));
        assert_eq!(usage_limit_reset(&text("9:25am"), now), Some(now));
        // Older than ten minutes: the next day's.
        assert_eq!(
            usage_limit_reset(&text("9:20am"), now),
            Some(Utc.with_ymd_and_hms(2026, 10, 7, 9, 20, 0).unwrap())
        );
        // Just before midnight, read just after it.
        let after_midnight = Utc.with_ymd_and_hms(2026, 10, 7, 0, 3, 0).unwrap();
        assert_eq!(
            usage_limit_reset(&text("11:58pm"), after_midnight),
            Some(after_midnight)
        );
        // A weekly reset that just passed is not next week's.
        assert_eq!(
            usage_limit_reset(
                "You've hit your weekly limit · resets Tue 9:30am (UTC)",
                now
            ),
            Some(now)
        );
    }

    #[test]
    fn assistant_quota_limits_are_a_rejected_reset() {
        use chrono::{TimeZone, Utc};

        let frame = json!({
            "type": "assistant",
            "error": "rate_limit",
            "is_api_error_message": true,
            "quotaLimits": {
                "status": "rejected",
                "resetsAt": 1791279000,
                "rateLimitType": "five_hour"
            },
            "message": { "content": [{ "type": "text", "text": "You've hit your session limit" }] }
        });
        assert_eq!(
            claude_frame_limit_reset(&frame),
            Utc.timestamp_opt(1791279000, 0).single()
        );
        let allowed = json!({
            "type": "rate_limit_event",
            "rate_limit_info": { "status": "allowed", "resetsAt": 1791279000, "rateLimitType": "five_hour" }
        });
        assert!(claude_frame_limit_reset(&claude_quota_event(&allowed).unwrap()).is_none());
    }
}

/// Token and cost counters of Claude model calls, in Anthropic's sense: cache
/// reads and writes are counted beside `input`.
#[derive(Clone, Debug, Default, PartialEq)]
pub(super) struct ClaudeUsage {
    input: u64,
    output: u64,
    cache_read: u64,
    cache_write: u64,
    cost_usd: f64,
}

impl ClaudeUsage {
    /// Sum of a `result` frame's `modelUsage`, which covers every model the
    /// process has called (subagents included) since it started.
    fn from_model_usage(result: &Value) -> Option<Self> {
        let models = result.get("modelUsage")?.as_object()?;
        let count = |entry: &Value, key: &str| entry.get(key).and_then(Value::as_u64).unwrap_or(0);
        let mut total = Self::default();
        for entry in models.values() {
            total.input += count(entry, "inputTokens");
            total.output += count(entry, "outputTokens");
            total.cache_read += count(entry, "cacheReadInputTokens");
            total.cache_write += count(entry, "cacheCreationInputTokens");
        }
        total.cost_usd = result
            .get("total_cost_usd")
            .and_then(Value::as_f64)
            .unwrap_or(0.0);
        Some(total)
    }

    /// A `result` frame's own `usage`: the main thread of one invocation.
    fn from_result_usage(result: &Value) -> Option<Self> {
        let usage = result.get("usage").filter(|usage| usage.is_object())?;
        let count = |key: &str| usage.get(key).and_then(Value::as_u64).unwrap_or(0);
        Some(Self {
            input: count("input_tokens"),
            output: count("output_tokens"),
            cache_read: count("cache_read_input_tokens"),
            cache_write: count("cache_creation_input_tokens"),
            cost_usd: 0.0,
        })
    }

    fn add(&mut self, other: &Self) {
        self.input += other.input;
        self.output += other.output;
        self.cache_read += other.cache_read;
        self.cache_write += other.cache_write;
        self.cost_usd += other.cost_usd;
    }

    /// `self - earlier`, or `None` when a counter went backwards.
    fn since(&self, earlier: &Self) -> Option<Self> {
        Some(Self {
            input: self.input.checked_sub(earlier.input)?,
            output: self.output.checked_sub(earlier.output)?,
            cache_read: self.cache_read.checked_sub(earlier.cache_read)?,
            cache_write: self.cache_write.checked_sub(earlier.cache_write)?,
            cost_usd: (self.cost_usd - earlier.cost_usd).max(0.0),
        })
    }

    fn counters(&self) -> Value {
        json!({
            "input_tokens": self.input,
            "output_tokens": self.output,
            "cache_read_input_tokens": self.cache_read,
            "cache_creation_input_tokens": self.cache_write,
        })
    }
}

/// Claude's `modelUsage` and `total_cost_usd` count from process start, so
/// the share of each `result` is its difference from the previous one.
#[derive(Default)]
pub(super) struct ClaudeUsageLedger {
    seen: ClaudeUsage,
}

impl ClaudeUsageLedger {
    /// The usage a `result` frame adds. Without `modelUsage` (older CLIs)
    /// the frame's own `usage` stands in, which misses subagent calls.
    fn advance(&mut self, result: &Value) -> Option<ClaudeUsage> {
        let Some(total) = ClaudeUsage::from_model_usage(result) else {
            return ClaudeUsage::from_result_usage(result);
        };
        let added = total.since(&self.seen).unwrap_or_else(|| total.clone());
        self.seen = total;
        Some(added)
    }
}

/// Context window of the main model from a `result` frame's `modelUsage`,
/// keyed by model id. With no main model seen, a single entry is unambiguous.
fn claude_context_window(result: &Value, model: Option<&str>) -> Option<u64> {
    let models = result.get("modelUsage")?.as_object()?;
    let entry = match model {
        Some(model) => models.get(model).or_else(|| {
            models
                .iter()
                .find(|(id, _)| id.starts_with(model) || model.starts_with(id.as_str()))
                .map(|(_, entry)| entry)
        }),
        None if models.len() == 1 => models.values().next(),
        None => None,
    }?;
    entry.get("contextWindow").and_then(Value::as_u64)
}

/// The error text inside a Claude `result` frame. Current CLI releases put
/// the detail in `errors[]` and leave `result` unset, so check every known
/// slot before falling back — otherwise the real reason ("No conversation
/// found with session ID", auth failures) is replaced by a generic message.
fn claude_result_error(message: &Value) -> Option<String> {
    if let Some(text) = message
        .get("result")
        .and_then(Value::as_str)
        .filter(|text| !text.trim().is_empty())
    {
        return Some(text.to_owned());
    }
    let errors = message
        .get("errors")
        .and_then(Value::as_array)?
        .iter()
        .filter_map(Value::as_str)
        .collect::<Vec<_>>();
    if errors.is_empty() {
        return None;
    }
    Some(errors.join("; "))
}

// Protocol reference: anthropics/claude-agent-sdk-python, _internal/query.py.
// Use the same initialize exchange as the official Agent SDK before sending
// a user turn. A rejected/unsupported control channel must fail before tools run.
//
// `expected_mode` is the requested `--permission-mode`: when the response
// reports another one in place of bypass or auto, a `get_settings` probe
// finds the settings layer that disabled it (see [`permission_mode`]).
async fn initialize_claude(
    process: &mut JsonLineProcess,
    cancel: &mut watch::Receiver<bool>,
    expected_mode: Option<&str>,
) -> Result<(Vec<ProviderCommandDescriptor>, permission_mode::ModeReport), AppError> {
    process
        .send(&json!({
            "type": "control_request",
            "request_id": "todex-initialize",
            "request": { "subtype": "initialize", "hooks": null }
        }))
        .await?;
    let initialize = async {
        loop {
            let Some(message) = process.read().await? else {
                return Err(provider_exit_error(
                    process,
                    "Claude Code closed during initialization",
                )
                .await);
            };
            if message.get("type").and_then(Value::as_str) == Some("result")
                && message.get("is_error").and_then(Value::as_bool) == Some(true)
            {
                let detail = claude_result_error(&message)
                    .unwrap_or_else(|| "provider rejected startup".to_owned());
                return Err(provider_exit_error(
                    process,
                    &format!("Claude Code initialization failed: {detail}"),
                )
                .await);
            }
            if answer_early_control_request(process, &message).await? {
                continue;
            }
            if message.get("type").and_then(Value::as_str) == Some("control_response")
                && message
                    .pointer("/response/request_id")
                    .and_then(Value::as_str)
                    == Some("todex-initialize")
            {
                return if message.pointer("/response/subtype").and_then(Value::as_str)
                    == Some("success")
                {
                    let report = permission_mode::ModeReport {
                        reported: message
                            .pointer("/response/response/current_permission_mode")
                            .and_then(Value::as_str)
                            .map(str::to_owned),
                        settings: None,
                    };
                    Ok((claude_command_catalog(&message), report))
                } else {
                    Err(AppError::ProviderUnavailable(format!(
                        "Claude Code initialization rejected: {}",
                        message
                            .pointer("/response/error")
                            .and_then(Value::as_str)
                            .unwrap_or("unsupported control protocol")
                    )))
                };
            }
        }
    };
    let (commands, mut report) = tokio::select! {
        result = tokio::time::timeout(super::process::control_timeout()?, initialize) => {
            result.map_err(|_| AppError::ProviderUnavailable("Claude Code initialization timed out".to_owned()))??
        }
        _ = cancel.changed() => return Err(AppError::InvalidRequest("Claude Code initialization cancelled".to_owned())),
    };
    let refused = report
        .reported
        .as_deref()
        .is_some_and(|reported| permission_mode::needs_settings_probe(expected_mode, reported));
    if refused {
        // Only explains the refusal, so it never fails the start: an
        // unanswered or failed probe just names no settings layer.
        report.settings = Some(
            tokio::select! {
                sources = tokio::time::timeout(SETTINGS_PROBE_TIMEOUT, probe_settings(process)) => {
                    sources.ok().flatten()
                }
                _ = cancel.changed() => return Err(AppError::InvalidRequest("Claude Code initialization cancelled".to_owned())),
            }
            .unwrap_or_default(),
        );
    }
    Ok((commands, report))
}

/// How long start-up waits on the `get_settings` probe.
const SETTINGS_PROBE_TIMEOUT: Duration = Duration::from_secs(5);

/// Sends `get_settings` and reads until its answer; `None` when the process
/// ends first or the request cannot be sent.
async fn probe_settings(
    process: &mut JsonLineProcess,
) -> Option<permission_mode::DisablingSources> {
    if let Err(error) = process.send(&permission_mode::settings_probe()).await {
        tracing::debug!(%error, "could not ask Claude Code for its settings");
        return None;
    }
    loop {
        let message = process.read().await.ok()??;
        if let Some(sources) = permission_mode::settings_probe_answer(&message) {
            return Some(sources);
        }
        answer_early_control_request(process, &message).await.ok()?;
    }
}

/// Refuses a `control_request` Claude sends before any turn owns the
/// session (during `initialize`): unanswered, Claude would wait on it.
/// Returns whether `message` was one.
async fn answer_early_control_request(
    process: &mut JsonLineProcess,
    message: &Value,
) -> Result<bool, AppError> {
    if message.get("type").and_then(Value::as_str) != Some("control_request") {
        return Ok(false);
    }
    let Some(request_id) = message.get("request_id").and_then(Value::as_str) else {
        tracing::warn!("Claude control request is missing request_id");
        return Ok(true);
    };
    process
        .send(&json!({
            "type": "control_response",
            "response": {
                "subtype": "error",
                "request_id": request_id,
                "error": "TodeX session is starting",
            }
        }))
        .await?;
    Ok(true)
}

/// The initialize response carries the session's slash-command catalog —
/// builtins, workspace commands, plugin commands and invocable skills — each
/// invoked by sending `/name` as an ordinary user message. `builtin: true`
/// marks CLI-native commands and an alias containing `:` identifies the plugin
/// form (`plugin:command`), so user-authored entries are what remain.
fn claude_command_catalog(initialize_response: &Value) -> Vec<ProviderCommandDescriptor> {
    let Some(items) = initialize_response
        .pointer("/response/response/commands")
        .and_then(Value::as_array)
    else {
        return Vec::new();
    };
    let mut seen = HashSet::new();
    items
        .iter()
        .filter_map(|item| {
            let name = item
                .get("name")
                .and_then(Value::as_str)?
                .trim()
                .trim_start_matches('/');
            // `__`-prefixed entries are internal harness plumbing, not commands.
            if name.is_empty() || name.starts_with("__") || !seen.insert(name.to_owned()) {
                return None;
            }
            let builtin = item
                .get("builtin")
                .and_then(Value::as_bool)
                .unwrap_or(false);
            let plugin = item
                .get("aliases")
                .and_then(Value::as_array)
                .is_some_and(|aliases| {
                    aliases
                        .iter()
                        .any(|alias| alias.as_str().is_some_and(|alias| alias.contains(':')))
                });
            Some(ProviderCommandDescriptor {
                name: name.to_owned(),
                description: item
                    .get("description")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_owned(),
                source: if builtin {
                    "builtin"
                } else if plugin {
                    "plugin"
                } else {
                    "user"
                }
                .to_owned(),
                source_info: None,
                invocation: "prompt".to_owned(),
                argument_hint: item
                    .get("argumentHint")
                    .and_then(Value::as_str)
                    .map(|hint| hint.trim().to_owned())
                    .filter(|hint| !hint.is_empty()),
                package_name: None,
                package_version: None,
            })
        })
        .collect()
}

/// How a turn meets the Claude process.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum TurnStart {
    /// An idle process: the prompt starts the work.
    Prompt,
    /// Claude already started an invocation of its own (see
    /// [`session_work`]) that no turn took over: the prompt queues behind
    /// it and the turn reads both.
    AfterOwnWork,
    /// A provider-initiated turn: nothing is sent, the turn reads the
    /// invocation Claude started on its own.
    Adopt,
}

/// One turn on an initialized Claude process: sends the prompt (see
/// [`TurnStart`]) and reads until the turn ends. `work` follows the
/// session's background work across turns. A cancelled turn returns at once
/// with the process still mid-turn; the caller interrupts or stops it.
/// `subagents` follows the process's subagent runs across turns.
#[allow(clippy::too_many_arguments)]
async fn run_claude_turn(
    process: &mut JsonLineProcess,
    usage_ledger: &mut ClaudeUsageLedger,
    mode: &mut permission_mode::ModeReport,
    context: DriverContext,
    prompt: DriverPrompt,
    requested_session_id: String,
    sink: &DriverEventSink,
    catalogs: &Catalogs,
    cancel: &mut watch::Receiver<bool>,
    work: &mut session_work::SessionWork,
    subagents: &mut ClaudeSubagents,
    start: TurnStart,
) -> Result<DriverTurnResult, AppError> {
    work.begin_turn();
    let adopt = start == TurnStart::Adopt;
    let execution = super::types::resolve_execution_config(
        context.manifest.provider,
        prompt.permission_mode.as_deref(),
        prompt.work_mode.as_deref(),
        prompt.permission_profile.as_deref(),
        prompt.sandbox_mode.as_deref(),
        prompt.approval_policy.as_deref(),
    )?;
    let expected_mode = execution.provider_mode.clone();
    // Start-up already showed Claude refusing full access: fail before the
    // prompt reaches the model.
    if let Some(reported) = mode.reported.as_deref() {
        if permission_mode::bypass_mode_refused(expected_mode.as_deref(), reported) {
            return Err(bypass_refused(sink, reported, mode.bypass_source()).await);
        }
    }
    let user_message = json!({
        "type": "user",
        "session_id": requested_session_id,
        "parent_tool_use_id": null,
        "message": {
            "role": "user",
            "content": claude_user_content(&prompt),
        }
    });
    if !adopt {
        process.send(&user_message).await?;
    }
    // Behind Claude's own invocation, its `idle` ends nothing: only the one
    // after Claude took (and replayed) this prompt does.
    let mut unreplayed = (start == TurnStart::AfterOwnWork)
        .then(|| user_message.pointer("/message/content").cloned())
        .flatten();
    // The transcript exists once initialize succeeds; persist its id right
    // away so a failed or cancelled turn still resumes instead of hitting
    // "Session ID is already in use" on the next launch. Written after the
    // prompt so the journal fsync overlaps the model call.
    let mut provider_state = context.provider_state;
    if provider_state.native_session_id.as_deref() != Some(requested_session_id.as_str()) {
        provider_state.native_session_id = Some(requested_session_id.clone());
        provider_state.recoverable = true;
        sink.save_provider_state(provider_state.clone()).await?;
    }

    let mut saw_output = false;
    let mut tools = ClaudeToolCalls::default();
    let mut background_tasks = BackgroundTasks::default();
    // Epoch from a rejected `rate_limit_event` or `quotaLimits`. Preferred
    // over the wall clock in the error text, which Claude rounds.
    let mut rejected_reset: Option<DateTime<Utc>> = None;
    // Text of an API-error assistant frame, used when the following `result`
    // has no usable message of its own.
    let mut limit_text: Option<String> = None;
    // Model of the latest main-thread API call, which picks the context
    // window out of the process-wide `modelUsage` ledger.
    let mut main_model: Option<String> = None;
    // Every model call of this turn, subagents and follow-up invocations
    // included; each `result` adds its share of the process ledger.
    let mut turn_usage = ClaudeUsage::default();
    // A `result` with zero model turns means the invocation ended without an
    // API call — a task notification queued during resume consumes the prompt
    // without answering it. The process stays alive on the open stream, so
    // resend the prompt instead of failing the turn. Once the prompt has been
    // answered, a zero-turn `result` is a follow-up invocation for a task
    // notification another hand-back already covered; resending then would
    // replay the user's prompt into the conversation.
    // An adopted invocation has no prompt of ours to resend.
    let mut empty_results = if adopt {
        MAX_EMPTY_RESULT_RESENDS
    } else {
        0_u32
    };
    // With `CLAUDE_CODE_EMIT_SESSION_STATE_EVENTS` Claude reports
    // `session_state_changed`, and `idle` comes only once its input queue and
    // background work are drained. That is the authoritative end of the turn:
    // a subagent's `SubagentHandback` reaches the main session as a queued
    // `peer` message with no `task_notification`, so `BackgroundTasks` alone
    // can end the turn on a `result` with a hand-back still queued and kill
    // the process before the model reads it. `settled` holds the latest
    // successful `result` until `idle`; a CLI that never reports session
    // state falls back to `BackgroundTasks`.
    // Claude's own invocation began with a `running` the worker already read.
    let mut session_states = start != TurnStart::Prompt;
    let mut settled: Option<Value> = None;
    // When the settled `result` stops waiting for `idle`: Claude withholds
    // that frame while it still counts work as live, including work whose
    // completion TodeX already saw, so a turn could otherwise stay open until
    // the user cancelled it. See [`IDLE_CONFIRM_GRACE`].
    let mut idle_deadline: Option<Instant> = None;
    // Prompts queued behind Claude's own invocation that it never took.
    let mut prompt_resends = 0_u32;
    // `can_use_tool` waits on the user for up to the permission timeout, so
    // control requests are answered by detached tasks while this loop keeps
    // draining stdout — awaiting a prompt inline would let the pipe fill and
    // freeze the whole Claude process, async subagents included, until its
    // own stall watchdog kills them. Responses come back through the channel
    // so `process` stays single-owner. When the turn ends the cancel channel
    // closes, which resolves any prompt still waiting, and the closed
    // response channel drops the answer.
    let (response_tx, mut response_rx) = mpsc::unbounded_channel::<Value>();
    let open_requests = OpenRequests::default();
    // Whether this turn already reported Claude replacing `auto`.
    let mut auto_fallback = AutoFallback::Active;
    // Model from this invocation's `init` frame, for that report.
    let mut init_model: Option<String> = None;
    let result = loop {
        let message = tokio::select! {
            message = process.read_frame() => sink.provider_frame(message?).await?,
            response = response_rx.recv() => {
                // `response_tx` lives in this scope, so recv() cannot observe
                // the channel closing while the loop runs.
                if let Some(response) = response {
                    process.send(&response).await?;
                }
                continue;
            }
            // Claude owes a follow-up it never starts, or swallows the
            // `idle` that would end the turn: end it on the answer it already
            // gave instead of holding the client in "working".
            _ = confirmation_due(idle_deadline), if !open_requests.outstanding() => {
                idle_deadline = None;
                let Some(result) = settled.take() else {
                    continue;
                };
                if unreplayed.is_some() {
                    if prompt_resends >= MAX_EMPTY_RESULT_RESENDS {
                        return Err(AppError::ProviderUnavailable(
                            "Claude Code never took the queued prompt; please retry".to_owned(),
                        ));
                    }
                    // The prompt never reached the model: send it again rather
                    // than ending a turn that shows an answer to it.
                    prompt_resends += 1;
                    unreplayed = None;
                    process.send(&user_message).await?;
                    continue;
                }
                tracing::warn!(
                    grace_ms = idle_confirm_grace(&background_tasks).as_millis() as u64,
                    background_tasks = background_tasks.pending(),
                    "Claude Code did not report idle after its result; ending the turn"
                );
                sink.emit(
                    "provider.event",
                    json!({
                        "provider": "claude-code",
                        "providerMethod": "idle_timeout",
                        "metadata": {
                            "taskIds": background_tasks.ids(),
                            "tasks": background_tasks.tasks(),
                        },
                    }),
                )
                .await?;
                break result;
            }
                changed = cancel.changed() => {
                    let _ = changed;
                    return Ok(DriverTurnResult {
                    native_session_id: Some(requested_session_id.clone()),
                    stop_reason: "cancelled".to_owned(),
                    cancelled: true,
                });
            }
        };
        let Some(message) = message else {
            return Err(provider_exit_error(process, "Claude Code closed stdout").await);
        };
        if message.is_null() {
            continue;
        }
        match message.get("type").and_then(Value::as_str) {
            Some("result") => {
                if let Some(usage) = usage_ledger.advance(&message) {
                    turn_usage.add(&usage);
                    // A sum of the turn's calls, so it is not a context
                    // snapshot; the per-call `assistant` frames are.
                    sink.emit("usage.updated", json!({ "provider": "claude-code", "turnId": prompt.turn_id, "source": "provider", "scope": "turn", "aggregation": "sum", "usage": turn_usage.counters(), "costUsd": turn_usage.cost_usd, "contextWindow": claude_context_window(&message, main_model.as_deref()) })).await?;
                }
                let is_error = message
                    .get("is_error")
                    .and_then(Value::as_bool)
                    .unwrap_or(false);
                if is_error {
                    let detail = claude_result_error(&message)
                        .unwrap_or_else(|| "Claude Code returned an error".to_owned());
                    let parsed = usage_limit_reset(&detail, Utc::now()).or_else(|| {
                        limit_text
                            .as_deref()
                            .and_then(|text| usage_limit_reset(text, Utc::now()))
                    });
                    // Re-emit at the end of the turn so a later non-exhausted
                    // snapshot cannot clear the wait before `after_turn`.
                    if let Some(until) = rejected_reset.or(parsed) {
                        sink.emit("quota.updated", rejected_quota_payload(until))
                            .await?;
                    }
                    return Err(AppError::ProviderUnavailable(
                        detail.chars().take(1000).collect(),
                    ));
                }
                if message.get("num_turns").and_then(Value::as_u64) == Some(0)
                    && !saw_output
                    && empty_results < MAX_EMPTY_RESULT_RESENDS
                {
                    empty_results += 1;
                    process.send(&user_message).await?;
                    continue;
                }
                // `result` ends the model turn, not the process: while
                // background work remains, Claude keeps the stream open,
                // delivers task_notification, and runs a follow-up turn that
                // ends in another `result`. Terminating here kills those
                // subagents mid-flight or drops their queued hand-back. With
                // session state events the pending notice is only for clients;
                // `idle` decides when the turn ends.
                //
                // The `result` is the turn's answer either way, so it is kept
                // even while that work is outstanding: dropping it left the
                // turn with nothing to end on when the `idle` never came.
                if background_tasks.pending() {
                    sink.emit(
                        "provider.event",
                        json!({
                            "provider": "claude-code",
                            "providerMethod": "background_tasks_pending",
                            "metadata": {
                                "taskIds": background_tasks.ids(),
                                "tasks": background_tasks.tasks(),
                            },
                        }),
                    )
                    .await?;
                }
                if session_states {
                    settled = Some(message);
                    idle_deadline = Some(Instant::now() + idle_confirm_grace(&background_tasks));
                    continue;
                }
                // No session state events (an older CLI): only outstanding
                // work keeps the turn open.
                if !background_tasks.pending() {
                    break message;
                }
                settled = Some(message);
                idle_deadline = Some(Instant::now() + idle_confirm_grace(&background_tasks));
                continue;
            }
            Some("stream_event") => {
                saw_output = true;
                handle_stream_event(&message, &prompt.turn_id, &mut tools, sink).await?;
            }
            Some("assistant") => {
                saw_output = true;
                if let Some(until) = claude_frame_limit_reset(&message) {
                    rejected_reset = Some(until);
                }
                if claude_frame_is_api_error(&message) {
                    if let Some(text) = claude_assistant_text(&message) {
                        limit_text = Some(text);
                    }
                }
                let subagent = message.get("parent_tool_use_id").and_then(Value::as_str);
                if subagent.is_none() {
                    if let Some(model) = message.pointer("/message/model").and_then(Value::as_str) {
                        main_model = Some(model.to_owned());
                    }
                }
                sink.emit(
                    "message.completed",
                    json!({ "provider": "claude-code", "message": message.get("message"), "subagentId": subagent }),
                )
                .await?;
                // The streamed `tool_use` start carries an empty input; the
                // complete assistant message is the first with arguments.
                for block in content_blocks(&message, "tool_use") {
                    if subagent.is_none() {
                        work.apply_tool_use(block);
                    }
                    if let Some(id) = tools.record(block, subagent) {
                        if ClaudeSubagents::is_agent_tool(block.get("name")) {
                            if let Some(payload) = subagents.start_from_tool(block, &prompt.turn_id)
                            {
                                sink.emit("subagent.started", payload).await?;
                            }
                        }
                        sink.emit("tool.updated", tools.payload(&id, &prompt.turn_id, "delta"))
                            .await?;
                    }
                }
            }
            Some("user") => {
                if unreplayed.is_some()
                    && message.pointer("/message/content") == unreplayed.as_ref()
                {
                    unreplayed = None;
                }
                background_tasks.delivered(&message);
                let subagent = message.get("parent_tool_use_id").and_then(Value::as_str);
                for block in content_blocks(&message, "tool_result") {
                    if subagent.is_none() {
                        work.apply_tool_result(block, tokio::time::Instant::now());
                    }
                    if let Some(id) = tools.complete(block, subagent) {
                        let payload = tools.payload(&id, &prompt.turn_id, "completed");
                        sink.emit("tool.completed", payload.clone()).await?;
                        if let Some((event, subagent)) = subagents.finish_from_tool(
                            &id,
                            &message,
                            payload.get("result").unwrap_or(&Value::Null),
                            payload
                                .get("isError")
                                .and_then(Value::as_bool)
                                .unwrap_or(false),
                            &prompt.turn_id,
                        ) {
                            sink.emit(event, subagent).await?;
                        }
                    }
                }
            }
            Some("tool_progress") => {
                saw_output = true;
                let subagent = message.get("parent_tool_use_id").and_then(Value::as_str);
                if let Some(id) = message.get("tool_use_id").and_then(Value::as_str) {
                    tools.name_if_unknown(id, message.get("tool_name"), subagent);
                    sink.emit("tool.updated", tools.payload(id, &prompt.turn_id, "delta"))
                        .await?;
                }
            }
            Some("control_request") => {
                spawn_control_request(
                    &response_tx,
                    message,
                    sink,
                    cancel,
                    &open_requests,
                    &context.manifest.workspace,
                );
            }
            // Claude withdrew a request it is no longer waiting on (an
            // interrupt while `can_use_tool` was open): close its prompt.
            Some("control_cancel_request") => {
                if let Some(request_id) = message.get("request_id").and_then(Value::as_str) {
                    open_requests.withdraw(request_id);
                }
            }
            Some("system") => {
                if message.get("subtype").and_then(Value::as_str) == Some("session_state_changed") {
                    session_states = true;
                    let state = message.get("state").and_then(Value::as_str);
                    // Journaled so a stuck turn can be diagnosed from the
                    // conversation alone: the frames themselves are invisible
                    // to clients.
                    sink.emit(
                        "provider.event",
                        json!({
                            "provider": "claude-code",
                            "providerMethod": "session_state_changed",
                            "metadata": {
                                "state": state,
                                "waitingOnUser": message.get("waiting_on_user"),
                            },
                        }),
                    )
                    .await?;
                    match state {
                        // Claude waits on the user (a parked permission
                        // prompt): no deadline covers a person's decision.
                        Some("requires_action") => idle_deadline = None,
                        // A new invocation began: the previous answer no longer
                        // ends this turn.
                        Some("running") => {
                            settled = None;
                            idle_deadline = None;
                        }
                        // An `idle` before any successful `result` follows a
                        // zero-turn `result` whose prompt was just resent.
                        Some("idle") => {
                            if unreplayed.is_some() {
                                // Behind Claude's own invocation the prompt is
                                // queued in its input and taken next, so this
                                // `idle` answers nothing of this turn's. A
                                // prompt it never takes is resent once the
                                // turn has waited for `idle` too long.
                            } else if let Some(result) = settled.take() {
                                break result;
                            }
                        }
                        _ => {}
                    }
                    continue;
                }
                background_tasks.apply(&message);
                work.apply_system(&message);
                if let Some((event, payload)) = subagents.system_event(&message, &prompt.turn_id) {
                    sink.emit(event, payload).await?;
                }
                if message.get("subtype").and_then(Value::as_str) == Some("init") {
                    // `terminal_slash_commands` names the catalog entries bound
                    // to the local terminal (e.g. a prompt-bar color picker);
                    // remote clients are expected to hide them.
                    if let Some(terminal_commands) = message
                        .get("terminal_slash_commands")
                        .and_then(Value::as_array)
                    {
                        let terminal_commands: HashSet<&str> =
                            terminal_commands.iter().filter_map(Value::as_str).collect();
                        if !terminal_commands.is_empty() {
                            if let Some(catalog) =
                                catalogs.lock().await.get_mut(&context.manifest.id)
                            {
                                catalog.commands.retain(|command| {
                                    !terminal_commands.contains(command.name.as_str())
                                });
                            }
                        }
                    }
                    init_model = message
                        .get("model")
                        .and_then(Value::as_str)
                        .map(str::to_owned);
                    if let Some(actual) = message.get("permissionMode").and_then(Value::as_str) {
                        let expected = expected_mode.as_deref();
                        if permission_mode::bypass_mode_refused(expected, actual) {
                            return Err(bypass_refused(sink, actual, mode.bypass_source()).await);
                        } else if auto_mode_fell_back(expected, actual) {
                            // Claude Code quietly runs in another mode when
                            // the model or account has no auto mode, or a
                            // settings layer disables it. That mode still
                            // asks through `can_use_tool`, i.e. TodeX's own
                            // approval, so the turn continues.
                            auto_fallback
                                .report(
                                    process,
                                    sink,
                                    mode,
                                    &context.handles,
                                    &execution.work_mode,
                                    init_model.as_deref(),
                                    actual,
                                )
                                .await?;
                        } else if expected != Some(actual) {
                            return Err(AppError::ProviderUnavailable(
                                permission_mode::mode_mismatch_reason(
                                    expected,
                                    actual,
                                    mode.auto_source(),
                                ),
                            ));
                        }
                    }
                }
                // Claude changes its mode mid-turn with a `status` frame:
                // a settings layer can drop `auto` after `init` reported it.
                // `plan` comes and goes with EnterPlanMode/ExitPlanMode.
                if message.get("subtype").and_then(Value::as_str) == Some("status") {
                    if let Some(actual) = message.get("permissionMode").and_then(Value::as_str) {
                        let expected = expected_mode.as_deref();
                        if actual != "plan"
                            && permission_mode::bypass_mode_refused(expected, actual)
                        {
                            return Err(bypass_refused(sink, actual, mode.bypass_source()).await);
                        }
                        if auto_mode_fell_back(expected, actual) {
                            auto_fallback
                                .report(
                                    process,
                                    sink,
                                    mode,
                                    &context.handles,
                                    &execution.work_mode,
                                    init_model.as_deref(),
                                    actual,
                                )
                                .await?;
                        }
                    }
                }
                sink.emit(
                    "provider.event",
                    json!({
                        "provider": "claude-code",
                        "providerMethod": message.get("subtype"),
                        "metadata": {
                            "sessionId": message.get("session_id"),
                            "model": message.get("model"),
                            "permissionMode": message.get("permissionMode"),
                            "taskId": message.get("task_id"),
                            "taskType": message.get("task_type"),
                            "taskStatus": message.get("status"),
                            "description": message.get("description"),
                            "toolUseId": message.get("tool_use_id"),
                        }
                    }),
                )
                .await?;
            }
            Some("control_response") => {
                if let Some(sources) = permission_mode::settings_probe_answer(&message) {
                    mode.settings = Some(sources);
                    auto_fallback.probe_answered(sink, mode).await?;
                }
            }
            Some("rate_limit_event") => {
                if let Some(quota) = claude_quota_event(&message) {
                    if let Some(until) = claude_frame_limit_reset(&quota) {
                        rejected_reset = Some(until);
                    }
                    sink.emit("quota.updated", quota).await?;
                }
            }
            Some(event_type) => {
                sink.emit(
                    "provider.event",
                    json!({ "provider": "claude-code", "providerMethod": event_type }),
                )
                .await?;
            }
            None => {}
        }
        // Any frame while a deadline waits for `idle` resets it: only real
        // silence ends a turn without the frame. A prompt waiting on the user
        // is never on a deadline.
        if idle_deadline.is_some() {
            idle_deadline = (!open_requests.outstanding())
                .then(|| Instant::now() + idle_confirm_grace(&background_tasks));
        }
    };
    if session_states {
        work.end_turn();
    }
    // The settings probe went unanswered: report without a source.
    auto_fallback.probe_answered(sink, mode).await?;
    // Work Claude did on its own may end without visible output.
    if !saw_output && !adopt {
        return Err(AppError::ProviderUnavailable(
            "Claude Code completed without producing output; please retry".to_owned(),
        ));
    }
    let native_session_id = result
        .get("session_id")
        .and_then(Value::as_str)
        .unwrap_or(&requested_session_id)
        .to_owned();
    // Only a changed state is written: a resident session usually ends each
    // turn on the id, recoverability and clean error it started with.
    if provider_state.native_session_id.as_deref() != Some(native_session_id.as_str())
        || !provider_state.recoverable
        || provider_state.last_error.is_some()
    {
        provider_state.native_session_id = Some(native_session_id.clone());
        provider_state.recoverable = true;
        provider_state.last_error = None;
        sink.save_provider_state(provider_state).await?;
    }
    Ok(DriverTurnResult {
        native_session_id: Some(native_session_id),
        stop_reason: result
            .get("subtype")
            .and_then(Value::as_str)
            .unwrap_or("completed")
            .to_owned(),
        cancelled: false,
    })
}

/// Normalizes a Claude Code `rate_limit_event` into the shared account-quota
/// payload. `unifiedWindows` carries the 5-hour and 7-day subscription windows
/// (`utilization` is a 0–1 fraction); a bare event still reports its top-level
/// `rateLimitType`/`resetsAt` as a single window without a percentage.
fn claude_quota_event(message: &Value) -> Option<Value> {
    let info = message.get("rate_limit_info")?.as_object()?;
    let mut windows = Vec::new();
    if let Some(unified) = info.get("unifiedWindows").and_then(Value::as_object) {
        let mut ordered: Vec<(&String, &Value)> = unified.iter().collect();
        ordered.sort_by_key(|(id, _)| id.as_str());
        for (id, window) in ordered {
            windows.push(json!({
                "id": id,
                "usedPercent": window
                    .get("utilization")
                    .and_then(Value::as_f64)
                    .map(|fraction| fraction * 100.0),
                "resetsAt": window.get("resetsAt"),
            }));
        }
    }
    if windows.is_empty() {
        if let Some(window_type) = info.get("rateLimitType").and_then(Value::as_str) {
            windows.push(json!({ "id": window_type, "resetsAt": info.get("resetsAt") }));
        }
    }
    Some(json!({
        "provider": "claude-code",
        "scope": "account",
        "windows": windows,
        "status": info.get("status"),
        "isUsingOverage": info.get("isUsingOverage"),
        "raw": info,
    }))
}

fn rejected_quota_payload(until: DateTime<Utc>) -> Value {
    let resets_at = until.timestamp();
    json!({
        "provider": "claude-code",
        "scope": "account",
        "status": "rejected",
        "windows": [{ "id": "session", "resetsAt": resets_at }],
        "raw": { "resetsAt": resets_at },
    })
}

fn claude_frame_is_api_error(message: &Value) -> bool {
    message.get("error").and_then(Value::as_str) == Some("rate_limit")
        || message.get("is_api_error_message").and_then(Value::as_bool) == Some(true)
        || message.get("isApiErrorMessage").and_then(Value::as_bool) == Some(true)
}

/// Reset instant carried on a rejected quota snapshot. Claude Code 2.1 puts
/// this on the assistant frame as `quotaLimits` (the stream-json serializer
/// usually drops it) and on `rate_limit_event` / the normalized quota payload.
fn claude_frame_limit_reset(message: &Value) -> Option<DateTime<Utc>> {
    let nested = message
        .get("quotaLimits")
        .or_else(|| message.get("quota_limits"))
        .or_else(|| message.get("rate_limit_info"));
    let source = nested.unwrap_or(message);
    let status = source.get("status").and_then(Value::as_str);
    // An explicit `allowed` snapshot is not exhaustion. A frame with no
    // status still counts when the assistant message itself is the 429.
    let rejected =
        status == Some("rejected") || (status.is_none() && claude_frame_is_api_error(message));
    if !rejected {
        return None;
    }
    let raw = source
        .get("resetsAt")
        .or_else(|| source.pointer("/raw/resetsAt"))
        .or_else(|| message.pointer("/raw/resetsAt"));
    epoch_to_utc(raw?)
}

fn claude_assistant_text(message: &Value) -> Option<String> {
    let content = message.pointer("/message/content")?;
    if let Some(text) = content.as_str() {
        let text = text.trim();
        return (!text.is_empty()).then(|| text.to_owned());
    }
    let text = content
        .as_array()?
        .iter()
        .filter(|block| block.get("type").and_then(Value::as_str) == Some("text"))
        .filter_map(|block| block.get("text").and_then(Value::as_str))
        .collect::<Vec<_>>()
        .join("\n");
    let text = text.trim();
    (!text.is_empty()).then(|| text.to_owned())
}

fn epoch_to_utc(value: &Value) -> Option<DateTime<Utc>> {
    let raw = value.as_i64()?;
    let seconds = if raw > 100_000_000_000 {
        raw.checked_div(1000)?
    } else {
        raw
    };
    DateTime::from_timestamp(seconds, 0)
}

/// Wall-clock reset in a Claude usage-limit error, for example
/// "You've hit your session limit · resets 5:30pm (Australia/Perth)" or
/// "You've hit your weekly limit · resets Mon 12:00am (UTC)".
/// `None` when the text is not a usage limit or the clock cannot be resolved.
fn usage_limit_reset(text: &str, now: DateTime<Utc>) -> Option<DateTime<Utc>> {
    let lower = text.to_ascii_lowercase();
    if !lower.contains("reset")
        || !(lower.contains("limit") || lower.contains("out of extra usage"))
    {
        return None;
    }
    let start = lower.rfind("reset")?;
    let mut scan = Scan { text, i: start };
    if !scan.eat_ignore("reset") {
        return None;
    }
    scan.eat_ignore("s");
    scan.skip_ws();
    scan.eat_ignore("at");
    scan.skip_ws();
    let weekday = scan.peek_word().and_then(weekday_name).inspect(|_| {
        scan.eat_word();
    });
    let (hour, minute) = scan.eat_clock()?;
    let zone = scan.eat_zone();
    match zone {
        Some(name) => {
            let tz: Tz = name.parse().ok()?;
            next_zoned(&tz, now, hour, minute, weekday)
        }
        None => next_zoned(&chrono::Local, now, hour, minute, weekday),
    }
}

/// How far in the past a parsed reset may lie and still mean "now": the
/// message is only minute-precise and may be read a little after the reset
/// (or with a skewed clock). Older times are the next day's or week's.
const RECENT_RESET_GRACE_MINUTES: i64 = 10;

/// The first instant at `hour:minute` (on `weekday`, if given) in `tz` that
/// is after `now`. A time at most [`RECENT_RESET_GRACE_MINUTES`] ago — the
/// reset just passed — is `now` instead of a day or week later; callers add
/// their own minimum wait.
fn next_zoned<T: TimeZone>(
    tz: &T,
    now: DateTime<Utc>,
    hour: u32,
    minute: u32,
    weekday: Option<Weekday>,
) -> Option<DateTime<Utc>> {
    let grace = chrono::Duration::minutes(RECENT_RESET_GRACE_MINUTES);
    // From yesterday, so a reset just before local midnight read just
    // after it still counts as recent.
    let yesterday = now
        .with_timezone(tz)
        .date_naive()
        .checked_sub_days(Days::new(1))?;
    let span = if weekday.is_some() { 9 } else { 3 };
    for day in 0..span {
        let date = yesterday.checked_add_days(Days::new(day))?;
        if weekday.is_some_and(|weekday| date.weekday() != weekday) {
            continue;
        }
        let naive = date.and_hms_opt(hour, minute, 0)?;
        let utc = match tz.from_local_datetime(&naive) {
            LocalResult::Single(local) => local.with_timezone(&Utc),
            LocalResult::Ambiguous(early, late) => {
                let early = early.with_timezone(&Utc);
                if early > now {
                    early
                } else {
                    late.with_timezone(&Utc)
                }
            }
            LocalResult::None => continue,
        };
        if utc > now {
            return Some(utc);
        }
        if now - utc <= grace {
            return Some(now);
        }
    }
    None
}

fn weekday_name(word: &str) -> Option<Weekday> {
    Some(
        match word.trim_end_matches(',').to_ascii_lowercase().as_str() {
            "mon" | "monday" => Weekday::Mon,
            "tue" | "tues" | "tuesday" => Weekday::Tue,
            "wed" | "wednesday" => Weekday::Wed,
            "thu" | "thur" | "thurs" | "thursday" => Weekday::Thu,
            "fri" | "friday" => Weekday::Fri,
            "sat" | "saturday" => Weekday::Sat,
            "sun" | "sunday" => Weekday::Sun,
            _ => return None,
        },
    )
}

struct Scan<'a> {
    text: &'a str,
    i: usize,
}

impl<'a> Scan<'a> {
    fn rest(&self) -> &'a str {
        &self.text[self.i..]
    }

    fn skip_ws(&mut self) {
        let rest = self.rest();
        self.i += rest.len() - rest.trim_start().len();
    }

    fn eat_ignore(&mut self, lit: &str) -> bool {
        let rest = self.rest();
        // Bytes, not a str slice: the text after `lit.len()` may split a
        // multi-byte character, and an ASCII match ends on a boundary.
        if rest
            .as_bytes()
            .get(..lit.len())
            .is_some_and(|head| head.eq_ignore_ascii_case(lit.as_bytes()))
        {
            self.i += lit.len();
            true
        } else {
            false
        }
    }

    fn peek_word(&mut self) -> Option<&'a str> {
        self.skip_ws();
        let rest = self.rest();
        let end = rest
            .find(|c: char| !c.is_ascii_alphabetic())
            .unwrap_or(rest.len());
        (end > 0).then(|| &rest[..end])
    }

    fn eat_word(&mut self) -> Option<&'a str> {
        let word = self.peek_word()?;
        self.i += word.len();
        Some(word)
    }

    fn eat_number(&mut self) -> Option<u32> {
        let rest = self.rest();
        let digits = rest.chars().take_while(|c| c.is_ascii_digit()).count();
        if digits == 0 || digits > 2 {
            return None;
        }
        let value = rest[..digits].parse().ok()?;
        self.i += digits;
        Some(value)
    }

    fn eat_clock(&mut self) -> Option<(u32, u32)> {
        self.skip_ws();
        let hour12 = self.eat_number()?;
        let minute = if self.eat_ignore(":") {
            self.eat_number()?
        } else {
            0
        };
        self.skip_ws();
        let pm = if self.eat_ignore("p.m") {
            while self.eat_ignore(".") {}
            true
        } else if self.eat_ignore("a.m") {
            while self.eat_ignore(".") {}
            false
        } else if self.eat_ignore("pm") {
            true
        } else if self.eat_ignore("am") {
            false
        } else {
            return None;
        };
        if !(1..=12).contains(&hour12) || minute > 59 {
            return None;
        }
        let hour = match (hour12, pm) {
            (12, false) => 0,
            (12, true) => 12,
            (hour, true) => hour + 12,
            (hour, false) => hour,
        };
        Some((hour, minute))
    }

    fn eat_zone(&mut self) -> Option<&'a str> {
        self.skip_ws();
        let rest = self.rest();
        if !rest.starts_with('(') {
            return None;
        }
        let end = rest.find(')')?;
        let name = rest[1..end].trim();
        if name.is_empty() {
            return None;
        }
        self.i += end + 1;
        Some(name)
    }
}

fn content_blocks<'a>(message: &'a Value, kind: &'a str) -> impl Iterator<Item = &'a Value> {
    message
        .pointer("/message/content")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter(move |block| block.get("type").and_then(Value::as_str) == Some(kind))
}

/// Resend attempts allowed when a `result` reports zero model turns. Several
/// queued notifications can each consume an invocation, so the cap is above
/// one but still bounded.
const MAX_EMPTY_RESULT_RESENDS: u32 = 3;

/// How long a settled `result` waits for Claude's `idle`, with no background
/// task outstanding.
///
/// `idle` is the authoritative end of a turn, but Claude only reports it once
/// it stops counting work as live, and that bookkeeping can disagree with
/// reality: a subagent restart, a notification it already answered, or a
/// parked prompt leaves it `running` for the rest of the process's life. The
/// turn then stays open until the user cancels it, which is exactly what a
/// client shows as "working". The Agent SDK driving the same stream guards
/// itself the same way (`runEndCeilingMs`: "No 'idle' N ms after the last
/// result, ending the run"), so ending on the answer Claude already gave is
/// the vendor's own fallback. The process stays alive: whatever Claude runs
/// afterwards reaches the supervisor as a provider-initiated turn.
const IDLE_CONFIRM_GRACE: Duration = if cfg!(test) {
    Duration::from_secs(1)
} else {
    Duration::from_secs(30)
};

/// The same wait while Claude still owes a follow-up invocation for
/// background work: a command may run for minutes before it reports back, and
/// the vendor's own ceiling for that case is ten minutes.
const IDLE_CONFIRM_GRACE_PENDING: Duration = if cfg!(test) {
    Duration::from_secs(2)
} else {
    Duration::from_secs(600)
};

/// How long this turn waits for `idle` once a `result` settles it.
fn idle_confirm_grace(background_tasks: &BackgroundTasks) -> Duration {
    if background_tasks.pending() {
        IDLE_CONFIRM_GRACE_PENDING
    } else {
        IDLE_CONFIRM_GRACE
    }
}

/// Resolves when `deadline` passes; never when there is none, so the select
/// arm costs nothing while a turn waits for frames. A frame resets it: only
/// real silence ends a turn without `idle`.
async fn confirmation_due(deadline: Option<Instant>) {
    match deadline {
        Some(deadline) => tokio::time::sleep_until(deadline).await,
        None => std::future::pending().await,
    }
}

/// Background work that keeps a Claude turn open.
///
/// `live` mirrors the Agent SDK: `task_started` adds (unless explicitly
/// foreground), `task_notification` removes, and `background_tasks_changed`
/// replaces the whole set — its `tasks` payload is "every live background
/// task after the change".
///
/// An empty set is not enough to end the turn. Claude emits
/// `task_notification` as soon as a task finishes, even mid-invocation, and
/// hands the notification to the model either later in the same invocation
/// (replayed as a user message with `origin.kind == "task-notification"`) or
/// in a follow-up invocation that starts (with `system/init`) after the
/// current `result`. `owed` holds notified tasks not handed over yet; ending
/// on the earlier `result` would kill the process with the hand-back still
/// queued, while waiting for a hand-back that already happened never ends.
#[derive(Default)]
struct BackgroundTasks {
    /// Live background tasks by task_id, with what clients show while the
    /// turn only waits on them.
    live: HashMap<String, BackgroundTaskInfo>,
    /// Known task metadata seen this turn (foreground or background):
    /// task_id -> info.
    known: HashMap<String, BackgroundTaskInfo>,
    /// Main-session background tasks seen this turn. Tasks a subagent owns
    /// notify that subagent, not the main session, so they owe no follow-up.
    main: HashSet<String>,
    /// Notified main-session tasks (task_id → run_id) whose notification the
    /// model has not received yet.
    owed: HashMap<String, Option<String>>,
}

/// Display details from the frame that announced a background task.
#[derive(Clone, Debug, Default)]
struct BackgroundTaskInfo {
    description: Option<String>,
    task_type: Option<String>,
    tool_use_id: Option<String>,
    started_at: Option<String>,
}

impl BackgroundTaskInfo {
    fn from_frame(frame: &Value, started_at: Option<String>) -> Self {
        let text = |key: &str| {
            frame
                .get(key)
                .and_then(Value::as_str)
                .filter(|value| !value.is_empty())
                .map(str::to_owned)
        };
        Self {
            description: text("description"),
            task_type: text("task_type"),
            tool_use_id: text("tool_use_id"),
            started_at,
        }
    }

    /// Fills fields the earlier frame lacked; a later description wins since
    /// `task_progress` refreshes it.
    fn merge(&mut self, newer: Self) {
        if newer.description.is_some() {
            self.description = newer.description;
        }
        self.task_type = self.task_type.take().or(newer.task_type);
        self.tool_use_id = self.tool_use_id.take().or(newer.tool_use_id);
        self.started_at = self.started_at.take().or(newer.started_at);
    }
}

impl BackgroundTasks {
    fn apply(&mut self, message: &Value) {
        match message.get("subtype").and_then(Value::as_str) {
            // Each invocation starts with `init`, after it has taken the
            // queued notifications.
            Some("init") => self.owed.clear(),
            Some("task_started") => {
                let Some(id) = message.get("task_id").and_then(Value::as_str) else {
                    return;
                };
                let is_subagent = message.get("owned_by_subagent").and_then(Value::as_bool)
                    == Some(true)
                    || message
                        .get("parent_task_id")
                        .and_then(Value::as_str)
                        .is_some();
                let info = BackgroundTaskInfo::from_frame(message, Some(Utc::now().to_rfc3339()));
                self.known
                    .entry(id.to_owned())
                    .or_default()
                    .merge(info.clone());
                if !is_subagent {
                    self.main.insert(id.to_owned());
                    if message.get("is_backgrounded").and_then(Value::as_bool) != Some(false) {
                        self.live.entry(id.to_owned()).or_default().merge(info);
                    }
                }
            }
            Some("task_updated") => {
                let Some(id) = message.get("task_id").and_then(Value::as_str) else {
                    return;
                };
                if let Some(bg) = message
                    .pointer("/patch/is_backgrounded")
                    .and_then(Value::as_bool)
                {
                    if bg && self.main.contains(id) {
                        let mut info = self.known.get(id).cloned().unwrap_or_default();
                        info.merge(BackgroundTaskInfo::from_frame(message, None));
                        self.live.insert(id.to_owned(), info);
                    } else if !bg {
                        self.live.remove(id);
                    }
                }
            }
            Some("task_notification") => {
                if let Some(id) = message.get("task_id").and_then(Value::as_str) {
                    self.live.remove(id);
                    if self.main.contains(id) {
                        let run_id = message.get("run_id").and_then(Value::as_str);
                        self.owed.insert(id.to_owned(), run_id.map(str::to_owned));
                    }
                }
            }
            Some("task_progress") => {
                if let Some(id) = message.get("task_id").and_then(Value::as_str) {
                    let update = BackgroundTaskInfo::from_frame(message, None);
                    if let Some(info) = self.known.get_mut(id) {
                        info.merge(update.clone());
                    }
                    if let Some(info) = self.live.get_mut(id) {
                        info.merge(update);
                    }
                }
            }
            Some("background_tasks_changed") => {
                let mut previous = std::mem::take(&mut self.live);
                if let Some(tasks) = message.get("tasks").and_then(Value::as_array) {
                    for task in tasks {
                        let Some(id) = task.get("task_id").and_then(Value::as_str) else {
                            continue;
                        };
                        let is_subagent = task.get("owned_by_subagent").and_then(Value::as_bool)
                            == Some(true)
                            || task.get("parent_task_id").and_then(Value::as_str).is_some()
                            || !self.main.contains(id) && self.known.contains_key(id);
                        if is_subagent {
                            continue;
                        }
                        self.main.insert(id.to_owned());
                        let mut info = previous
                            .remove(id)
                            .or_else(|| self.known.get(id).cloned())
                            .unwrap_or_default();
                        info.merge(BackgroundTaskInfo::from_frame(task, None));
                        self.live.insert(id.to_owned(), info);
                    }
                }
            }
            _ => {}
        }
    }

    /// A replayed user message delivering task notifications to the model in
    /// the current invocation: those tasks owe no follow-up invocation.
    fn delivered(&mut self, message: &Value) {
        let origin = message.get("origin");
        if origin
            .and_then(|origin| origin.get("kind"))
            .and_then(Value::as_str)
            != Some("task-notification")
        {
            return;
        }
        if let Some(run_id) = origin
            .and_then(|origin| origin.get("runId"))
            .and_then(Value::as_str)
        {
            self.owed
                .retain(|_, owed_run| owed_run.as_deref() != Some(run_id));
        }
        // The content names each delivered task as `<task-id>…</task-id>`.
        let text = match message.pointer("/message/content") {
            Some(Value::String(text)) => text.clone(),
            Some(Value::Array(blocks)) => blocks
                .iter()
                .filter_map(|block| block.get("text").and_then(Value::as_str))
                .collect::<Vec<_>>()
                .join("\n"),
            _ => String::new(),
        };
        for part in text.split("<task-id>").skip(1) {
            if let Some((id, _)) = part.split_once("</task-id>") {
                self.owed.remove(id.trim());
            }
        }
    }

    /// Whether Claude will still produce another `result` in this turn.
    fn pending(&self) -> bool {
        !self.owed.is_empty() || !self.live.is_empty()
    }

    fn ids(&self) -> Vec<Value> {
        self.tasks()
            .into_iter()
            .filter_map(|task| task.get("taskId").cloned())
            .collect()
    }

    /// Live tasks, oldest first, as `{ taskId, description?, taskType?,
    /// toolUseId?, startedAt? }`.
    fn tasks(&self) -> Vec<Value> {
        let mut tasks = self.live.iter().collect::<Vec<_>>();
        // RFC 3339 UTC strings from one clock sort chronologically; unknown
        // start times go last.
        tasks.sort_by(|(a_id, a), (b_id, b)| {
            (a.started_at.is_none(), &a.started_at, a_id).cmp(&(
                b.started_at.is_none(),
                &b.started_at,
                b_id,
            ))
        });
        tasks
            .into_iter()
            .map(|(id, info)| {
                let mut task = Map::new();
                task.insert("taskId".to_owned(), json!(id));
                for (key, value) in [
                    ("description", &info.description),
                    ("taskType", &info.task_type),
                    ("toolUseId", &info.tool_use_id),
                    ("startedAt", &info.started_at),
                ] {
                    if let Some(value) = value {
                        task.insert(key.to_owned(), json!(value));
                    }
                }
                Value::Object(task)
            })
            .collect()
    }
}

/// Subagent runs keyed by the surfaced `subagentId`: the spawning Task/Agent
/// tool_use id when the run came from a tool call, otherwise the provider
/// task_id. `by_task` links background task frames (keyed by task_id) back to
/// that surfaced id so progress and terminal notifications update one entry.
/// Kept for the whole process: a background agent reports in later turns and
/// between turns.
#[derive(Default)]
struct ClaudeSubagents {
    /// subagentId → tool_use_id of the spawning Task/Agent call.
    items: HashMap<String, String>,
    /// task_id → subagentId for every task classified as a subagent.
    by_task: HashMap<String, String>,
    /// subagentIds that already emitted a terminal event.
    finished: HashSet<String>,
    /// subagentId → the turn that started it, which its events name when
    /// no turn runs.
    turns: HashMap<String, String>,
}

impl ClaudeSubagents {
    fn is_agent_tool(name: Option<&Value>) -> bool {
        matches!(name.and_then(Value::as_str), Some("Task" | "Agent"))
    }

    /// Background shells and monitors ride the same task frames; only agent
    /// kinds belong on the subagent surface.
    fn is_agent_task_kind(kind: Option<&Value>) -> bool {
        kind.and_then(Value::as_str)
            .is_some_and(|kind| kind.contains("agent"))
    }

    /// A Task/Agent `tool_use` opens a subagent run even when no task frame
    /// ever arrives — foreground agents only report through their tool result.
    fn start_from_tool(&mut self, block: &Value, turn_id: &str) -> Option<Value> {
        let tool_id = block.get("id").and_then(Value::as_str)?.to_owned();
        if self.items.contains_key(&tool_id) {
            return None;
        }
        self.items.insert(tool_id.clone(), tool_id.clone());
        self.turns.insert(tool_id.clone(), turn_id.to_owned());
        let input = block.get("input").cloned().unwrap_or(Value::Null);
        Some(json!({
            "provider": "claude-code",
            "source": "provider",
            "turnId": turn_id,
            "subagentId": tool_id,
            "providerItemId": tool_id,
            "agentKind": input.get("subagent_type"),
            "title": input.get("description").or_else(|| input.get("subagent_type")),
            "task": input.get("prompt").or_else(|| input.get("description")),
            "status": "running",
        }))
    }

    /// The tool_result of a Task/Agent call ends the run for foreground
    /// agents. Background launches report `async_launched` and finish through
    /// their task_notification instead.
    fn finish_from_tool(
        &mut self,
        tool_id: &str,
        message: &Value,
        result: &Value,
        is_error: bool,
        turn_id: &str,
    ) -> Option<(&'static str, Value)> {
        if !self.items.contains_key(tool_id) || self.finished.contains(tool_id) {
            return None;
        }
        let tool_use_result = message
            .get("tool_use_result")
            .cloned()
            .unwrap_or(Value::Null);
        let async_launch = tool_use_result
            .get("isAsync")
            .and_then(Value::as_bool)
            .unwrap_or(false)
            || tool_use_result
                .get("status")
                .and_then(Value::as_str)
                .is_some_and(|status| status == "async_launched");
        if async_launch {
            return Some((
                "subagent.updated",
                json!({
                    "provider": "claude-code",
                    "source": "provider",
                    "turnId": turn_id,
                    "subagentId": tool_id,
                    "providerItemId": tool_id,
                    "agentId": tool_use_result.get("agentId"),
                    "status": "running",
                    "metadata": { "outputFile": tool_use_result.get("outputFile") },
                }),
            ));
        }
        self.finished.insert(tool_id.to_owned());
        Some((
            if is_error {
                "subagent.failed"
            } else {
                "subagent.completed"
            },
            json!({
                "provider": "claude-code",
                "source": "provider",
                "turnId": turn_id,
                "subagentId": tool_id,
                "providerItemId": tool_id,
                "status": if is_error { "failed" } else { "completed" },
                "result": result,
                "error": is_error.then(|| result.clone()),
            }),
        ))
    }

    /// `task_started`, `task_progress` and `task_notification` frames carry the
    /// subagent lifecycle for provider-side tasks. `turn_id` is the running
    /// turn; a run started earlier keeps naming its own turn.
    fn system_event(&mut self, message: &Value, turn_id: &str) -> Option<(&'static str, Value)> {
        let task_id = message.get("task_id").and_then(Value::as_str)?;
        let turn_id = self
            .by_task
            .get(task_id)
            .and_then(|subagent_id| self.turns.get(subagent_id))
            .map_or(turn_id, String::as_str)
            .to_owned();
        let turn_id = turn_id.as_str();
        match message.get("subtype").and_then(Value::as_str) {
            Some("task_started") => {
                let tool_use_id = message.get("tool_use_id").and_then(Value::as_str);
                let kind = message.get("task_type");
                // A linked Task call already opened the run from its tool_use;
                // record the task_id link so later task frames find it.
                if let Some(subagent_id) = tool_use_id.and_then(|id| self.items.get(id)).cloned() {
                    self.by_task.insert(task_id.to_owned(), subagent_id.clone());
                    // Claude restarts a finished agent when a background
                    // command it left behind completes: the run is live again.
                    if !self.finished.remove(&subagent_id) {
                        return None;
                    }
                    let turn_id = self.turns.get(&subagent_id).map_or(turn_id, String::as_str);
                    return Some((
                        "subagent.started",
                        json!({
                            "provider": "claude-code",
                            "source": "provider",
                            "turnId": turn_id,
                            "subagentId": subagent_id,
                            "providerItemId": self.items.get(&subagent_id),
                            "agentId": message.get("agent_id"),
                            "status": "running",
                        }),
                    ));
                }
                if !Self::is_agent_task_kind(kind) {
                    return None;
                }
                self.by_task.insert(task_id.to_owned(), task_id.to_owned());
                self.turns.insert(task_id.to_owned(), turn_id.to_owned());
                // A restart reports through its own task_notification again.
                self.finished.remove(task_id);
                if let Some(tool_use_id) = tool_use_id {
                    self.items
                        .insert(task_id.to_owned(), tool_use_id.to_owned());
                }
                Some((
                    "subagent.started",
                    json!({
                        "provider": "claude-code",
                        "source": "provider",
                        "turnId": turn_id,
                        "subagentId": task_id,
                        "providerItemId": tool_use_id,
                        "agentKind": kind,
                        "agentId": message.get("agent_id"),
                        "title": message
                            .get("description")
                            .or(kind)
                            .filter(|title| title.is_string()),
                        "task": message.get("description"),
                        "status": "running",
                    }),
                ))
            }
            Some("task_progress") => {
                let subagent_id = self.by_task.get(task_id)?;
                if self.finished.contains(subagent_id) {
                    return None;
                }
                Some((
                    "subagent.updated",
                    json!({
                        "provider": "claude-code",
                        "source": "provider",
                        "turnId": turn_id,
                        "subagentId": subagent_id,
                        "status": message.get("status"),
                        "metadata": {
                            "taskId": task_id,
                            "description": message.get("description"),
                            "toolName": message.get("tool_name"),
                            "usage": message.get("usage"),
                        },
                    }),
                ))
            }
            Some("task_notification") => {
                let subagent_id = self.by_task.get(task_id)?.clone();
                if self.finished.contains(&subagent_id) {
                    return None;
                }
                self.finished.insert(subagent_id.clone());
                let status = message
                    .get("status")
                    .and_then(Value::as_str)
                    .unwrap_or("completed");
                let event = match status {
                    "completed" => "subagent.completed",
                    "failed" => "subagent.failed",
                    _ => "subagent.cancelled",
                };
                Some((
                    event,
                    json!({
                        "provider": "claude-code",
                        "source": "provider",
                        "turnId": turn_id,
                        "subagentId": subagent_id,
                        "providerItemId": self.items.get(&subagent_id),
                        "agentId": message.get("agent_id"),
                        "status": status,
                        "result": message.get("summary"),
                        "error": (status != "completed")
                            .then(|| message.get("summary").cloned())
                            .flatten(),
                        "metadata": {
                            "taskId": task_id,
                            "outputFile": message.get("output_file"),
                            "stopCause": message.get("stop_cause"),
                            "snapshot": message.get("snapshot"),
                        },
                    }),
                ))
            }
            _ => None,
        }
    }
}

#[derive(Default)]
struct ClaudeToolCall {
    name: Value,
    input: Value,
    result: Option<Value>,
    is_error: Option<bool>,
    /// `parent_tool_use_id` of the frames the call was reported in; a
    /// subagent's inner tools fold under its run instead of interrupting the
    /// main assistant stream.
    subagent_id: Option<String>,
}

/// Claude reports one tool call across several messages: the streamed
/// `tool_use` start (empty input), the complete assistant message, progress
/// ticks and the replayed `tool_result`. Clients replace a tool card with the
/// latest snapshot, so every event carries everything known about the call.
#[derive(Default)]
struct ClaudeToolCalls(HashMap<String, ClaudeToolCall>);

impl ClaudeToolCalls {
    /// Record a `tool_use` block, keeping earlier non-empty input when a
    /// later block (the stream start) has none.
    fn record(&mut self, block: &Value, subagent: Option<&str>) -> Option<String> {
        let id = block.get("id").and_then(Value::as_str)?.to_owned();
        let call = self.0.entry(id.clone()).or_default();
        if let Some(subagent) = subagent {
            call.subagent_id = Some(subagent.to_owned());
        }
        if let Some(name) = block.get("name").filter(|name| name.is_string()) {
            call.name = name.clone();
        }
        if let Some(input) = block
            .get("input")
            .filter(|input| input.as_object().is_some_and(|input| !input.is_empty()))
        {
            call.input = input.clone();
        }
        Some(id)
    }

    fn name_if_unknown(&mut self, id: &str, name: Option<&Value>, subagent: Option<&str>) {
        let call = self.0.entry(id.to_owned()).or_default();
        if let Some(subagent) = subagent {
            call.subagent_id = Some(subagent.to_owned());
        }
        if call.name.is_null() {
            if let Some(name) = name.filter(|name| name.is_string()) {
                call.name = name.clone();
            }
        }
    }

    fn complete(&mut self, block: &Value, subagent: Option<&str>) -> Option<String> {
        let id = block.get("tool_use_id").and_then(Value::as_str)?.to_owned();
        let call = self.0.entry(id.clone()).or_default();
        if let Some(subagent) = subagent {
            call.subagent_id = Some(subagent.to_owned());
        }
        call.result = Some(tool_result_text(block.get("content")));
        call.is_error = Some(
            block
                .get("is_error")
                .and_then(Value::as_bool)
                .unwrap_or(false),
        );
        Some(id)
    }

    fn payload(&self, id: &str, turn_id: &str, phase: &str) -> Value {
        let call = self.0.get(id);
        json!({
            "provider": "claude-code",
            "toolCallId": id,
            "toolName": call.map_or(&Value::Null, |call| &call.name),
            "arguments": call.map_or(&Value::Null, |call| &call.input),
            "result": call.and_then(|call| call.result.as_ref()),
            "isError": call.and_then(|call| call.is_error),
            "subagentId": call.and_then(|call| call.subagent_id.as_ref()),
            "block": {
                "category": "tool",
                "id": id,
                "turnId": turn_id,
                "phase": phase,
            },
        })
    }
}

/// `tool_result` content is a string or a list of content blocks; text blocks
/// are joined, anything else (images) is kept as-is.
fn tool_result_text(content: Option<&Value>) -> Value {
    match content {
        Some(Value::Array(blocks)) => {
            let text = blocks
                .iter()
                .filter_map(|block| block.get("text").and_then(Value::as_str))
                .collect::<Vec<_>>()
                .join("\n");
            if text.is_empty() {
                Value::Array(blocks.clone())
            } else {
                Value::String(text)
            }
        }
        Some(value) => value.clone(),
        None => Value::Null,
    }
}

async fn handle_stream_event(
    message: &Value,
    turn_id: &str,
    tools: &mut ClaudeToolCalls,
    sink: &DriverEventSink,
) -> Result<(), AppError> {
    let event = message.get("event").cloned().unwrap_or(Value::Null);
    let event_type = event.get("type").and_then(Value::as_str).unwrap_or("");
    match event_type {
        "content_block_delta" => {
            let delta = event.get("delta").cloned().unwrap_or(Value::Null);
            let delta_type = delta.get("type").and_then(Value::as_str);
            let event_type = if delta_type == Some("thinking_delta") {
                "thought.delta"
            } else {
                "message.delta"
            };
            let subagent = message.get("parent_tool_use_id").and_then(Value::as_str);
            let payload = json!({ "provider": "claude-code", "role": "assistant", "delta": delta, "subagentId": subagent });
            // Text, thinking and tool-argument JSON merge per content block
            // (concatenated `partial_json` is still the same JSON prefix);
            // signatures stay one event per fragment.
            let text: Option<&'static [&'static str]> = match delta_type {
                Some("text_delta") => Some(&["/delta/text"]),
                Some("thinking_delta") => Some(&["/delta/thinking"]),
                Some("input_json_delta") => Some(&["/delta/partial_json"]),
                _ => None,
            };
            match text {
                Some(text) => {
                    // The payload omits the content-block index; keep blocks
                    // (and subagent streams) apart anyway. The rest of the
                    // payload is the delta type and the subagent id.
                    let key = serde_json::to_string(&(
                        delta_type,
                        event.get("index"),
                        message.get("parent_tool_use_id"),
                    ))
                    .unwrap_or_default();
                    sink.emit_delta_keyed(event_type, payload, key, text)
                        .await?;
                }
                None => {
                    sink.emit(event_type, payload).await?;
                }
            }
        }
        "content_block_start" => {
            let content = event.get("content_block").cloned().unwrap_or(Value::Null);
            if content.get("type").and_then(Value::as_str) == Some("tool_use") {
                let subagent = message.get("parent_tool_use_id").and_then(Value::as_str);
                if let Some(id) = tools.record(&content, subagent) {
                    sink.emit("tool.started", tools.payload(&id, turn_id, "started"))
                        .await?;
                }
            }
        }
        "message_stop" | "message_start" | "content_block_stop" => {}
        _ => {
            sink.emit(
                "provider.event",
                json!({ "provider": "claude-code", "providerMethod": event_type }),
            )
            .await?;
        }
    }
    Ok(())
}

/// Claude Code runs another mode in place of the requested `auto` and the
/// turn continues: `default` when the model (for example Haiku through a
/// non-first-party API) or the account does not offer auto mode, or the
/// settings' `defaultMode` when `permissions.disableAutoMode` turns it off.
/// See [`permission_mode::auto_fallback_mode`] for the modes that qualify.
fn auto_mode_fell_back(expected: Option<&str>, actual: &str) -> bool {
    expected == Some("auto") && permission_mode::auto_fallback_mode(actual).is_some()
}

/// The `provider.event` recorded when [`auto_mode_fell_back`]; same shape as
/// Antigravity's `auto_mode_unavailable`, plus the `cause` and the
/// `settingsSource` that disabled auto mode (`null` when none did or it is
/// unknown).
fn auto_mode_unavailable_event(model: Option<&str>, active: &str, source: Option<&str>) -> Value {
    let reason = match source {
        Some(source) => format!(
            "Claude Code settings ({source}) disable auto mode (permissions.disableAutoMode)"
        ),
        None => "Claude Code did not enable auto mode for this model or account".to_owned(),
    };
    json!({
        "provider": "claude-code",
        "providerMethod": "auto_mode_unavailable",
        "metadata": {
            "requestedMode": "auto",
            "activeMode": active,
            "model": model,
            "reason": reason,
            "cause": if source.is_some() { "settings" } else { "unavailable" },
            "settingsSource": source,
        },
    })
}

/// Records `bypass_mode_disabled` and returns the error that fails the turn.
async fn bypass_refused(sink: &DriverEventSink, actual: &str, source: Option<&str>) -> AppError {
    if let Err(error) = sink
        .emit(
            "provider.event",
            permission_mode::bypass_mode_disabled_event(actual, source),
        )
        .await
    {
        return error;
    }
    AppError::ProviderUnavailable(permission_mode::bypass_refused_reason(source, actual))
}

/// Whether a turn that asked for `auto` reported Claude running another mode.
enum AutoFallback {
    Active,
    /// Waiting on the `get_settings` probe before reporting.
    Probing {
        model: Option<String>,
        active: &'static str,
    },
    Reported,
}

impl AutoFallback {
    /// Claude runs `actual` in place of `auto`: TodeX's own tools ask from
    /// now on, and the turn reports it once. Without a settings layer known
    /// yet, a `get_settings` probe goes out first and the report waits for
    /// its answer ([`Self::probe_answered`]).
    #[allow(clippy::too_many_arguments)]
    async fn report(
        &mut self,
        process: &mut JsonLineProcess,
        sink: &DriverEventSink,
        mode: &permission_mode::ModeReport,
        handles: &super::types::TurnHandles,
        work_mode: &str,
        model: Option<&str>,
        actual: &str,
    ) -> Result<(), AppError> {
        if !matches!(self, Self::Active) {
            return Ok(());
        }
        let active = permission_mode::auto_fallback_mode(actual).unwrap_or("ask");
        if let Some(handle) = &handles.mode {
            handle.downgrade_to_ask(work_mode);
        }
        if mode.can_probe() {
            process.send(&permission_mode::settings_probe()).await?;
            *self = Self::Probing {
                model: model.map(str::to_owned),
                active,
            };
            return Ok(());
        }
        *self = Self::Reported;
        sink.emit(
            "provider.event",
            auto_mode_unavailable_event(model, active, mode.auto_source()),
        )
        .await?;
        Ok(())
    }

    /// Reports a fallback that waited on the settings probe.
    async fn probe_answered(
        &mut self,
        sink: &DriverEventSink,
        mode: &permission_mode::ModeReport,
    ) -> Result<(), AppError> {
        if !matches!(self, Self::Probing { .. }) {
            return Ok(());
        }
        let Self::Probing { model, active } = std::mem::replace(self, Self::Reported) else {
            return Ok(());
        };
        sink.emit(
            "provider.event",
            auto_mode_unavailable_event(model.as_deref(), active, mode.auto_source()),
        )
        .await?;
        Ok(())
    }
}

/// The control requests a turn is answering, by `request_id`. Claude sends
/// `control_cancel_request` for one it stopped waiting on; withdrawing it
/// closes its prompt like a cancelled turn does and drops the answer.
#[derive(Clone, Default)]
struct OpenRequests(Arc<std::sync::Mutex<HashMap<String, Arc<watch::Sender<bool>>>>>);

impl OpenRequests {
    fn open(&self, request_id: &str) -> watch::Receiver<bool> {
        let (sender, receiver) = watch::channel(false);
        self.0
            .lock()
            .expect("Claude open requests lock")
            .insert(request_id.to_owned(), Arc::new(sender));
        receiver
    }

    fn withdraw(&self, request_id: &str) {
        let sender = self
            .0
            .lock()
            .expect("Claude open requests lock")
            .remove(request_id);
        if let Some(sender) = sender {
            sender.send_replace(true);
        }
    }

    /// Whether a prompt still waits on the user; such a wait has no deadline.
    fn outstanding(&self) -> bool {
        !self.0.lock().expect("Claude open requests lock").is_empty()
    }

    /// Done with `request_id`; false when Claude withdrew it, so the answer
    /// must not be sent.
    fn close(&self, request_id: &str) -> bool {
        self.0
            .lock()
            .expect("Claude open requests lock")
            .remove(request_id)
            .is_some()
    }
}

/// Answers a control request from a detached task so the read loop keeps
/// draining stdout while `can_use_tool` waits on the user. Claude matches the
/// `control_response` by `request_id`, so reply order does not matter. If the
/// turn ends first, the closed cancel channel resolves the prompt as
/// cancelled; the closed response channel then drops the answer, which is
/// safe because the turn's process is already being torn down. A request
/// Claude withdraws ([`OpenRequests`]) is resolved the same way and never
/// answered.
fn spawn_control_request(
    responses: &mpsc::UnboundedSender<Value>,
    message: Value,
    sink: &DriverEventSink,
    cancel: &watch::Receiver<bool>,
    open_requests: &OpenRequests,
    cwd: &Path,
) {
    let request_id = message
        .get("request_id")
        .or_else(|| message.pointer("/request/request_id"))
        .and_then(Value::as_str)
        .map(str::to_owned);
    let request = message.get("request").cloned().unwrap_or(Value::Null);
    let responses = responses.clone();
    let sink = sink.clone();
    let mut turn_cancel = cancel.clone();
    let open_requests = open_requests.clone();
    let cwd = cwd.to_path_buf();
    // Opened before the read loop goes on, so a withdrawal right behind the
    // request finds it.
    let opened = request_id
        .as_deref()
        .map(|request_id| open_requests.open(request_id));
    tokio::spawn(async move {
        let (Some(request_id), Some(mut cancel)) = (request_id, opened) else {
            // Without an id the request cannot be answered at all; keep the
            // turn alive and log instead of failing it over a malformed frame.
            tracing::warn!("Claude control request is missing request_id");
            return;
        };
        let outcome = async {
            match request.get("subtype").and_then(Value::as_str) {
                Some("can_use_tool") => {
                    tool_permission_response(&request_id, &request, &sink, &mut cancel, &cwd).await
                }
                _ => Err(AppError::Unsupported(
                    "control request is not supported by TodeX".to_owned(),
                )),
            }
        };
        tokio::pin!(outcome);
        // The turn ending (its cancel channel changing or closing) cancels
        // this request too; after that only the prompt is awaited, and its
        // answer goes to a closed channel.
        let (outcome, turn_ended) = tokio::select! {
            outcome = &mut outcome => (outcome, false),
            _ = turn_cancel.changed() => {
                open_requests.withdraw(&request_id);
                (outcome.await, true)
            }
        };
        if !open_requests.close(&request_id) && !turn_ended {
            tracing::debug!(request_id, "Claude withdrew the control request");
            return;
        }
        let response = match outcome {
            Ok(response) => json!({
                "subtype": "success",
                "request_id": request_id,
                "response": response,
            }),
            // A failed prompt still answers Claude with `subtype: "error"` or
            // it would wait on the response forever.
            Err(error) => json!({
                "subtype": "error",
                "request_id": request_id,
                "error": error.to_string(),
            }),
        };
        if responses
            .send(json!({ "type": "control_response", "response": response }))
            .is_err()
        {
            tracing::debug!(
                request_id,
                "turn ended before the control response was sent"
            );
        }
    });
}

/// Waits for the user's decision and builds the `can_use_tool` response body.
/// Errors mean the prompt itself failed (cancelled, expired, or journal
/// write), not that the user denied the tool. `cwd` is Claude's working
/// directory, which relative file paths in the request refer to.
async fn tool_permission_response(
    request_id: &str,
    request: &Value,
    sink: &DriverEventSink,
    cancel: &mut watch::Receiver<bool>,
    cwd: &Path,
) -> Result<Value, AppError> {
    let tool_name = request
        .get("tool_name")
        .and_then(Value::as_str)
        .unwrap_or("operation");
    if tool_name == ASK_USER_QUESTION_TOOL {
        if let Some(details) = request.get("input").and_then(claude_question_details) {
            let decision = sink
                .request_permission(
                    request_id.to_owned(),
                    "user_input",
                    "Claude needs input",
                    details,
                    json!([
                        { "id": "answer", "kind": "answer", "name": "Answer" },
                        { "id": "reject_once", "kind": "reject_once", "name": "Skip" }
                    ]),
                    cancel,
                )
                .await?;
            let input = request.get("input").cloned().unwrap_or_else(|| json!({}));
            return Ok(claude_question_response(&input, &decision));
        }
    }

    let rule = session_rules::session_rule(request, cwd);
    let decision = sink
        .request_permission(
            request_id.to_owned(),
            "tool",
            format!("Allow Claude tool {tool_name}?"),
            claude_permission_details(request),
            claude_tool_options(rule.is_some()),
            cancel,
        )
        .await?;
    Ok(claude_tool_response(request, rule, &decision))
}

/// The options of a Claude tool approval; "Allow for session" only when a
/// session rule can cover a repeat of the request.
fn claude_tool_options(session_rule: bool) -> Value {
    let mut options =
        vec![json!({ "id": "allow_once", "kind": "allow_once", "name": "Allow once" })];
    if session_rule {
        options.push(
            json!({ "id": "allow_always", "kind": "allow_always", "name": "Allow for session" }),
        );
    }
    options.push(json!({ "id": "reject_once", "kind": "reject_once", "name": "Reject" }));
    Value::Array(options)
}

/// The `can_use_tool` response for `decision`. Allowing for the session adds
/// `rule` (see [`session_rules`]) to the running process only.
fn claude_tool_response(
    request: &Value,
    rule: Option<Value>,
    decision: &PermissionDecision,
) -> Value {
    let input = request.get("input").cloned().unwrap_or_else(|| json!({}));
    match (decision.outcome, rule) {
        (PermissionOutcome::AllowAlways, Some(rule)) => json!({
            "behavior": "allow",
            "updatedInput": input,
            "updatedPermissions": [session_rules::session_permission_update(rule)],
        }),
        (PermissionOutcome::AllowOnce | PermissionOutcome::AllowAlways, _) => json!({
            "behavior": "allow",
            "updatedInput": input,
        }),
        (
            PermissionOutcome::RejectOnce
            | PermissionOutcome::RejectAlways
            | PermissionOutcome::Answer
            | PermissionOutcome::AbortTurn,
            _,
        ) => json!({
            "behavior": "deny",
            "message": "User rejected this tool request",
        }),
    }
}

/// Lift the parts a person needs to judge a `can_use_tool` request to the
/// shared top-level `command` / `reason` keys every client already renders for
/// Codex approvals. `decision_reason` explains why Claude asked at all — a
/// `safetyCheck` asks even under `bypassPermissions`.
fn claude_permission_details(request: &Value) -> Value {
    let mut details = request.clone();
    let command = request
        .pointer("/input/command")
        .and_then(Value::as_str)
        .map(str::to_owned);
    let reason = request
        .get("decision_reason")
        .and_then(Value::as_str)
        .filter(|reason| !reason.trim().is_empty())
        .map(str::to_owned);
    if let Some(object) = details.as_object_mut() {
        if let Some(command) = command {
            object.insert("command".to_owned(), Value::String(command));
        }
        if let Some(reason) = reason {
            object.insert("reason".to_owned(), Value::String(reason));
        }
    }
    details
}

/// Claude's clarifying-question tool. It is answered rather than approved:
/// the tool reads the user's picks from `updatedInput.answers`, so allowing it
/// with the original input reads as "the user did not answer".
const ASK_USER_QUESTION_TOOL: &str = "AskUserQuestion";

fn claude_question_id(index: usize) -> String {
    format!("q{index}")
}

/// Surface `AskUserQuestion` as the shared `user_input` request so every
/// client's question form applies. Returns `None` for malformed input, which
/// then falls back to a plain tool approval.
fn claude_question_details(input: &Value) -> Option<Value> {
    let questions = input.get("questions")?.as_array()?;
    if questions.is_empty() {
        return None;
    }
    let questions = questions
        .iter()
        .enumerate()
        .map(|(index, question)| {
            let text = question
                .get("question")
                .and_then(Value::as_str)
                .filter(|text| !text.trim().is_empty())?;
            Some(json!({
                "id": claude_question_id(index),
                "question": text,
                "header": question.get("header").and_then(Value::as_str),
                "options": question.get("options").filter(|options| options.is_array()),
                "multiSelect": question.get("multiSelect").and_then(Value::as_bool).unwrap_or(false),
                // Claude accepts a free-text answer in place of the options.
                "isOther": true,
            }))
        })
        .collect::<Option<Vec<_>>>()?;
    Some(json!({ "questions": questions }))
}

/// Map `user_input` answers (validated by the broker to cover every question)
/// back to Claude's shape: keyed by question text, multi-select picks joined
/// with ", " as the SDK documents.
fn claude_question_response(input: &Value, decision: &PermissionDecision) -> Value {
    if !matches!(decision.outcome, PermissionOutcome::Answer) {
        return json!({
            "behavior": "deny",
            "message": "User declined to answer the question",
        });
    }
    let answers = decision.data.as_ref().and_then(|data| data.get("answers"));
    let mut mapped = Map::new();
    let questions = input.get("questions").and_then(Value::as_array);
    for (index, question) in questions.into_iter().flatten().enumerate() {
        let Some(text) = question.get("question").and_then(Value::as_str) else {
            continue;
        };
        let picks = answers
            .and_then(|answers| answers.get(claude_question_id(index)))
            .and_then(|answer| answer.get("answers"))
            .and_then(Value::as_array)
            .map(|values| {
                values
                    .iter()
                    .filter_map(Value::as_str)
                    .collect::<Vec<_>>()
                    .join(", ")
            })
            .unwrap_or_default();
        mapped.insert(text.to_owned(), Value::String(picks));
    }
    let mut updated = input.as_object().cloned().unwrap_or_default();
    updated.insert("answers".to_owned(), Value::Object(mapped));
    json!({ "behavior": "allow", "updatedInput": updated })
}
