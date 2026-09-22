//! Codex CLI live config adapter.
//!
//! Exclusive mode. A provider's `settingsConfig` is `{auth, config}` where
//! `auth` maps to `~/.codex/auth.json` (or `null`/absent to remove it) and
//! `config` is the full `config.toml` text — the same shape cc-switch stores.

use std::path::PathBuf;

use serde_json::Value;
use toml_edit::DocumentMut;

use super::auth_config::{self, PairedFiles};
use super::AgentDirs;
use crate::error::AppError;

/// `http_headers` holds literal header values; `env_http_headers` holds
/// variable names and stays readable.
pub const HEADER_TABLES: &[&str] = &["http_headers"];

pub fn config_path(dirs: &AgentDirs) -> PathBuf {
    dirs.codex_home.join("config.toml")
}

pub fn auth_path(dirs: &AgentDirs) -> PathBuf {
    dirs.codex_home.join("auth.json")
}

fn paired_files(dirs: &AgentDirs) -> PairedFiles {
    PairedFiles {
        config: config_path(dirs),
        auth: auth_path(dirs),
        agent_label: "Codex",
    }
}

/// Live state normalized as `{auth: <object|null>, config: <toml text|null>}`;
/// `None` when neither file exists.
pub fn read_live(dirs: &AgentDirs) -> Result<Option<Value>, AppError> {
    auth_config::read_live(&paired_files(dirs))
}

/// Write the provider projection. `auth` absent/`null` deletes `auth.json`
/// when `remove_auth` is set; `config` must be valid TOML when present.
pub fn write_live(dirs: &AgentDirs, settings: &Value, remove_auth: bool) -> Result<(), AppError> {
    auth_config::write_live(&paired_files(dirs), settings, remove_auth)
}

/// Extract `(base_url, api_key)` for the model-listing proxy call.
pub fn endpoint_credentials(settings: &Value) -> (Option<String>, Option<String>) {
    let base_url = settings
        .get("config")
        .and_then(Value::as_str)
        .and_then(|text| text.parse::<DocumentMut>().ok())
        .and_then(|doc| {
            let provider_id = doc
                .get("model_provider")
                .and_then(|item| item.as_str())
                .map(str::to_owned)?;
            doc.get("model_providers")
                .and_then(|item| item.get(&provider_id))
                .and_then(|item| item.get("base_url"))
                .and_then(|item| item.as_str())
                .map(str::to_owned)
        });
    let api_key = settings
        .get("auth")
        .and_then(|auth| auth.get("OPENAI_API_KEY"))
        .and_then(Value::as_str)
        .map(str::to_owned);
    (base_url, api_key)
}

/// Return a copy with secrets in both the `auth` object and the `config` TOML
/// text replaced by the mask sentinel.
pub fn masked_settings(settings: &Value) -> Value {
    auth_config::masked_settings(settings, HEADER_TABLES)
}
