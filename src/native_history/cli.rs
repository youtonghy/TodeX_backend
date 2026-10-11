//! The provider CLIs native history drives for Codex, Grok Build and Devin:
//! each deletes its own sessions (`codex delete --force`, `grok sessions
//! delete`, `devin rm --force`), and Devin's local sessions are listed
//! through ACP `session/list` on a short-lived `devin acp`. TodeX never
//! removes these providers' files itself.

use std::{
    path::{Path, PathBuf},
    time::Duration,
};

use serde_json::{json, Value};
use tokio::time::Instant;

use crate::{
    config::AgentConfig,
    error::AppError,
    provider::{
        devin_cli_environment, grok_cli_environment,
        process::{
            executable_available, redact_sensitive_text, run_bounded_command, CommandSpec,
            JsonLineProcess,
        },
    },
};

use super::{parse::cap, NativeProvider};

/// A delete may reach a remote service (Grok deletes its cloud copy first).
const DELETE_TIMEOUT: Duration = Duration::from_secs(60);
/// The whole Devin listing exchange, start-up included.
const LIST_TIMEOUT: Duration = Duration::from_secs(30);
const MAX_DELETE_OUTPUT_BYTES: usize = 64 * 1024;
/// `session/list` pages (Devin serves 50 sessions a page).
const MAX_LIST_PAGES: usize = 400;
/// Longest CLI failure text reported in a skipped item.
const FAILURE_CHARS: usize = 300;

/// The provider binaries and environment allowlists, from the agent config.
#[derive(Clone, Debug)]
pub struct NativeClis {
    pub codex: String,
    pub grok: String,
    pub devin: String,
    pub grok_env_allowlist: Vec<String>,
    pub devin_env_allowlist: Vec<String>,
}

impl Default for NativeClis {
    fn default() -> Self {
        Self {
            codex: "codex".to_owned(),
            grok: "grok".to_owned(),
            devin: "devin".to_owned(),
            grok_env_allowlist: Vec::new(),
            devin_env_allowlist: Vec::new(),
        }
    }
}

impl NativeClis {
    pub fn from_config(config: &AgentConfig) -> Self {
        Self {
            codex: config.codex_bin.clone(),
            grok: config.grok_bin.clone(),
            devin: config.devin_bin.clone(),
            grok_env_allowlist: config.grok_env_allowlist.clone(),
            devin_env_allowlist: config.devin_env_allowlist.clone(),
        }
    }

    /// `codex delete --force <id>` with `CODEX_HOME` pinned to the home the
    /// listing read. `--force` skips the prompt and requires a UUID.
    pub(super) fn codex_delete(&self, codex_home: &Path, id: &str) -> CommandSpec {
        let mut spec = CommandSpec::new(&self.codex, working_dir(codex_home));
        spec.args = vec!["delete".to_owned(), "--force".to_owned(), id.to_owned()];
        spec.env
            .insert("CODEX_HOME".to_owned(), codex_home.display().to_string());
        spec
    }

    /// `grok --no-auto-update sessions delete <id>` (never prompts) with
    /// `GROK_HOME` pinned to the home the listing read.
    pub(super) fn grok_delete(&self, grok_home: &Path, id: &str) -> CommandSpec {
        let mut spec = CommandSpec::new(&self.grok, working_dir(grok_home));
        spec.args = vec![
            "--no-auto-update".to_owned(),
            "sessions".to_owned(),
            "delete".to_owned(),
            id.to_owned(),
        ];
        spec.env = grok_cli_environment(&self.grok_env_allowlist);
        spec.env
            .insert("GROK_HOME".to_owned(), grok_home.display().to_string());
        if let Some(auth) = std::env::var_os("GROK_AUTH_PATH").filter(|value| !value.is_empty()) {
            spec.env.insert(
                "GROK_AUTH_PATH".to_owned(),
                auth.to_string_lossy().into_owned(),
            );
        }
        spec
    }

    /// `devin rm --force <id>`; Devin finds its data under `XDG_DATA_HOME`
    /// (forwarded to every provider process).
    pub(super) fn devin_delete(&self, devin_dir: &Path, id: &str) -> CommandSpec {
        let mut spec = CommandSpec::new(&self.devin, working_dir(devin_dir));
        spec.args = vec!["rm".to_owned(), "--force".to_owned(), id.to_owned()];
        spec.env = devin_cli_environment(&self.devin_env_allowlist);
        spec
    }

    fn devin_acp(&self, devin_dir: &Path) -> CommandSpec {
        let mut spec = CommandSpec::new(&self.devin, working_dir(devin_dir));
        spec.args = vec!["acp".to_owned()];
        spec.env = devin_cli_environment(&self.devin_env_allowlist);
        spec
    }
}

/// `dir` when it exists, else the system temp directory: CLI commands need
/// an existing absolute working directory and do not depend on it.
fn working_dir(dir: &Path) -> PathBuf {
    if dir.is_absolute() && dir.is_dir() {
        dir.to_owned()
    } else {
        std::env::temp_dir()
    }
}

