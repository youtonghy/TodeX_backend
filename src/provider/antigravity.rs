//! Antigravity CLI (`agy`) driver.
//!
//! `agy` has no ACP or app-server mode; its print mode speaks its own
//! NDJSON (`--input-format stream-json --output-format stream-json`): one
//! `{"event":"user"}` frame in, then `init`, `step_update` and a terminal
//! `result` out. A process runs one turn and exits once stdin closes, which
//! lets it flush the conversation history it keeps under
//! `~/.gemini/antigravity-cli`; the next turn resumes it with
//! `--conversation <id>`.
//!
//! Headless `agy` cannot ask for approval: a tool needing it is denied. So
//! implement turns run with `--dangerously-skip-permissions` and TodeX gates
//! tools itself through a global `PreToolUse` hook ([`integration`],
//! decided in `agent_mcp::agy_hook` by the turn's ask / auto / full-access
//! mode). The same static config carries TodeX's MCP servers; both reach the
//! conversation through variables in the `agy` process environment. When
//! the hook cannot be installed or reached, ask / auto turns keep agy's own
//! review instead, which refuses whatever would need approval. Plan turns
//! always keep it (`--mode plan`), so anything beyond reading is refused.

mod integration;

pub(crate) use integration::{config_dir as integration_dir, remove as remove_integration};

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use serde_json::{json, Value};
use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::time::Duration;
use tokio::sync::watch;

use crate::config::AgentConfig;
use crate::conversation::ProviderKind;
use crate::error::AppError;
use crate::workspace_trust::WorkspaceTrustPermit;

use super::discovery::CatalogCache;
use super::process::{
    executable_available, provider_exit_error, run_bounded_command, stderr_excerpt, CommandSpec,
    JsonLineProcess,
};
use super::profile::{
    CatalogProfile, CatalogSource, ConfigHome, FileAttachmentStyle, McpInjection, ProcessModel,
    ProviderProfile, SkillInjection, UserConfigFile,
};
use super::types::{
    DriverContext, DriverEventSink, DriverPrompt, DriverTurnResult, ImageInputMode,
    PermissionConfigCapabilities, ProviderDescriptor, ProviderDriver, ProviderModelDescriptor,
};

const PROVIDER: &str = "antigravity";

/// What the Antigravity CLI supports and how TodeX adapts to it.
pub(super) const PROFILE: ProviderProfile = ProviderProfile {
    kind: ProviderKind::Antigravity,
    display_name: "Antigravity",
    permission_config: PermissionConfigCapabilities {
        modes: &["ask", "auto", "full-access"],
        default_mode: "ask",
        supports_plan: true,
        sandbox_modes: &[],
        approval_policies: &[],
        permission_profiles: &[],
        enforcement: "agent-policy",
        description: "agy runs with --dangerously-skip-permissions and a TodeX PreToolUse hook approves each tool: ask prompts for edits, commands, network and MCP tools; auto allows workspace edits and MCP tools; full-access allows everything. Plan uses agy --mode plan and only reads. Not an operating-system sandbox.",
    },
    native_fork: false,
    native_compact: false,
    native_resume: true,
    cancel: true,
    permissions: true,
    tool_events: true,
    native_skills: true,
    native_mcp: true,
    model_selection: true,
    image_input: false,
    image_input_mode: ImageInputMode::Model,
    mcp_injection: McpInjection::GlobalConfigEnv,
    skill_injection: SkillInjection::PromptText,
    file_attachments: FileAttachmentStyle::AtMention,
    profile_required: false,
    recovery_full_scan: false,
    process_model: ProcessModel::PerTurn,
    discovery_cache_ttl: Some(Duration::from_secs(60)),
    catalog: CatalogProfile {
        source: CatalogSource::Filesystem,
        config_home: ConfigHome {
            env: None,
            home_relative: ".gemini/config",
        },
        project_skills: ".agents/skills",
        mcp_user_files: &[UserConfigFile::ConfigHome("mcp_config.json")],
        mcp_project_files: &[],
        mcp_user_source: "antigravity-user",
        mcp_project_source: "antigravity-project",
    },
};

/// Effort suffixes `agy models` appends to one model's variants
/// (`gemini-3.8-flash-high`), lowest first.
const EFFORT_SUFFIXES: [&str; 3] = ["low", "medium", "high"];
const MODELS_TIMEOUT: Duration = Duration::from_secs(20);
const USAGE_TIMEOUT: Duration = Duration::from_secs(20);
const MAX_COMMAND_OUTPUT_BYTES: usize = 256 * 1024;
/// How long a finished turn's process may take to flush its history and
/// exit after stdin closes, before it is stopped.
const EXIT_GRACE: Duration = Duration::from_secs(10);

pub struct AntigravityDriver {
    binary: String,
    /// Daemon variables passed through to `agy`, e.g. `GEMINI_API_KEY`;
    /// provider processes otherwise start from a cleared environment.
    env_allowlist: Vec<String>,
    models: CatalogCache<Vec<ProviderModelDescriptor>>,
}

impl AntigravityDriver {
    pub fn new(config: &AgentConfig) -> Self {
        Self {
            binary: config.antigravity_bin.clone(),
            env_allowlist: config.antigravity_env_allowlist.clone(),
            models: CatalogCache::new(PROFILE.discovery_cache_ttl),
        }
    }

    fn command_spec(&self, cwd: &Path, args: &[&str]) -> CommandSpec {
        command_spec(&self.binary, &self.env_allowlist, cwd, args)
    }

