//! Claude Code live config adapter.
//!
//! Exclusive mode: the whole `settings.json` is one provider's projection,
//! like cc-switch. `env` entries inside it carry `ANTHROPIC_BASE_URL`,
//! `ANTHROPIC_AUTH_TOKEN`, `ANTHROPIC_MODEL`, and friends.

use std::path::PathBuf;

use serde_json::Value;

use super::files;
use crate::error::AppError;

use super::AgentDirs;

pub fn settings_path(dirs: &AgentDirs) -> PathBuf {
    dirs.claude_dir.join("settings.json")
}

/// The live settings as one `settingsConfig` blob; `None` when no file exists.
pub fn read_live(dirs: &AgentDirs) -> Result<Option<Value>, AppError> {
    let path = settings_path(dirs);
    let Some(value) = files::read_json5(&path, "Claude settings")? else {
        return Ok(None);
    };
    files::ensure_object(&value, &path, "Claude settings")?;
    Ok(Some(value))
}

pub fn write_live(dirs: &AgentDirs, settings: &Value) -> Result<(), AppError> {
    let path = settings_path(dirs);
    files::ensure_object(settings, &path, "Claude provider settings")?;
    files::write_json_pretty(&path, settings)
}

/// `(base_url, api_key)` from the `env` block; `ANTHROPIC_AUTH_TOKEN` wins
/// over `ANTHROPIC_API_KEY`, as in Claude Code itself.
pub fn endpoint_credentials(settings: &Value) -> (Option<String>, Option<String>) {
    let env = settings.get("env");
    (
        env.and_then(|env| env.get("ANTHROPIC_BASE_URL"))
            .and_then(Value::as_str)
            .map(str::to_owned),
        env.and_then(|env| {
            env.get("ANTHROPIC_AUTH_TOKEN")
                .or_else(|| env.get("ANTHROPIC_API_KEY"))
        })
        .and_then(Value::as_str)
        .map(str::to_owned),
    )
}

pub(super) struct Projection;

impl super::ExclusiveProjection for Projection {
    fn read_live(&self, dirs: &AgentDirs) -> Result<Option<Value>, AppError> {
        read_live(dirs)
    }

    fn write_live(
        &self,
        dirs: &AgentDirs,
        settings: &Value,
        _remove_auth: bool,
    ) -> Result<(), AppError> {
        write_live(dirs, settings)
    }

    fn live_matches(&self, live: &Value, settings: &Value) -> bool {
        live == settings
    }

    fn endpoint_credentials(&self, settings: &Value) -> (Option<String>, Option<String>) {
        endpoint_credentials(settings)
    }
}