/// Runs a delete command. Success is a zero exit (and, for Grok, not its
/// "No session found" notice); a failure becomes the error the cleanup
/// report skips the session with.
pub(super) async fn run_delete(
    provider: NativeProvider,
    spec: &CommandSpec,
) -> Result<(), AppError> {
    if !executable_available(&spec.program) {
        return Err(AppError::Unsupported(format!(
            "the {} CLI ('{}') is not installed; TodeX deletes {} sessions only through it",
            provider.display_name(),
            spec.program,
            provider.display_name(),
        )));
    }
    let output = run_bounded_command(spec, MAX_DELETE_OUTPUT_BYTES, DELETE_TIMEOUT).await?;
    let stdout = String::from_utf8_lossy(&output.stdout);
    if output.success {
        if provider == NativeProvider::GrokBuild && stdout.contains("No session found") {
            return Err(AppError::NotFound(
                "grok found no session with this id".to_owned(),
            ));
        }
        return Ok(());
    }
    Err(classify_failure(
        provider,
        &failure_text(&String::from_utf8_lossy(&output.stderr), &stdout),
    ))
}

/// The line that explains a CLI failure: the last `Error: …` line of stderr
/// (then stdout), else the last non-empty line; redacted and shortened.
pub(super) fn failure_text(stderr: &str, stdout: &str) -> String {
    let lines: Vec<&str> = stderr
        .lines()
        .chain(stdout.lines())
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .collect();
    let line = lines
        .iter()
        .rev()
        .find_map(|line| {
            line.strip_prefix("Error:")
                .or_else(|| line.strip_prefix("error:"))
        })
        .map(str::trim)
        .or_else(|| lines.last().copied())
        .unwrap_or("exited with an error");
    cap(&redact_sensitive_text(line), FAILURE_CHARS)
}

fn classify_failure(provider: NativeProvider, text: &str) -> AppError {
    let lower = text.to_ascii_lowercase();
    if lower.contains("no session found") || lower.contains("not found") {
        AppError::NotFound(text.to_owned())
    } else if lower.contains("already open") || lower.contains("another process") {
        AppError::Conflict(text.to_owned())
    } else {
        AppError::ProviderUnavailable(format!(
            "{} could not delete the session: {text}",
            provider.display_name()
        ))
    }
}

/// Devin's local sessions: ACP `initialize`, then every `session/list` page
/// (`sessionId`, `cwd`, `title`, `updatedAt`, `_meta` with
/// `cognition.ai/createdAt` and `cognition.ai/isLocked`). Needs no login.
pub(super) async fn devin_sessions(
    clis: &NativeClis,
    devin_dir: &Path,
) -> Result<Vec<Value>, AppError> {
    let mut process = JsonLineProcess::spawn(&clis.devin_acp(devin_dir)).await?;
    let deadline = Instant::now() + LIST_TIMEOUT;
    let result = async {
        let initialize = request(
            &mut process,
            0,
            "initialize",
            json!({
                "protocolVersion": 1,
                "clientCapabilities": {
                    "fs": {"readTextFile": false, "writeTextFile": false},
                    "terminal": false,
                },
                "clientInfo": {"name": "todex-agentd", "version": crate::version::APP_VERSION},
            }),
            deadline,
        )
        .await?;
        if initialize
            .pointer("/agentCapabilities/sessionCapabilities/list")
            .is_none()
        {
            return Err(AppError::Unsupported(
                "this devin CLI does not support ACP session/list".to_owned(),
            ));
        }
        let mut sessions = Vec::new();
        let mut cursor: Option<String> = None;
        for page in 1..=MAX_LIST_PAGES {
            let params = match &cursor {
                Some(cursor) => json!({ "cursor": cursor }),
                None => json!({}),
            };
            let result =
                request(&mut process, page as u64, "session/list", params, deadline).await?;
            if let Some(items) = result.get("sessions").and_then(Value::as_array) {
                sessions.extend(items.iter().cloned());
            }
            cursor = result
                .get("nextCursor")
                .and_then(Value::as_str)
                .filter(|next| !next.is_empty())
                .map(str::to_owned);
            if cursor.is_none() {
                break;
            }
        }
        Ok(sessions)
    }
    .await;
    process.terminate().await;
    result
}

async fn request(
    process: &mut JsonLineProcess,
    id: u64,
    method: &str,
    params: Value,
    deadline: Instant,
) -> Result<Value, AppError> {
    process
        .send(&json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params}))
        .await?;
    loop {
        let Some(frame) = process.read_control_until(deadline).await? else {
            return Err(AppError::ProviderUnavailable(format!(
                "devin acp exited before answering {method}"
            )));
        };
        if frame.get("method").is_some() {
            // A request from the agent: decline it; notifications are ignored.
            if let Some(request_id) = frame.get("id") {
                process
                    .send(&json!({
                        "jsonrpc": "2.0",
                        "id": request_id,
                        "error": {"code": -32601, "message": "method not supported"},
                    }))
                    .await?;
            }
            continue;
        }
        if frame.get("id").and_then(Value::as_u64) != Some(id) {
            continue;
        }
        if let Some(error) = frame.get("error") {
            let message = error
                .get("message")
                .and_then(Value::as_str)
                .unwrap_or("request failed");
            return Err(AppError::ProviderUnavailable(format!(
                "devin acp {method} failed: {}",
                cap(&redact_sensitive_text(message), FAILURE_CHARS)
            )));
        }
        return Ok(frame.get("result").cloned().unwrap_or(Value::Null));
    }
}