    /// The `--model` id for a turn: catalog entries group the effort
    /// variants of one model, so the selected effort picks the variant.
    async fn launch_model(&self, workspace: &Path, prompt: &DriverPrompt) -> Option<String> {
        let model = prompt.model.as_deref()?;
        let efforts = match self.discover_models(workspace).await {
            Ok(models) => models
                .into_iter()
                .find(|entry| entry.id == model)
                .map(|entry| {
                    (
                        entry.supported_reasoning_efforts,
                        entry.default_reasoning_effort,
                    )
                }),
            Err(error) => {
                tracing::warn!(%error, "Antigravity model catalog unavailable; passing the model id unchanged");
                None
            }
        };
        Some(variant_id(
            model,
            efforts.as_ref().map(|(supported, _)| supported.as_slice()),
            prompt
                .reasoning_effort
                .as_deref()
                .or(efforts.as_ref().and_then(|(_, default)| default.as_deref())),
        ))
    }
}

#[async_trait]
impl ProviderDriver for AntigravityDriver {
    fn descriptor(&self) -> ProviderDescriptor {
        let available = executable_available(&self.binary);
        ProviderDescriptor {
            id: ProviderKind::Antigravity,
            display_name: PROFILE.display_name,
            available,
            unavailable_reason: (!available)
                .then(|| format!("executable '{}' was not found", self.binary)),
            profiles: Vec::new(),
            capabilities: PROFILE.capabilities(),
            models: Vec::new(),
        }
    }

    async fn discover_models(
        &self,
        workspace: &Path,
    ) -> Result<Vec<ProviderModelDescriptor>, AppError> {
        let fetch = async {
            let spec = self.command_spec(workspace, &["models"]);
            let output =
                run_bounded_command(&spec, MAX_COMMAND_OUTPUT_BYTES, MODELS_TIMEOUT).await?;
            if !output.success {
                return Err(AppError::ProviderUnavailable(format!(
                    "agy models failed: {}",
                    stderr_excerpt(&output.stderr).unwrap_or_default()
                )));
            }
            Ok(parse_models(&String::from_utf8_lossy(&output.stdout)))
        };
        self.models.fetch(&self.binary, workspace, fetch).await
    }

    fn discovery_timeout(&self) -> Duration {
        MODELS_TIMEOUT
    }

    async fn run_turn(
        &self,
        context: DriverContext,
        prompt: DriverPrompt,
        sink: DriverEventSink,
        mut cancel: watch::Receiver<bool>,
        launch_permit: WorkspaceTrustPermit,
    ) -> Result<DriverTurnResult, AppError> {
        let controls = super::types::resolve_execution_config(
            context.manifest.provider,
            prompt.permission_mode.as_deref(),
            prompt.work_mode.as_deref(),
            prompt.permission_profile.as_deref(),
            prompt.sandbox_mode.as_deref(),
            prompt.approval_policy.as_deref(),
        )?;
        let workspace = context.manifest.workspace.clone();
        let global = context
            .agent_mcp
            .as_ref()
            .and_then(|launch| launch.global.clone());
        let bridged = match &global {
            Some(global) => match install_integration(&global.daemon).await {
                Ok(()) => true,
                Err(error) => {
                    tracing::warn!(%error, "could not install TodeX's Antigravity hook and MCP entries");
                    false
                }
            },
            None => false,
        };
        let mut spec = self.command_spec(
            &workspace,
            &[
                "--input-format",
                "stream-json",
                "--output-format",
                "stream-json",
            ],
        );
        if let (true, Some(global)) = (bridged, &global) {
            spec.env.extend(global.env.iter().cloned());
        }
        if controls.work_mode == "plan" {
            spec.args.extend(["--mode".to_owned(), "plan".to_owned()]);
        } else if bridged || controls.permission_mode == "full-access" {
            spec.args.push("--dangerously-skip-permissions".to_owned());
        } else {
            // Without TodeX's hook nothing would gate tools: keep agy's own
            // review, which refuses what needs approval.
            sink.emit(
                "provider.event",
                json!({
                    "provider": PROVIDER,
                    "providerMethod": "approval_bridge_unavailable",
                    "metadata": {
                        "reason": "TodeX could not install its Antigravity approval hook; tools that need approval are refused. Use full access or check the daemon log.",
                    },
                }),
            )
            .await?;
        }
        let requested = context.provider_state.native_session_id.clone();
        if let Some(conversation) = &requested {
            spec.push_flag_value("--conversation", conversation)?;
        }
        let model = self.launch_model(&workspace, &prompt).await;
        if let Some(model) = &model {
            spec.push_flag_value("--model", model)?;
        }
        // `-p` takes a value and would swallow the next flag; the prompt
        // arrives on stdin, so the flag closes the argument list empty.
        spec.args.push("-p=".to_owned());

        let mut process = JsonLineProcess::spawn_trusted(&spec, launch_permit).await?;
        let result = run_agy_turn(
            &mut process,
            self,
            context,
            &prompt,
            requested,
            model.as_deref(),
            &sink,
            &mut cancel,
        )
        .await;
        if result.as_ref().is_ok_and(|result| !result.cancelled) {
            process.close_stdin();
            if !process.wait_exit(EXIT_GRACE).await {
                tracing::warn!("agy did not exit after its turn; stopping it");
            }
        }
        process.terminate().await;
        result
    }
}

