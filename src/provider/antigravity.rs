//! Antigravity CLI (`agy`) driver over ACP (`agy acp`).
//!
//! Each conversation runs one resident `agy acp` process (see
//! [`super::resident`]); the shared ACP engine in [`super::acp`] drives it.
//! agy specifics live in [`ACP_QUIRKS`]:
//!
//! - agy binds a session to the working directory of the process that
//!   created it and ignores `session/new`'s `cwd`, so the process starts in
//!   the conversation's workspace.
//! - The model is chosen through the `model` config option with agy's own
//!   variant id (`gemini-3.8-flash-low`, the TodeX model plus its effort);
//!   agy sets `thought_level` from it, and restores neither model nor mode on
//!   `session/resume`, so both are restated every turn.
//! - Permission modes map onto agy's session modes: `plan` and `auto` are
//!   agy's own, `ask` and `full-access` run `default`, where agy asks through
//!   `session/request_permission`; TodeX answers `full-access` itself and
//!   passes the rest to the user. Calls to TodeX's own MCP servers are
//!   approved in every mode: those servers gate their tools by the turn's
//!   mode themselves. Without agy's Auto (an account preview) an `auto` turn
//!   runs as `ask` and says so (`auto_mode_unavailable`).
//! - agy streams a failed turn's error (quota, sign-in) as message text and
//!   ends it like a normal one, without usage; see [`agy_turn_error`].
//!
//! TodeX's MCP servers reach agy through `session/new`'s `mcpServers`.
//! Earlier TodeX versions drove agy's print mode through a global hook and
//! static MCP entries in `~/.gemini/config`; they are removed once per
//! daemon ([`integration`]).

mod integration;

pub(crate) use integration::{config_dir as integration_dir, remove as remove_integration};

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use serde_json::{json, Value};
use std::path::{Path, PathBuf};
use std::time::Duration;
use tokio::sync::watch;

use crate::config::AgentConfig;
use crate::conversation::ProviderKind;
use crate::error::AppError;

use super::acp::{AcpQuirks, AcpRuntimeOptions, PromptUsage, PLAIN_ACP};
use super::discovery::CatalogCache;
use super::process::{executable_available, run_bounded_command, stderr_excerpt, CommandSpec};
use super::profile::{
    CatalogProfile, CatalogSource, ConfigHome, McpInjection, ProcessModel, ProviderProfile,
    SkillInjection, UserConfigFile,
};
use super::resident::{ResidentLaunch, ResidentSessions};
use super::types::{
    DriverContext, DriverEventSink, DriverPrompt, DriverTurnResult, ImageInputMode,
    PermissionConfigCapabilities, ProviderDescriptor, ProviderDriver, ProviderModelDescriptor,
    ProviderPrewarm,
};

const PROVIDER: &str = "antigravity";

