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