#[allow(clippy::too_many_arguments)]
async fn run_agy_turn(
    process: &mut JsonLineProcess,
    driver: &AntigravityDriver,
    context: DriverContext,
    prompt: &DriverPrompt,
    requested: Option<String>,
    model: Option<&str>,
    sink: &DriverEventSink,
    cancel: &mut watch::Receiver<bool>,
) -> Result<DriverTurnResult, AppError> {
    process
        .send(&json!({
            "event": "user",
            "message": { "role": "user", "content": prompt.text },
        }))
        .await?;
    let mut turn = AgyTurn::new(&prompt.turn_id);
    let mut provider_state = context.provider_state;
    let mut conversation = requested.clone();
    loop {
        let frame = tokio::select! {
            frame = process.read_frame() => sink.provider_frame(frame?).await?,
            changed = cancel.changed() => {
                let _ = changed;
                return Ok(DriverTurnResult {
                    native_session_id: conversation,
                    stop_reason: "cancelled".to_owned(),
                    cancelled: true,
                });
            }
        };
        let Some(frame) = frame else {
            return Err(provider_exit_error(process, "Antigravity CLI closed stdout").await);
        };
        match frame.get("event").and_then(Value::as_str) {
            Some("init") => {
                let Some(id) = frame.get("conversation_id").and_then(Value::as_str) else {
                    continue;
                };
                if requested
                    .as_deref()
                    .is_some_and(|requested| requested != id)
                {
                    // agy starts a new conversation when the recorded one is
                    // gone (deleted, or another machine's state).
                    sink.emit(
                        "provider.event",
                        json!({
                            "provider": PROVIDER,
                            "providerMethod": "session/recreated",
                            "metadata": {
                                "reason": "Antigravity could not find the previous conversation",
                                "previousSessionId": requested,
                            },
                        }),
                    )
                    .await?;
                }
                // Persist at once so a failed or cancelled turn still resumes
                // the conversation agy has already written.
                if provider_state.native_session_id.as_deref() != Some(id) {
                    provider_state.native_session_id = Some(id.to_owned());
                    provider_state.recoverable = true;
                    sink.save_provider_state(provider_state.clone()).await?;
                }
                conversation = Some(id.to_owned());
                sink.emit(
                    "provider.event",
                    json!({
                        "provider": PROVIDER,
                        "providerMethod": "init",
                        "metadata": {
                            "sessionId": id,
                            "model": frame.pointer("/init/model"),
                            "permissionMode": frame.pointer("/init/permission_mode"),
                        },
                    }),
                )
                .await?;
            }
            Some("step_update") => {
                let Some(step) = frame.get("step_update") else {
                    continue;
                };
                for emit in turn.step(step) {
                    emit.send(sink).await?;
                }
            }
            Some("result") => {
                let result = frame.get("result").cloned().unwrap_or(Value::Null);
                if let Some(usage) = result.get("usage").filter(|usage| usage.is_object()) {
                    sink.emit(
                        "usage.updated",
                        json!({ "provider": PROVIDER, "turnId": prompt.turn_id, "source": "provider", "scope": "turn", "usage": usage }),
                    )
                    .await?;
                }
                if let Some(denied) = result
                    .get("denied_actions")
                    .and_then(Value::as_array)
                    .filter(|denied| !denied.is_empty())
                {
                    sink.emit(
                        "provider.event",
                        json!({
                            "provider": PROVIDER,
                            "providerMethod": "denied_actions",
                            "metadata": { "deniedActions": denied },
                        }),
                    )
                    .await?;
                }
                let native_session_id = result
                    .get("conversation_id")
                    .and_then(Value::as_str)
                    .filter(|id| !id.is_empty())
                    .map(ToOwned::to_owned)
                    .or(conversation);
                if result.get("status").and_then(Value::as_str) != Some("SUCCESS") {
                    let detail = result
                        .get("error")
                        .and_then(Value::as_str)
                        .filter(|error| !error.is_empty())
                        .map(ToOwned::to_owned)
                        .or_else(|| turn.error_text.clone())
                        .unwrap_or_else(|| "Antigravity returned an error".to_owned());
                    return Err(fail_turn(driver, model, &detail, sink).await);
                }
                provider_state.native_session_id = native_session_id.clone();
                provider_state.recoverable = true;
                provider_state.last_error = None;
                sink.save_provider_state(provider_state).await?;
                return Ok(DriverTurnResult {
                    native_session_id,
                    stop_reason: "completed".to_owned(),
                    cancelled: false,
                });
            }
            Some(other) => {
                sink.emit(
                    "provider.event",
                    json!({ "provider": PROVIDER, "providerMethod": other }),
                )
                .await?;
            }
            None => {}
        }
    }
}

/// The error a failed turn ends with. A quota failure first records which
/// window ran out, so the follow-up queue resumes once it resets.
async fn fail_turn(
    driver: &AntigravityDriver,
    model: Option<&str>,
    detail: &str,
    sink: &DriverEventSink,
) -> AppError {
    if is_quota_error(detail) {
        match fetch_usage(&driver.binary, &driver.env_allowlist).await {
            Ok(usage) => {
                if let Some(until) = exhausted_until(&usage, model) {
                    if let Err(error) = sink
                        .emit("quota.updated", rejected_quota(&usage, until))
                        .await
                    {
                        return error;
                    }
                }
            }
            Err(error) => {
                tracing::warn!(%error, "could not read Antigravity quota after a limit error")
            }
        }
    }
    AppError::ProviderUnavailable(detail.chars().take(1000).collect())
}

fn is_quota_error(detail: &str) -> bool {
    let detail = detail.to_ascii_lowercase();
    [
        "quota",
        "rate limit",
        "resource_exhausted",
        "credits",
        "429",
    ]
    .iter()
    .any(|marker| detail.contains(marker))
}

fn command_spec(binary: &str, env_allowlist: &[String], cwd: &Path, args: &[&str]) -> CommandSpec {
    let mut spec = CommandSpec::new(binary, cwd);
    spec.args = args.iter().map(|arg| (*arg).to_owned()).collect();
    spec.env = super::resident::allowlisted_environment(&[("NO_COLOR", "1")], env_allowlist);
    spec
}

/// Serializes writes to agy's global config between concurrent turns.
static INTEGRATION_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

/// Brings TodeX's hook and MCP entries in `~/.gemini/config` up to date,
/// retrying once when another tool edited a file meanwhile.
async fn install_integration(daemon: &Path) -> Result<(), AppError> {
    let _serial = INTEGRATION_LOCK.lock().await;
    let daemon = daemon.to_owned();
    let dir = integration::config_dir();
    tokio::task::spawn_blocking(move || match integration::ensure(&dir, &daemon) {
        Err(AppError::Conflict(_)) => integration::ensure(&dir, &daemon),
        other => other,
    })
    .await
    .map_err(|error| AppError::Anyhow(error.into()))?
}