/// What the Antigravity CLI supports and how TodeX adapts to it.
pub(super) const PROFILE: ProviderProfile = ProviderProfile {
    kind: ProviderKind::Antigravity,
    display_name: "Antigravity",
    permission_config: PermissionConfigCapabilities {
        modes: &["ask", "auto", "full-access"],
        default_mode: "auto",
        supports_plan: true,
        supports_replan: false,
        sandbox_modes: &[],
        approval_policies: &[],
        permission_profiles: &[],
        enforcement: "agent-policy",
        description: "agy session modes over ACP. ask runs agy's default mode and asks before what agy asks about (edits, commands, network, other MCP tools); full-access runs the same mode and TodeX approves everything; auto runs agy's own Auto review, which runs what it judges safe and asks about the rest (accounts without agy's Auto run as ask). Plan uses agy's plan mode. TodeX's own MCP tools follow their own approval rules in every mode. Not an operating-system sandbox.",
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
    image_input: true,
    image_input_mode: ImageInputMode::Model,
    mcp_injection: McpInjection::AcpServers,
    skill_injection: SkillInjection::PromptText,
    profile_required: false,
    recovery_full_scan: false,
    process_model: ProcessModel::Resident {
        idle: Some(Duration::from_secs(300)),
        max_sessions: 32,
    },
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

/// Where `agy acp` deviates from plain ACP.
pub(super) const ACP_QUIRKS: AcpQuirks = AcpQuirks {
    full_access_approval: true,
    trusted_permission: Some(todex_mcp_permission),
    auto_mode_fallback: Some("default"),
    turn_error: Some(agy_turn_error),
    // Reattach without replaying history.
    prefer_resume: true,
    recreate_unloadable_session: true,
    prompt_usage: PromptUsage::ResultUsage,
    // Tool calls carry agy's own `name`/`args` next to the schema fields.
    lenient_updates: true,
    command_updates: true,
    usage_updates: true,
    mode_updates: true,
    effort_config: "thought_level",
    session_mode: Some(antigravity_session_mode),
    ..PLAIN_ACP
};

/// agy's session mode for a prompt. Restated every turn: agy resumes a
/// session in `default`, and a resident one keeps the last turn's mode.
fn antigravity_session_mode(prompt: &DriverPrompt) -> Option<&'static str> {
    if prompt.work_mode.as_deref() == Some("plan") {
        return Some("plan");
    }
    Some(match prompt.permission_mode.as_deref() {
        Some("auto") => "auto",
        _ => "default",
    })
}

/// Whether a permission request is for one of TodeX's own MCP tools, which
/// agy names `mcp_<server>_<tool>`. The name does not delimit the server, so
/// a user's own server named like `todex_ssh_<more>` would match too.
fn todex_mcp_permission(params: &Value) -> bool {
    let Some(name) = params.pointer("/toolCall/name").and_then(Value::as_str) else {
        return false;
    };
    crate::agent_mcp::ALL_SERVERS
        .iter()
        .any(|(server, _, _)| name.starts_with(&format!("mcp_{server}_")))
}

/// agy ends a failed turn (quota, sign-in) as `end_turn` without usage, its
/// error streamed as message text behind a warning sign.
fn agy_turn_error(result: &Value, text: &str) -> Option<String> {
    if result.get("usage").is_some() {
        return None;
    }
    let error = text.trim().strip_prefix('\u{26a0}')?.trim();
    (!error.is_empty()).then(|| error.to_owned())
}

/// Effort suffixes `agy models` appends to one model's variants
/// (`gemini-3.8-flash-high`), lowest first.
const EFFORT_SUFFIXES: [&str; 3] = ["low", "medium", "high"];
const MODELS_TIMEOUT: Duration = Duration::from_secs(20);
/// How long a turn waits for the model catalog to pick a variant before
/// passing the selected model id unchanged.
const LAUNCH_CATALOG_TIMEOUT: Duration = Duration::from_secs(2);
const USAGE_TIMEOUT: Duration = Duration::from_secs(20);
const MAX_COMMAND_OUTPUT_BYTES: usize = 256 * 1024;

pub struct AntigravityDriver {
    binary: String,
    /// Daemon variables passed through to `agy`, e.g. `GEMINI_API_KEY`;
    /// provider processes otherwise start from a cleared environment.
    env_allowlist: Vec<String>,
    models: CatalogCache<Vec<ProviderModelDescriptor>>,
    sessions: ResidentSessions,
    /// agy's global config, where earlier TodeX versions put a hook and MCP
    /// entries; `None` leaves it alone (tests).
    legacy_integration: Option<PathBuf>,
    legacy_removed: tokio::sync::OnceCell<()>,
}

impl AntigravityDriver {
    pub fn new(config: &AgentConfig) -> Self {
        Self {
            binary: config.antigravity_bin.clone(),
            env_allowlist: config.antigravity_env_allowlist.clone(),
            models: CatalogCache::new(PROFILE.discovery_cache_ttl),
            sessions: ResidentSessions::new(&PROFILE, "Antigravity")
                .with_idle_timeout(config.provider_keepalive()),
            legacy_integration: (!cfg!(test)).then(integration::config_dir),
            legacy_removed: tokio::sync::OnceCell::new(),
        }
    }

    fn command_spec(&self, cwd: &Path, args: &[&str]) -> CommandSpec {
        command_spec(&self.binary, &self.env_allowlist, cwd, args)
    }

    /// How a conversation's resident process is started: in its workspace,
    /// which agy binds the session to.
    fn resident_launch(
        &self,
        context: &DriverContext,
        uploads: Option<&Path>,
    ) -> Result<ResidentLaunch, AppError> {
        let mut spec = self.command_spec(&context.manifest.workspace, &["acp"]);
        if let Some(uploads) = uploads {
            spec.push_flag_value("--add-dir", &uploads.to_string_lossy())?;
        }
        Ok(ResidentLaunch {
            spec,
            runtime: AcpRuntimeOptions {
                suppress_load_replay: true,
                ..Default::default()
            },
        })
    }

    /// What a prompt asks of agy: the model as agy's variant id, which also
    /// sets its thinking level.
    async fn agy_prompt(&self, workspace: &Path, mut prompt: DriverPrompt) -> DriverPrompt {
        prompt.model = self.launch_model(workspace, &prompt).await;
        prompt.reasoning_effort = None;
        prompt
    }

    /// Removes the hook and MCP entries earlier TodeX versions wrote into
    /// agy's global config, once per daemon. Best effort.
    async fn remove_legacy_integration(&self) {
        let Some(dir) = self.legacy_integration.clone() else {
            return;
        };
        self.legacy_removed
            .get_or_init(|| async move {
                match tokio::task::spawn_blocking(move || integration::remove(&dir)).await {
                    Ok(Ok(true)) => {
                        tracing::info!("removed TodeX's old hook and MCP entries from agy's config")
                    }
                    Ok(Ok(false)) => {}
                    Ok(Err(error)) => tracing::warn!(
                        %error,
                        "could not remove TodeX's old hook and MCP entries from agy's config"
                    ),
                    Err(error) => tracing::warn!(%error, "agy config cleanup did not finish"),
                }
            })
            .await;
    }

    /// The `--model` id for a turn: catalog entries group the effort
    /// variants of one model, so the selected effort picks the variant.
    async fn launch_model(&self, workspace: &Path, prompt: &DriverPrompt) -> Option<String> {
        let model = prompt.model.as_deref()?;
        // A cold or slow catalog must not hold up the turn; the lookup keeps
        // running in the background and serves the next turn.
        let catalog = tokio::time::timeout(LAUNCH_CATALOG_TIMEOUT, self.discover_models(workspace))
            .await
            .unwrap_or_else(|_| {
                Err(AppError::ProviderUnavailable(format!(
                    "agy models took longer than {}s",
                    LAUNCH_CATALOG_TIMEOUT.as_secs()
                )))
            });
        let efforts = match catalog {
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
        let spec = self.command_spec(workspace, &["models"]);
        let fetch = async move {
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
        self.models
            .fetch_or_stale(&self.binary, workspace, fetch)
            .await
    }

    fn discovery_timeout(&self) -> Duration {
        MODELS_TIMEOUT
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
            json!({"provider":PROVIDER,"conversationId":conversation_id,"status":"stopped","reason":"user_closed"}),
        )
    }

    async fn shutdown(&self) {
        self.sessions.stop_all().await;
    }

    async fn prewarm(
        &self,
        context: DriverContext,
        settings: DriverPrompt,
        sink: DriverEventSink,
    ) -> Result<ProviderPrewarm, AppError> {
        self.remove_legacy_integration().await;
        let uploads = crate::prompt_uploads::readable_root().await;
        let settings = self.agy_prompt(&context.manifest.workspace, settings).await;
        self.sessions
            .prewarm(context, settings, sink, |context, _| {
                self.resident_launch(context, uploads.as_deref())
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
        self.remove_legacy_integration().await;
        let uploads = crate::prompt_uploads::readable_root().await;
        let prompt = self.agy_prompt(&context.manifest.workspace, prompt).await;
        let model = prompt.model.clone();
        let result = self
            .sessions
            .run_turn(context, prompt, sink.clone(), cancel, |context, _| {
                self.resident_launch(context, uploads.as_deref())
            })
            .await;
        match result {
            Err(AppError::ProviderUnavailable(detail)) => {
                Err(fail_turn(self, model.as_deref(), &detail, &sink).await)
            }
            other => other,
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
                    // gpt-oss is text-only; the Gemini and Claude models
                    // agy offers read images.
                    image_input: Some(!base.starts_with("gpt-oss")),
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
    fn only_gpt_oss_models_lack_image_input() {
        let models = parse_models(MODELS);
        let images: Vec<_> = models
            .iter()
            .map(|model| (model.id.as_str(), model.image_input))
            .collect();
        assert_eq!(
            images,
            [
                ("gemini-3.8-flash", Some(true)),
                ("gemini-3.1-pro", Some(true)),
                ("claude-opus-4-6-thinking", Some(true)),
                ("gpt-oss-120b", Some(false)),
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
    fn only_todex_mcp_calls_are_trusted() {
        let request = |name: &str| json!({"toolCall": {"name": name}});
        assert!(todex_mcp_permission(&request(
            "mcp_todex_ssh_ssh_list_hosts"
        )));
        assert!(todex_mcp_permission(&request(
            "mcp_todex_agents_agents_call"
        )));
        assert!(!todex_mcp_permission(&request(
            "mcp_context7_resolve-library-id"
        )));
        assert!(!todex_mcp_permission(&request("run_command")));
        assert!(!todex_mcp_permission(&json!({"toolCall": {}})));
    }

    #[test]
    fn a_quota_error_streamed_as_text_fails_the_turn() {
        let text = "\n\u{26a0} Individual quota reached. Resets in 1h.\n";
        assert_eq!(
            agy_turn_error(&json!({"stopReason": "end_turn"}), text).as_deref(),
            Some("Individual quota reached. Resets in 1h.")
        );
        // A real answer reports usage, even one that mentions a warning sign.
        assert_eq!(
            agy_turn_error(&json!({"stopReason": "end_turn", "usage": {}}), text),
            None
        );
        assert_eq!(
            agy_turn_error(&json!({"stopReason": "end_turn"}), "Hi"),
            None
        );
    }

    #[test]
    fn session_modes_follow_the_permission_and_work_modes() {
        let prompt = |permission: &str, work: &str| DriverPrompt {
            turn_id: "turn".to_owned(),
            text: String::new(),
            content: vec![],
            skills: vec![],
            model: None,
            reasoning_effort: None,
            permission_mode: Some(permission.to_owned()),
            work_mode: Some(work.to_owned()),
            permission_profile: None,
            sandbox_mode: None,
            approval_policy: None,
            after_plan: None,
            replan: None,
        };
        let mode = |permission, work| antigravity_session_mode(&prompt(permission, work));
        assert_eq!(mode("ask", "implement"), Some("default"));
        assert_eq!(mode("full-access", "implement"), Some("default"));
        assert_eq!(mode("auto", "implement"), Some("auto"));
        assert_eq!(mode("auto", "plan"), Some("plan"));
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
        use crate::provider::types::{DriverPromptContent, PermissionBroker};

        struct Fixture {
            root: PathBuf,
            driver: AntigravityDriver,
            store: ConversationStore,
            manifest: ConversationManifest,
            /// TodeX's agent MCP, for turns that get TodeX's MCP servers.
            bridge: Option<crate::agent_mcp::AgentMcp>,
        }

        impl Fixture {
            async fn new() -> Self {
                use std::os::unix::fs::PermissionsExt;
                let root = std::env::temp_dir()
                    .join(format!("todex-agy-acp-{}", uuid::Uuid::new_v4().simple()));
                std::fs::create_dir_all(root.join("workspace")).unwrap();
                let root = std::fs::canonicalize(root).unwrap();
                let binary = root.join("agy-fixture");
                std::fs::write(
                    &binary,
                    include_str!("../../tests/fixtures/agy_acp_fixture.py"),
                )
                .unwrap();
                std::fs::set_permissions(&binary, std::fs::Permissions::from_mode(0o755)).unwrap();
                let driver = AntigravityDriver {
                    binary: binary.to_string_lossy().into_owned(),
                    env_allowlist: Vec::new(),
                    models: CatalogCache::new(None),
                    sessions: ResidentSessions::new(&PROFILE, "Antigravity"),
                    legacy_integration: Some(root.join("gemini-config")),
                    legacy_removed: tokio::sync::OnceCell::new(),
                };
                let store = ConversationStore::new(root.join("data")).await.unwrap();
                let manifest = store
                    .create(ConversationManifest::new(
                        ProviderKind::Antigravity,
                        root.join("workspace"),
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
                    bridge: None,
                }
            }

            /// Turns get TodeX's MCP servers and tool modes, as when agent
            /// MCP is on.
            async fn with_bridge() -> Self {
                let mut fixture = Self::new().await;
                let data = fixture.root.join("bridge");
                let ssh = crate::ssh::SshService::with_home(&data, "ssh".into(), None)
                    .await
                    .unwrap();
                ssh.set_agent_local(crate::ssh::AgentLocal { enabled: true })
                    .await
                    .unwrap();
                let desktop = crate::agent_desktop::AgentDesktop::load(&data)
                    .await
                    .unwrap();
                let bridge = crate::agent_mcp::AgentMcp::with_bridge_command(
                    &data,
                    ssh,
                    desktop,
                    Some(PathBuf::from("/opt/todex/todex-agentd")),
                )
                .await
                .unwrap();
                bridge.set_listen_addr("127.0.0.1:7345".parse().unwrap());
                fixture.bridge = Some(bridge);
                fixture
            }

            fn mark(&self, name: &str) {
                std::fs::write(self.root.join(name), "").unwrap();
            }

            fn prompt(
                &self,
                turn_id: &str,
                text: &str,
                permission: &str,
                work: &str,
            ) -> DriverPrompt {
                DriverPrompt {
                    turn_id: turn_id.to_owned(),
                    text: text.to_owned(),
                    content: vec![],
                    skills: vec![],
                    model: Some("gemini-3.8-flash".to_owned()),
                    reasoning_effort: Some("low".to_owned()),
                    permission_mode: Some(permission.to_owned()),
                    work_mode: Some(work.to_owned()),
                    permission_profile: None,
                    sandbox_mode: None,
                    approval_policy: None,
                    after_plan: None,
                    replan: None,
                }
            }

            async fn context(&self, prompt: &DriverPrompt) -> DriverContext {
                let mut handles = crate::provider::types::TurnHandles {
                    mode: None,
                    ..Default::default()
                };
                let mut agent_mcp = None;
                if let Some(bridge) = &self.bridge {
                    // What the supervisor does as the turn starts.
                    bridge.record_turn_mode(
                        &self.manifest.id,
                        prompt.permission_mode.as_deref().unwrap(),
                        prompt.work_mode.as_deref().unwrap(),
                    );
                    agent_mcp = bridge.launch(&self.manifest.id).await;
                    handles.mode = Some(crate::provider::types::TurnModeHandle {
                        agent_mcp: bridge.clone(),
                        conversation_id: self.manifest.id.clone(),
                    });
                }
                DriverContext {
                    manifest: self.manifest.clone(),
                    provider_state: self.store.provider_state(&self.manifest.id).await.unwrap(),
                    agent_mcp,
                    handles,
                }
            }

            fn sink(&self, turn_id: &str) -> DriverEventSink {
                DriverEventSink::new(
                    self.store.clone(),
                    ConversationEventHub::default(),
                    PermissionBroker::default(),
                    &self.manifest.id,
                )
                .with_turn_id(turn_id)
            }

            async fn run_prompt(&self, prompt: DriverPrompt) -> Result<DriverTurnResult, AppError> {
                let context = self.context(&prompt).await;
                let sink = self.sink(&prompt.turn_id);
                let (_cancel, receiver) = watch::channel(false);
                tokio::time::timeout(
                    Duration::from_secs(60),
                    self.driver.run_turn(context, prompt, sink, receiver),
                )
                .await
                .expect("the turn ends")
            }

            async fn run(
                &self,
                turn_id: &str,
                text: &str,
                permission: &str,
                work: &str,
            ) -> Result<DriverTurnResult, AppError> {
                self.run_prompt(self.prompt(turn_id, text, permission, work))
                    .await
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

            async fn provider_events(&self, method: &str) -> Vec<Value> {
                self.events()
                    .await
                    .into_iter()
                    .filter(|(kind, payload)| {
                        kind == "provider.event" && payload["providerMethod"] == method
                    })
                    .map(|(_, payload)| payload)
                    .collect()
            }

            fn requests(&self) -> Vec<Value> {
                std::fs::read_to_string(self.root.join("agy-requests.jsonl"))
                    .unwrap_or_default()
                    .lines()
                    .filter_map(|line| serde_json::from_str(line).ok())
                    .collect()
            }

            /// `(configId, value)` of every `session/set_config_option`.
            fn config_writes(&self) -> Vec<(String, String)> {
                self.requests()
                    .into_iter()
                    .filter(|request| request["method"] == "session/set_config_option")
                    .map(|request| {
                        (
                            request["params"]["configId"].as_str().unwrap().to_owned(),
                            request["params"]["value"].as_str().unwrap().to_owned(),
                        )
                    })
                    .collect()
            }

            fn launches(&self) -> Vec<Vec<String>> {
                std::fs::read_to_string(self.root.join("argv.log"))
                    .unwrap_or_default()
                    .lines()
                    .map(|line| serde_json::from_str::<Vec<String>>(line).unwrap())
                    .filter(|argv| argv.first().map(String::as_str) == Some("acp"))
                    .collect()
            }

            async fn finish(self) {
                self.driver.shutdown().await;
                std::fs::remove_dir_all(self.root).unwrap();
            }
        }

        #[tokio::test]
        async fn a_turn_streams_text_and_tools_and_the_next_reuses_the_process() {
            let fixture = Fixture::new().await;
            let result = fixture
                .run("turn-1", "hello", "ask", "implement")
                .await
                .unwrap();
            assert_eq!(result.stop_reason, "end_turn");
            assert_eq!(result.native_session_id.as_deref(), Some("agy-session-1"));
            let events = fixture.events().await;
            let text: String = events
                .iter()
                .filter(|(kind, _)| kind == "message.delta")
                .filter_map(|(_, payload)| payload.pointer("/content/text").and_then(Value::as_str))
                .collect();
            assert_eq!(text, "Hi there");
            let tool = events
                .iter()
                .find(|(kind, _)| kind == "tool.started")
                .map(|(_, payload)| payload)
                .expect("a tool call");
            assert_eq!(tool["toolName"], "run_command");
            assert_eq!(tool["arguments"]["CommandLine"], "true");
            assert!(events
                .iter()
                .any(|(kind, payload)| kind == "tool.updated" && payload["status"] == "completed"));
            assert!(events.iter().any(|(kind, _)| kind == "usage.updated"));

            fixture
                .run("turn-2", "again", "ask", "implement")
                .await
                .unwrap();
            assert_eq!(fixture.launches().len(), 1, "one resident process");
            let requests = fixture.requests();
            // agy binds the session to the process' working directory.
            assert!(requests
                .iter()
                .all(|request| request["cwd"] == fixture.root.join("workspace").to_str().unwrap()));
            assert_eq!(
                requests
                    .iter()
                    .filter(|request| request["method"] == "session/new")
                    .count(),
                1
            );
            fixture.finish().await;
        }

        #[tokio::test]
        async fn a_new_process_resumes_the_session() {
            let fixture = Fixture::new().await;
            fixture
                .run("turn-1", "hello", "ask", "implement")
                .await
                .unwrap();
            fixture.driver.shutdown_session(&fixture.manifest.id).await;
            fixture
                .run("turn-2", "again", "ask", "implement")
                .await
                .unwrap();
            let methods: Vec<_> = fixture
                .requests()
                .into_iter()
                .filter_map(|request| request["method"].as_str().map(str::to_owned))
                .filter(|method| {
                    method.starts_with("session/") && method != "session/set_config_option"
                })
                .collect();
            assert_eq!(
                methods,
                [
                    "session/new",
                    "session/prompt",
                    "session/resume",
                    "session/prompt"
                ]
            );
            fixture.finish().await;
        }

        #[tokio::test]
        async fn the_model_and_effort_select_agys_variant_and_modes_follow_the_turn() {
            let fixture = Fixture::new().await;
            fixture
                .run("turn-1", "hello", "ask", "implement")
                .await
                .unwrap();
            fixture
                .run("turn-2", "hello", "auto", "implement")
                .await
                .unwrap();
            fixture
                .run("turn-3", "hello", "full-access", "plan")
                .await
                .unwrap();
            fixture
                .run("turn-4", "hello", "full-access", "implement")
                .await
                .unwrap();
            let writes = fixture.config_writes();
            assert!(writes.contains(&("model".to_owned(), "gemini-3.8-flash-low".to_owned())));
            assert!(!writes.iter().any(|(id, _)| id == "thought_level"));
            let modes: Vec<_> = writes
                .iter()
                .filter(|(id, _)| id == "mode")
                .map(|(_, value)| value.as_str())
                .collect();
            assert_eq!(modes, ["auto", "plan", "default"]);
            fixture.finish().await;
        }

        #[tokio::test]
        async fn auto_without_agys_auto_runs_as_ask() {
            use crate::agent_mcp::ToolMode;
            let fixture = Fixture::with_bridge().await;
            fixture.mark("no-auto");
            fixture
                .run("turn-1", "hello", "auto", "implement")
                .await
                .unwrap();
            assert_eq!(
                fixture.provider_events("auto_mode_unavailable").await.len(),
                1
            );
            assert!(!fixture
                .config_writes()
                .iter()
                .any(|(id, value)| id == "mode" && value == "auto"));
            let bridge = fixture.bridge.as_ref().unwrap();
            assert_eq!(
                bridge.authorizer_state().mode(&fixture.manifest.id),
                Some(ToolMode::Ask)
            );
            fixture.finish().await;
        }

        #[tokio::test]
        async fn todex_mcp_servers_go_to_the_session_and_their_calls_are_approved() {
            let fixture = Fixture::with_bridge().await;
            let result = fixture
                .run("turn-1", "TODEX_MCP", "ask", "implement")
                .await
                .unwrap();
            assert_eq!(result.stop_reason, "end_turn");
            let requests = fixture.requests();
            let new = requests
                .iter()
                .find(|request| request["method"] == "session/new")
                .unwrap();
            let servers: Vec<_> = new["params"]["mcpServers"]
                .as_array()
                .unwrap()
                .iter()
                .map(|server| server["name"].as_str().unwrap())
                .collect();
            assert!(servers.contains(&"todex_ssh"), "{servers:?}");
            assert!(requests.iter().any(|request| request["id"] == 99
                && request["result"]["outcome"]["optionId"] == "allow"));
            let approved = fixture.events().await.into_iter().any(|(kind, payload)| {
                kind == "permission.resolved" && payload["autoApproved"] == true
            });
            assert!(approved);
            fixture.finish().await;
        }

        #[tokio::test]
        async fn full_access_approves_and_ask_asks() {
            let fixture = Fixture::new().await;
            fixture
                .run("turn-1", "PERMISSION", "full-access", "implement")
                .await
                .unwrap();
            assert!(fixture.requests().iter().any(|request| request["id"] == 99
                && request["result"]["outcome"]["optionId"] == "allow"));

            let prompt = fixture.prompt("turn-2", "PERMISSION", "ask", "implement");
            let context = fixture.context(&prompt).await;
            let sink = fixture.sink("turn-2");
            let (cancel, receiver) = watch::channel(false);
            let turn = fixture.driver.run_turn(context, prompt, sink, receiver);
            let asked = async {
                loop {
                    let pending = fixture
                        .events()
                        .await
                        .into_iter()
                        .filter(|(kind, _)| kind == "permission.requested")
                        .count();
                    if pending == 2 {
                        break;
                    }
                    tokio::time::sleep(Duration::from_millis(50)).await;
                }
                cancel.send(true).unwrap();
            };
            let (result, ()) =
                tokio::time::timeout(Duration::from_secs(20), async { tokio::join!(turn, asked) })
                    .await
                    .expect("the turn ends");
            assert!(result.unwrap().cancelled);
            fixture.finish().await;
        }

        #[tokio::test]
        async fn a_quota_error_fails_the_turn_and_records_the_window() {
            let fixture = Fixture::new().await;
            fixture.mark("quota-exhausted");
            let error = fixture
                .run("turn-1", "QUOTA", "ask", "implement")
                .await
                .unwrap_err()
                .to_string();
            assert!(error.contains("Individual quota reached"), "{error}");
            let quota = fixture
                .events()
                .await
                .into_iter()
                .find(|(kind, _)| kind == "quota.updated")
                .map(|(_, payload)| payload)
                .expect("quota.updated");
            assert_eq!(quota["status"], "rejected");
            fixture.finish().await;
        }

        #[tokio::test]
        async fn an_error_response_fails_the_turn_with_its_message() {
            let fixture = Fixture::new().await;
            let error = fixture
                .run("turn-1", "FAIL model exploded", "ask", "implement")
                .await
                .unwrap_err()
                .to_string();
            assert!(error.contains("model exploded"), "{error}");
            fixture.finish().await;
        }

        #[tokio::test]
        async fn images_go_as_content_blocks() {
            use base64::Engine as _;
            let fixture = Fixture::new().await;
            let mut prompt = fixture.prompt("turn-1", "IMAGE", "ask", "implement");
            prompt.content = vec![DriverPromptContent::Image {
                path: None,
                data: base64::engine::general_purpose::STANDARD.encode(b"png bytes"),
                mime_type: "image/png".to_owned(),
            }];
            fixture.run_prompt(prompt).await.unwrap();
            let text: String = fixture
                .events()
                .await
                .iter()
                .filter(|(kind, _)| kind == "message.delta")
                .filter_map(|(_, payload)| payload.pointer("/content/text").and_then(Value::as_str))
                .map(str::to_owned)
                .collect();
            assert_eq!(text, "images: 1");
            fixture.finish().await;
        }

        #[tokio::test]
        async fn cancel_ends_a_running_turn_and_keeps_the_process() {
            let fixture = Fixture::new().await;
            let prompt = fixture.prompt("turn-1", "HANG", "ask", "implement");
            let context = fixture.context(&prompt).await;
            let sink = fixture.sink("turn-1");
            let (cancel, receiver) = watch::channel(false);
            let turn = fixture.driver.run_turn(context, prompt, sink, receiver);
            let stop = async {
                while !fixture
                    .requests()
                    .iter()
                    .any(|request| request["method"] == "session/prompt")
                {
                    tokio::time::sleep(Duration::from_millis(50)).await;
                }
                cancel.send(true).unwrap();
            };
            let (result, ()) =
                tokio::time::timeout(Duration::from_secs(20), async { tokio::join!(turn, stop) })
                    .await
                    .expect("the turn ends");
            assert!(result.unwrap().cancelled);
            fixture
                .run("turn-2", "again", "ask", "implement")
                .await
                .unwrap();
            assert_eq!(fixture.launches().len(), 1);
            fixture.finish().await;
        }

        #[tokio::test]
        async fn the_old_hook_and_mcp_entries_are_removed() {
            let fixture = Fixture::new().await;
            let dir = fixture.root.join("gemini-config");
            std::fs::create_dir_all(&dir).unwrap();
            std::fs::write(
                dir.join("hooks.json"),
                r#"{"todex-approval": {"PreToolUse": []}, "mine": {"Stop": []}}"#,
            )
            .unwrap();
            std::fs::write(
                dir.join("mcp_config.json"),
                r#"{"mcpServers": {"todex_ssh": {"command": "x"}, "mine": {"command": "y"}}}"#,
            )
            .unwrap();
            fixture
                .run("turn-1", "hello", "ask", "implement")
                .await
                .unwrap();
            let hooks: Value =
                serde_json::from_str(&std::fs::read_to_string(dir.join("hooks.json")).unwrap())
                    .unwrap();
            assert_eq!(hooks, json!({"mine": {"Stop": []}}));
            let mcp: Value = serde_json::from_str(
                &std::fs::read_to_string(dir.join("mcp_config.json")).unwrap(),
            )
            .unwrap();
            assert_eq!(mcp, json!({"mcpServers": {"mine": {"command": "y"}}}));
            fixture.finish().await;
        }

        /// Runs real `agy acp` turns: `TODEX_LIVE_AGY=1`, a signed-in `agy`
        /// (`TODEX_LIVE_AGY_BIN`, default `agy` on PATH), and a model with
        /// quota (`TODEX_LIVE_AGY_MODEL`, default `gemini-3.8-flash`).
        #[tokio::test]
        #[ignore = "opt-in: runs the real Antigravity CLI and spends quota"]
        async fn live_agy_turns_resume() {
            if std::env::var("TODEX_LIVE_AGY").as_deref() != Ok("1") {
                return;
            }
            let mut fixture = Fixture::new().await;
            fixture.driver.binary =
                std::env::var("TODEX_LIVE_AGY_BIN").unwrap_or_else(|_| "agy".to_owned());
            let model = std::env::var("TODEX_LIVE_AGY_MODEL")
                .unwrap_or_else(|_| "gemini-3.8-flash".to_owned());
            let mut prompt =
                fixture.prompt("turn-1", "Reply with exactly: pong", "ask", "implement");
            prompt.model = Some(model.clone());
            let first = fixture.run_prompt(prompt).await.unwrap();
            assert_eq!(first.stop_reason, "end_turn");
            fixture.driver.shutdown_session(&fixture.manifest.id).await;
            let mut prompt =
                fixture.prompt("turn-2", "Reply with exactly: ping", "ask", "implement");
            prompt.model = Some(model);
            let second = fixture.run_prompt(prompt).await.unwrap();
            assert_eq!(second.native_session_id, first.native_session_id);
            fixture.finish().await;
        }
    }
}