/// One emitted conversation event.
#[derive(Debug, PartialEq)]
enum Emit {
    Delta {
        event: &'static str,
        payload: Value,
        text: &'static [&'static str],
        block: String,
    },
    Event {
        event: &'static str,
        payload: Value,
    },
}

impl Emit {
    async fn send(self, sink: &DriverEventSink) -> Result<(), AppError> {
        match self {
            Self::Delta {
                event,
                payload,
                text,
                block,
            } => sink.emit_delta(event, payload, text, Some(&block)).await,
            Self::Event { event, payload } => sink.emit(event, payload).await.map(|_| ()),
        }
    }
}

/// Maps `step_update` frames to conversation events. A step is identified by
/// its `step_index`; tool steps report ACTIVE then DONE or ERROR, response
/// steps stream `text_delta` fragments and end DONE.
struct AgyTurn {
    turn_id: String,
    /// Text streamed so far per response step.
    responses: HashMap<u64, String>,
    /// Tool steps already announced.
    tools: HashSet<u64>,
    /// The last `error_message` step, for a failing `result` without one.
    error_text: Option<String>,
}

impl AgyTurn {
    fn new(turn_id: &str) -> Self {
        Self {
            turn_id: turn_id.to_owned(),
            responses: HashMap::new(),
            tools: HashSet::new(),
            error_text: None,
        }
    }

    fn step(&mut self, step: &Value) -> Vec<Emit> {
        let index = step.get("step_index").and_then(Value::as_u64).unwrap_or(0);
        let state = step.get("state").and_then(Value::as_str).unwrap_or("");
        let kind = step.get("step_type").and_then(Value::as_str).unwrap_or("");
        let mut out = Vec::new();
        if let Some(thinking) = step
            .get("thinking_delta")
            .and_then(Value::as_str)
            .filter(|text| !text.is_empty())
        {
            out.push(Emit::Delta {
                event: "thought.delta",
                payload: json!({ "provider": PROVIDER, "role": "assistant", "delta": { "type": "thinking_delta", "thinking": thinking } }),
                text: &["/delta/thinking"],
                block: format!("thinking-{index}"),
            });
        }
        match kind {
            "tool" | "subagent" => out.extend(self.tool(index, state, kind, step)),
            "user_input" => {}
            _ => {
                if let Some(text) = step
                    .get("text_delta")
                    .and_then(Value::as_str)
                    .filter(|text| !text.is_empty())
                {
                    self.responses.entry(index).or_default().push_str(text);
                    out.push(Emit::Delta {
                        event: "message.delta",
                        payload: json!({ "provider": PROVIDER, "role": "assistant", "delta": { "type": "text_delta", "text": text } }),
                        text: &["/delta/text"],
                        block: format!("text-{index}"),
                    });
                }
                if kind == "error_message" {
                    if let Some(text) = self.responses.get(&index).filter(|text| !text.is_empty()) {
                        self.error_text = Some(text.clone());
                    }
                }
                if state != "ACTIVE" {
                    if let Some(text) = self.responses.remove(&index) {
                        if kind != "error_message" && !text.trim().is_empty() {
                            out.push(Emit::Event {
                                event: "message.completed",
                                payload: json!({
                                    "provider": PROVIDER,
                                    "message": { "role": "assistant", "content": [{ "type": "text", "text": text }] },
                                }),
                            });
                        }
                    }
                    if !matches!(kind, "agent_response" | "planner_response") {
                        out.push(Emit::Event {
                            event: "provider.event",
                            payload: json!({ "provider": PROVIDER, "providerMethod": kind, "metadata": { "state": state } }),
                        });
                    }
                }
            }
        }
        out
    }

    fn tool(&mut self, index: u64, state: &str, kind: &str, step: &Value) -> Vec<Emit> {
        let info = step.get("tool_info").cloned().unwrap_or(Value::Null);
        let name = step
            .get("tool_name")
            .or_else(|| info.get("name"))
            .cloned()
            .unwrap_or_else(|| json!(kind));
        let error = info
            .pointer("/error/message")
            .or_else(|| info.get("error"))
            .filter(|error| !error.is_null())
            .cloned();
        let finished = state != "ACTIVE";
        let started = self.tools.insert(index);
        let phase = if finished {
            "completed"
        } else if started {
            "started"
        } else {
            "delta"
        };
        let event = match phase {
            "completed" => "tool.completed",
            "started" => "tool.started",
            _ => "tool.updated",
        };
        let result = if finished {
            error.clone().or_else(|| info.get("output").cloned())
        } else {
            None
        };
        let id = format!("step-{index}");
        let mut events = Vec::new();
        if finished && started {
            // A step first seen finished still opens its card.
            events.push(Emit::Event {
                event: "tool.started",
                payload: tool_payload(
                    &id,
                    &name,
                    &info,
                    None,
                    None,
                    step,
                    &self.turn_id,
                    "started",
                ),
            });
        }
        events.push(Emit::Event {
            event,
            payload: tool_payload(
                &id,
                &name,
                &info,
                result,
                finished.then_some(state == "ERROR" || error.is_some()),
                step,
                &self.turn_id,
                phase,
            ),
        });
        events
    }
}

#[allow(clippy::too_many_arguments)]
fn tool_payload(
    id: &str,
    name: &Value,
    info: &Value,
    result: Option<Value>,
    is_error: Option<bool>,
    step: &Value,
    turn_id: &str,
    phase: &str,
) -> Value {
    json!({
        "provider": PROVIDER,
        "toolCallId": id,
        "toolName": name,
        "arguments": info.get("parameters").cloned().unwrap_or(Value::Null),
        "result": result,
        "isError": is_error,
        "subagent": step.get("subagent_info"),
        "block": { "category": "tool", "id": id, "turnId": turn_id, "phase": phase },
    })
}

/// `agy models` prints `<id>\t<display name>` per line. Variants of one
/// model that differ only in an effort suffix become one entry whose
/// efforts select the variant.
fn parse_models(stdout: &str) -> Vec<ProviderModelDescriptor> {
    let mut models: Vec<ProviderModelDescriptor> = Vec::new();
    for line in stdout.lines() {
        let Some((id, display)) = line.split_once('\t') else {
            continue;
        };
        let (id, display) = (id.trim(), display.trim());
        if id.is_empty() {
            continue;
        }
        let (base, effort) = match EFFORT_SUFFIXES.iter().find_map(|effort| {
            id.strip_suffix(&format!("-{effort}"))
                .map(|base| (base, *effort))
        }) {
            Some((base, effort)) => (base, Some(effort)),
            None => (id, None),
        };
        let display = match effort {
            Some(_) => display
                .rsplit_once(" (")
                .map_or(display, |(name, _)| name)
                .to_owned(),
            None => display.to_owned(),
        };
        let entry = match models.iter_mut().position(|model| model.id == base) {
            Some(position) => &mut models[position],
            None => {
                models.push(ProviderModelDescriptor {
                    id: base.to_owned(),
                    display_name: display,
                    description: "Antigravity model".to_owned(),
                    is_default: false,
                    supported_reasoning_efforts: Vec::new(),
                    default_reasoning_effort: None,
                    context_window: None,
                    image_input: Some(false),
                    family: base.split('-').next().map(str::to_owned),
                });
                models.last_mut().expect("just pushed")
            }
        };
        if let Some(effort) = effort {
            entry.supported_reasoning_efforts.push(effort.to_owned());
        }
    }
    for model in &mut models {
        model
            .supported_reasoning_efforts
            .sort_by_key(|effort| EFFORT_SUFFIXES.iter().position(|known| known == effort));
        model.default_reasoning_effort = ["medium", "high", "low"]
            .into_iter()
            .find(|effort| {
                model
                    .supported_reasoning_efforts
                    .iter()
                    .any(|known| known == effort)
            })
            .map(str::to_owned);
    }
    models
}

/// The concrete id to launch: `model` plus the effort suffix when the
/// catalog groups variants under it, otherwise `model` unchanged.
fn variant_id(model: &str, efforts: Option<&[String]>, effort: Option<&str>) -> String {
    match (efforts, effort) {
        (Some(efforts), Some(effort)) if efforts.iter().any(|known| known == effort) => {
            format!("{model}-{effort}")
        }
        (Some(efforts), _) if !efforts.is_empty() => {
            // A stale effort the model no longer offers: its default variant.
            let fallback = ["medium", "high", "low"]
                .into_iter()
                .find(|effort| efforts.iter().any(|known| known == effort))
                .unwrap_or(efforts[0].as_str());
            format!("{model}-{fallback}")
        }
        _ => model.to_owned(),
    }
}

/// `agy -p /usage --output-format json`: the quota groups of the account.
/// It answers locally without an agent turn and costs no quota.
pub(crate) async fn fetch_usage(binary: &str, env_allowlist: &[String]) -> Result<Value, AppError> {
    let spec = command_spec(
        binary,
        env_allowlist,
        &std::env::temp_dir(),
        &["--output-format", "json", "-p=/usage"],
    );
    let output = run_bounded_command(&spec, MAX_COMMAND_OUTPUT_BYTES, USAGE_TIMEOUT).await?;
    let parsed: Value = serde_json::from_slice(&output.stdout).map_err(|error| {
        AppError::ProviderUnavailable(format!(
            "agy /usage returned invalid JSON ({error}): {}",
            stderr_excerpt(&output.stderr).unwrap_or_default()
        ))
    })?;
    if !output.success || parsed.get("status").and_then(Value::as_str) != Some("SUCCESS") {
        return Err(AppError::ProviderUnavailable(format!(
            "agy /usage failed: {}",
            parsed
                .get("error")
                .and_then(Value::as_str)
                .map(ToOwned::to_owned)
                .or_else(|| stderr_excerpt(&output.stderr))
                .unwrap_or_default()
        )));
    }
    parsed
        .pointer("/command/data")
        .cloned()
        .ok_or_else(|| AppError::ProviderUnavailable("agy /usage returned no data".to_owned()))
}

/// The shared account-quota payload for `/usage` data: one window per
/// bucket (`gemini-weekly`, `gemini-5h`, `3p-weekly`, `3p-5h`).
pub(crate) fn quota_payload(usage: &Value) -> Value {
    let windows: Vec<Value> = buckets(usage)
        .map(|(_, bucket)| {
            json!({
                "id": bucket.get("id"),
                "durationMins": match bucket.get("window").and_then(Value::as_str) {
                    Some("5h") => Some(300),
                    Some("weekly") => Some(7 * 24 * 60),
                    _ => None,
                },
                "usedPercent": bucket
                    .get("remaining_fraction")
                    .and_then(Value::as_f64)
                    .map(|remaining| ((1.0 - remaining) * 100.0).clamp(0.0, 100.0)),
                "resetsAt": bucket
                    .get("reset_time")
                    .and_then(Value::as_str)
                    .and_then(|time| DateTime::parse_from_rfc3339(time).ok())
                    .map(|time| time.timestamp()),
            })
        })
        .collect();
    json!({
        "provider": PROVIDER,
        "scope": "account",
        "windows": windows,
        "raw": usage,
    })
}

fn rejected_quota(usage: &Value, until: DateTime<Utc>) -> Value {
    let mut payload = quota_payload(usage);
    payload["status"] = json!("rejected");
    payload["raw"] = json!({ "resetsAt": until.timestamp(), "usage": usage });
    payload
}

fn buckets(usage: &Value) -> impl Iterator<Item = (&Value, &Value)> {
    usage
        .get("groups")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .flat_map(|group| {
            group
                .get("buckets")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
                .map(move |bucket| (group, bucket))
        })
}

/// When the quota covering `model` frees up again: the latest reset among
/// its group's exhausted buckets. `None` when nothing is exhausted, e.g. a
/// credits balance that no window refills.
fn exhausted_until(usage: &Value, model: Option<&str>) -> Option<DateTime<Utc>> {
    let gemini = model.is_none_or(|model| model.starts_with("gemini"));
    buckets(usage)
        .filter(|(group, _)| {
            let name = group
                .get("name")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_ascii_lowercase();
            name.contains("gemini") == gemini
        })
        .filter(|(_, bucket)| {
            bucket
                .get("remaining_fraction")
                .and_then(Value::as_f64)
                .is_some_and(|remaining| remaining <= 0.0)
        })
        .filter_map(|(_, bucket)| {
            bucket
                .get("reset_time")
                .and_then(Value::as_str)
                .and_then(|time| DateTime::parse_from_rfc3339(time).ok())
                .map(|time| time.with_timezone(&Utc))
        })
        .max()
}

#[cfg(test)]
mod tests {
    use super::*;

    const MODELS: &str = "gemini-3.8-flash-high\tGemini 3.8 Flash (High)\n\
gemini-3.8-flash-medium\tGemini 3.8 Flash (Medium)\n\
gemini-3.8-flash-low\tGemini 3.8 Flash (Low)\n\
gemini-3.1-pro-high\tGemini 3.1 Pro (High)\n\
gemini-3.1-pro-low\tGemini 3.1 Pro (Low)\n\
claude-opus-4-6-thinking\tClaude Opus 4.6 (Thinking)\n\
gpt-oss-120b-medium\tGPT-OSS 120B (Medium)\n";

    #[test]
    fn effort_variants_group_under_one_model() {
        let models = parse_models(&format!("Fetching available models...\n{MODELS}"));
        let summary: Vec<_> = models
            .iter()
            .map(|model| {
                (
                    model.id.as_str(),
                    model.display_name.as_str(),
                    model.supported_reasoning_efforts.clone(),
                    model.default_reasoning_effort.clone(),
                    model.family.clone(),
                )
            })
            .collect();
        assert_eq!(
            summary,
            [
                (
                    "gemini-3.8-flash",
                    "Gemini 3.8 Flash",
                    vec!["low".to_owned(), "medium".to_owned(), "high".to_owned()],
                    Some("medium".to_owned()),
                    Some("gemini".to_owned()),
                ),
                (
                    "gemini-3.1-pro",
                    "Gemini 3.1 Pro",
                    vec!["low".to_owned(), "high".to_owned()],
                    Some("high".to_owned()),
                    Some("gemini".to_owned()),
                ),
                (
                    "claude-opus-4-6-thinking",
                    "Claude Opus 4.6 (Thinking)",
                    vec![],
                    None,
                    Some("claude".to_owned()),
                ),
                (
                    "gpt-oss-120b",
                    "GPT-OSS 120B",
                    vec!["medium".to_owned()],
                    Some("medium".to_owned()),
                    Some("gpt".to_owned()),
                ),
            ]
        );
    }

    #[test]
    fn the_selected_effort_picks_the_variant() {
        let efforts = vec!["low".to_owned(), "high".to_owned()];
        assert_eq!(
            variant_id("gemini-3.1-pro", Some(&efforts), Some("low")),
            "gemini-3.1-pro-low"
        );
        assert_eq!(
            variant_id("gemini-3.1-pro", Some(&efforts), Some("medium")),
            "gemini-3.1-pro-high"
        );
        assert_eq!(
            variant_id("claude-opus-4-6-thinking", Some(&[]), Some("high")),
            "claude-opus-4-6-thinking"
        );
        assert_eq!(
            variant_id("gemini-3.8-flash-high", None, Some("low")),
            "gemini-3.8-flash-high"
        );
    }

    #[test]
    fn steps_map_to_text_tool_and_completion_events() {
        let mut turn = AgyTurn::new("turn-1");
        let mut events = Vec::new();
        for step in [
            json!({"step_index":0,"state":"DONE","step_type":"user_input"}),
            json!({"step_index":1,"state":"ACTIVE","step_type":"agent_response","text_delta":"Hel"}),
            json!({"step_index":1,"state":"DONE","step_type":"agent_response","text_delta":"lo"}),
            json!({"step_index":2,"state":"ACTIVE","step_type":"tool","tool_name":"run_command","tool_info":{"name":"run_command","parameters":{"CommandLine":"date"}}}),
            json!({"step_index":2,"state":"DONE","step_type":"tool","tool_name":"run_command","tool_info":{"name":"run_command","parameters":{"CommandLine":"date"},"output":"Fri\n"}}),
            json!({"step_index":3,"state":"ERROR","step_type":"tool","tool_name":"write_to_file","tool_info":{"name":"write_to_file","parameters":{"TargetFile":"/x"},"error":{"type":"TOOL_ERROR","message":"denied"}}}),
        ] {
            events.extend(turn.step(&step));
        }
        let kinds: Vec<_> = events
            .iter()
            .map(|emit| match emit {
                Emit::Delta { event, .. } | Emit::Event { event, .. } => *event,
            })
            .collect();
        assert_eq!(
            kinds,
            [
                "message.delta",
                "message.delta",
                "message.completed",
                "tool.started",
                "tool.completed",
                "tool.started",
                "tool.completed",
            ]
        );
        let Emit::Event { payload, .. } = &events[2] else {
            panic!("message.completed is a plain event");
        };
        assert_eq!(payload["message"]["content"][0]["text"], "Hello");
        let Emit::Event { payload, .. } = &events[4] else {
            panic!("tool.completed is a plain event");
        };
        assert_eq!(payload["result"], "Fri\n");
        assert_eq!(payload["isError"], false);
        assert_eq!(payload["arguments"]["CommandLine"], "date");
        let Emit::Event { payload, .. } = &events[6] else {
            panic!("tool.completed is a plain event");
        };
        assert_eq!(payload["result"], "denied");
        assert_eq!(payload["isError"], true);
    }

    fn usage(gemini_weekly: f64) -> Value {
        json!({"groups":[
            {"name":"Gemini Models","buckets":[
                {"id":"gemini-weekly","name":"Weekly Limit Remaining","window":"weekly","remaining_fraction":gemini_weekly,"reset_time":"2026-10-15T06:45:48Z"},
                {"id":"gemini-5h","name":"Five Hour Limit Remaining","window":"5h","remaining_fraction":0.5,"reset_time":"2026-10-09T10:48:32Z"}
            ]},
            {"name":"Claude and GPT models","buckets":[
                {"id":"other-5h","name":"Five Hour Limit Remaining","window":"5h","remaining_fraction":0.0,"reset_time":"2026-10-09T11:02:17Z"}
            ]}
        ]})
    }

    #[test]
    fn usage_buckets_become_quota_windows() {
        let payload = quota_payload(&usage(0.25));
        assert_eq!(payload["provider"], "antigravity");
        assert_eq!(payload["windows"][0]["id"], "gemini-weekly");
        assert_eq!(payload["windows"][0]["usedPercent"], 75.0);
        assert_eq!(payload["windows"][0]["durationMins"], 10080);
        assert_eq!(payload["windows"][1]["durationMins"], 300);
        assert_eq!(
            payload["windows"][0]["resetsAt"],
            DateTime::parse_from_rfc3339("2026-10-15T06:45:48Z")
                .unwrap()
                .timestamp()
        );
        assert_eq!(payload["windows"].as_array().unwrap().len(), 3);
    }

    #[test]
    fn only_the_models_own_exhausted_group_blocks_it() {
        let reset = |time: &str| {
            Some(
                DateTime::parse_from_rfc3339(time)
                    .unwrap()
                    .with_timezone(&Utc),
            )
        };
        assert_eq!(
            exhausted_until(&usage(0.25), Some("gemini-3.8-flash-high")),
            None
        );
        assert_eq!(
            exhausted_until(&usage(0.0), Some("gemini-3.8-flash-high")),
            reset("2026-10-15T06:45:48Z")
        );
        assert_eq!(
            exhausted_until(&usage(0.25), Some("claude-opus-4-6-thinking")),
            reset("2026-10-09T11:02:17Z")
        );
        assert!(is_quota_error(
            "Your AI credits balance is too low to continue."
        ));
        assert!(!is_quota_error("model not found"));
    }

    #[cfg(unix)]
    mod wire {
        use super::super::*;
        use crate::conversation::{ConversationEventHub, ConversationManifest, ConversationStore};
        use crate::provider::types::PermissionBroker;
        use std::path::PathBuf;

        struct Fixture {
            root: PathBuf,
            driver: AntigravityDriver,
            store: ConversationStore,
            manifest: ConversationManifest,
            trust: crate::workspace_trust::WorkspaceTrustStore,
        }

        impl Fixture {
            async fn new() -> Self {
                Self::with_binary(None).await
            }

            /// `binary` runs a real `agy`; `None` installs the fixture.
            async fn with_binary(binary: Option<String>) -> Self {
                use std::os::unix::fs::PermissionsExt;
                let root = std::env::temp_dir()
                    .join(format!("todex-agy-wire-{}", uuid::Uuid::new_v4().simple()));
                std::fs::create_dir_all(&root).unwrap();
                let root = std::fs::canonicalize(root).unwrap();
                let binary = binary.unwrap_or_else(|| {
                    let binary = root.join("agy-fixture");
                    std::fs::write(
                        &binary,
                        include_str!("../../tests/fixtures/antigravity_stream_fixture.py"),
                    )
                    .unwrap();
                    std::fs::set_permissions(&binary, std::fs::Permissions::from_mode(0o755))
                        .unwrap();
                    binary.to_string_lossy().to_string()
                });
                let driver = AntigravityDriver {
                    binary,
                    env_allowlist: Vec::new(),
                    models: CatalogCache::new(None),
                };
                let store = ConversationStore::new(root.join("data")).await.unwrap();
                let manifest = store
                    .create(ConversationManifest::new(
                        ProviderKind::Antigravity,
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

            async fn run(
                &self,
                turn_id: &str,
                text: &str,
                // `plan`, a permission mode, or `None` for full access.
                mode: Option<&str>,
                cancel_after: Option<Duration>,
            ) -> Result<DriverTurnResult, AppError> {
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
                    model: Some("gemini-3.8-flash".to_owned()),
                    reasoning_effort: Some("low".to_owned()),
                    permission_mode: Some(
                        match mode {
                            None | Some("plan") => "full-access",
                            Some(mode) => mode,
                        }
                        .to_owned(),
                    ),
                    work_mode: mode.filter(|mode| *mode == "plan").map(ToOwned::to_owned),
                    permission_profile: None,
                    sandbox_mode: None,
                    approval_policy: None,
                };
                let sink = DriverEventSink::new(
                    self.store.clone(),
                    ConversationEventHub::default(),
                    PermissionBroker::default(),
                    &self.manifest.id,
                )
                .with_turn_id(turn_id);
                let permit = self.trust.acquire_owned("local", &self.root).await.unwrap();
                let (cancel, receiver) = watch::channel(false);
                if let Some(delay) = cancel_after {
                    tokio::spawn(async move {
                        tokio::time::sleep(delay).await;
                        let _ = cancel.send(true);
                    });
                    self.driver
                        .run_turn(context, prompt, sink, receiver, permit)
                        .await
                } else {
                    let result = self
                        .driver
                        .run_turn(context, prompt, sink, receiver, permit)
                        .await;
                    drop(cancel);
                    result
                }
            }

            async fn events(&self) -> Vec<(String, Value)> {
                self.store
                    .complete_history(&self.manifest.id)
                    .await
                    .unwrap()
                    .into_iter()
                    .map(|event| (event.event_type, event.payload))
                    .collect()
            }

            /// The argv of every launch, one JSON array per line.
            fn launches(&self) -> Vec<Vec<String>> {
                std::fs::read_to_string(self.root.join("argv.log"))
                    .unwrap_or_default()
                    .lines()
                    .map(|line| serde_json::from_str::<Vec<String>>(line).unwrap())
                    .filter(|argv| argv.first().map(String::as_str) != Some("models"))
                    .collect()
            }
        }

        impl Drop for Fixture {
            fn drop(&mut self) {
                let _ = std::fs::remove_dir_all(&self.root);
            }
        }

        #[tokio::test]
        async fn a_turn_streams_text_and_tools_then_the_next_resumes_it() {
            let fixture = Fixture::new().await;
            let result = fixture.run("turn-1", "hello", None, None).await.unwrap();
            assert_eq!(result.native_session_id.as_deref(), Some("agy-conv-1"));
            let events = fixture.events().await;
            let text: String = events
                .iter()
                .filter(|(kind, _)| kind == "message.delta")
                .filter_map(|(_, payload)| payload.pointer("/delta/text").and_then(Value::as_str))
                .collect();
            assert_eq!(text, "Hi there");
            assert!(events.iter().any(|(kind, payload)| kind == "tool.completed"
                && payload["toolName"] == "run_command"
                && payload["result"] == "ok\n"));
            assert!(events.iter().any(|(kind, _)| kind == "usage.updated"));

            fixture.run("turn-2", "again", None, None).await.unwrap();
            let launches = fixture.launches();
            assert_eq!(launches.len(), 2);
            assert!(launches[0].contains(&"--dangerously-skip-permissions".to_owned()));
            assert!(launches[0].contains(&"gemini-3.8-flash-low".to_owned()));
            assert!(!launches[0].contains(&"--conversation".to_owned()));
            let resume = launches[1]
                .iter()
                .position(|arg| arg == "--conversation")
                .unwrap();
            assert_eq!(launches[1][resume + 1], "agy-conv-1");
            assert_eq!(launches[1].last().map(String::as_str), Some("-p="));
        }

        #[tokio::test]
        async fn plan_turns_keep_agys_own_review() {
            let fixture = Fixture::new().await;
            fixture
                .run("turn-1", "hello", Some("plan"), None)
                .await
                .unwrap();
            let launch = &fixture.launches()[0];
            assert!(!launch.contains(&"--dangerously-skip-permissions".to_owned()));
            let mode = launch.iter().position(|arg| arg == "--mode").unwrap();
            assert_eq!(launch[mode + 1], "plan");
        }

        #[tokio::test]
        async fn ask_without_the_approval_hook_keeps_agys_own_review() {
            let fixture = Fixture::new().await;
            fixture
                .run("turn-1", "hello", Some("ask"), None)
                .await
                .unwrap();
            assert!(!fixture.launches()[0].contains(&"--dangerously-skip-permissions".to_owned()));
            assert!(fixture
                .events()
                .await
                .iter()
                .any(|(kind, payload)| kind == "provider.event"
                    && payload["providerMethod"] == "approval_bridge_unavailable"));
        }

        #[tokio::test]
        async fn an_error_result_fails_the_turn_with_its_message() {
            let fixture = Fixture::new().await;
            let error = fixture
                .run("turn-1", "FAIL model exploded", None, None)
                .await
                .unwrap_err();
            assert!(error.to_string().contains("model exploded"), "{error}");
        }

        #[tokio::test]
        async fn cancel_stops_a_running_turn() {
            let fixture = Fixture::new().await;
            let started = std::time::Instant::now();
            let result = fixture
                .run("turn-1", "HANG", None, Some(Duration::from_secs(2)))
                .await
                .unwrap();
            assert!(result.cancelled);
            assert_eq!(result.native_session_id.as_deref(), Some("agy-conv-1"));
            assert!(started.elapsed() < Duration::from_secs(10));
        }

        /// Opt-in: drives the installed `agy` (or `$TODEX_AGY_LIVE_BIN`)
        /// through a tool turn and a resumed turn, then reads its quota.
        /// Uses the signed-in account and a little of its quota.
        #[tokio::test]
        #[ignore = "opt-in: runs the real Antigravity CLI and spends quota"]
        async fn live_agy_turns_resume_and_quota() {
            let binary = std::env::var("TODEX_AGY_LIVE_BIN").unwrap_or_else(|_| "agy".to_owned());
            let fixture = Fixture::with_binary(Some(binary.clone())).await;
            let first = fixture
                .run(
                    "turn-1",
                    "Run the shell command `echo todex-live-check` and reply with its output only.",
                    None,
                    None,
                )
                .await
                .unwrap();
            let conversation = first.native_session_id.clone().unwrap();
            let events = fixture.events().await;
            assert!(
                events.iter().any(|(kind, payload)| kind == "tool.completed"
                    && payload["toolName"] == "run_command"
                    && payload["isError"] == false),
                "{events:#?}"
            );
            let second = fixture
                .run("turn-2", "Reply with the single word: resumed", None, None)
                .await
                .unwrap();
            assert_eq!(
                second.native_session_id.as_deref(),
                Some(conversation.as_str())
            );
            let usage = fetch_usage(&binary, &[]).await.unwrap();
            assert!(!quota_payload(&usage)["windows"]
                .as_array()
                .unwrap()
                .is_empty());
        }
    }
}
