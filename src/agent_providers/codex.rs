//! Codex CLI live config adapter.
//!
//! Exclusive mode. A provider's `settingsConfig` is `{auth, config}` where
//! `auth` maps to `~/.codex/auth.json` (or `null`/absent to remove it) and
//! `config` is the full `config.toml` text — the same shape cc-switch stores.

use std::path::PathBuf;

use serde_json::{json, Value};
use toml_edit::DocumentMut;

use super::files;
use super::{is_sensitive_key, AgentDirs, MASKED_SECRET};
use crate::error::AppError;

pub fn config_path(dirs: &AgentDirs) -> PathBuf {
    dirs.codex_home.join("config.toml")
}

pub fn auth_path(dirs: &AgentDirs) -> PathBuf {
    dirs.codex_home.join("auth.json")
}

/// Live state normalized as `{auth: <object|null>, config: <toml text|null>}`;
/// `None` when neither file exists.
pub fn read_live(dirs: &AgentDirs) -> Result<Option<Value>, AppError> {
    let config = files::read_text(&config_path(dirs), "Codex config")?;
    let auth = match files::read_json5(&auth_path(dirs), "Codex auth")? {
        Some(value) => {
            files::ensure_object(&value, &auth_path(dirs), "Codex auth")?;
            Some(value)
        }
        None => None,
    };
    if config.is_none() && auth.is_none() {
        return Ok(None);
    }
    Ok(Some(json!({
        "auth": auth.unwrap_or(Value::Null),
        "config": config,
    })))
}

/// Write the provider projection. `auth` absent/`null` deletes `auth.json`
/// when `remove_auth` is set; `config` must be valid TOML when present.
pub fn write_live(dirs: &AgentDirs, settings: &Value, remove_auth: bool) -> Result<(), AppError> {
    let object = settings.as_object().ok_or_else(|| {
        AppError::InvalidRequest("Codex settingsConfig must be an object".to_owned())
    })?;

    if let Some(config) = object.get("config") {
        match config {
            Value::String(text) => {
                text.parse::<DocumentMut>().map_err(|error| {
                    AppError::InvalidRequest(format!("Codex config is not valid TOML: {error}"))
                })?;
                files::atomic_write_private(&config_path(dirs), text.as_bytes())?;
            }
            Value::Null => {}
            _ => {
                return Err(AppError::InvalidRequest(
                    "Codex settingsConfig.config must be a TOML string".to_owned(),
                ))
            }
        }
    }

    match object.get("auth") {
        Some(auth @ Value::Object(_)) => {
            files::write_json_pretty(&auth_path(dirs), auth)?;
        }
        Some(Value::Null) | None => {
            if remove_auth {
                files::remove_file_if_exists(&auth_path(dirs))?;
            }
        }
        _ => {
            return Err(AppError::InvalidRequest(
                "Codex settingsConfig.auth must be an object or null".to_owned(),
            ))
        }
    }
    Ok(())
}

/// The auth material a profile carries, if any.
pub fn has_auth(settings: &Value) -> bool {
    settings.get("auth").is_some_and(Value::is_object)
}

/// Replace literal secret values inside the TOML text with the mask sentinel.
/// `env_key`/`env_http_headers` hold variable *names*, not secrets, so their
/// values are left alone; `http_headers` holds literal header values.
pub fn mask_config_text(text: &str) -> String {
    let Ok(mut doc) = text.parse::<DocumentMut>() else {
        return text.to_owned();
    };
    mask_toml_table(doc.as_table_mut(), &[]);
    doc.to_string()
}

/// After an update carries masked placeholders, restore the real values from
/// the previously stored config text wherever the new text still has the mask.
pub fn restore_config_text(new_text: &str, old_text: &str) -> String {
    let Ok(mut new_doc) = new_text.parse::<DocumentMut>() else {
        return new_text.to_owned();
    };
    let Ok(old_doc) = old_text.parse::<DocumentMut>() else {
        return new_text.to_owned();
    };
    restore_toml_table(new_doc.as_table_mut(), old_doc.as_table(), &[]);
    new_doc.to_string()
}

fn mask_toml_table(table: &mut dyn toml_edit::TableLike, path: &[String]) {
    let in_http_headers = path.last().is_some_and(|segment| segment == "http_headers");
    for (key, item) in table.iter_mut() {
        match item {
            toml_edit::Item::Value(toml_edit::Value::String(value))
                if in_http_headers || is_sensitive_key(key.get()) =>
            {
                *value = toml_edit::Formatted::new(MASKED_SECRET.to_owned());
            }
            toml_edit::Item::Table(child) => {
                let mut next = path.to_vec();
                next.push(key.get().to_owned());
                mask_toml_table(child, &next);
            }
            toml_edit::Item::ArrayOfTables(children) => {
                for child in children.iter_mut() {
                    let mut next = path.to_vec();
                    next.push(key.get().to_owned());
                    mask_toml_table(child, &next);
                }
            }
            _ => {}
        }
    }
}

fn restore_toml_table(
    table: &mut dyn toml_edit::TableLike,
    old: &dyn toml_edit::TableLike,
    path: &[String],
) {
    let in_http_headers = path.last().is_some_and(|segment| segment == "http_headers");
    let keys: Vec<String> = table.iter().map(|(key, _)| key.to_owned()).collect();
    for key in keys {
        let Some(old_item) = old.get(&key) else {
            continue;
        };
        let Some(new_item) = table.get_mut(&key) else {
            continue;
        };
        match (new_item, old_item) {
            (
                toml_edit::Item::Value(toml_edit::Value::String(new_value)),
                toml_edit::Item::Value(toml_edit::Value::String(old_value)),
            ) if new_value.value() == MASKED_SECRET
                && (in_http_headers || is_sensitive_key(&key)) =>
            {
                *new_value = toml_edit::Formatted::new(old_value.value().clone());
            }
            (toml_edit::Item::Table(new_child), toml_edit::Item::Table(old_child)) => {
                let mut next = path.to_vec();
                next.push(key.clone());
                restore_toml_table(new_child, old_child, &next);
            }
            (
                toml_edit::Item::ArrayOfTables(new_children),
                toml_edit::Item::ArrayOfTables(old_children),
            ) => {
                for (new_child, old_child) in new_children.iter_mut().zip(old_children.iter()) {
                    let mut next = path.to_vec();
                    next.push(key.clone());
                    restore_toml_table(new_child, old_child, &next);
                }
            }
            _ => {}
        }
    }
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
    let mut masked = super::mask_json_secrets(settings.clone());
    if let Some(object) = masked.as_object_mut() {
        if let Some(config) = object.get("config").and_then(Value::as_str) {
            object.insert("config".to_owned(), Value::String(mask_config_text(config)));
        }
    }
    masked
}

/// Whether the `config` TOML text still contains an unrestored mask placeholder.
pub fn config_text_has_mask(settings: &Value) -> bool {
    settings
        .get("config")
        .and_then(Value::as_str)
        .is_some_and(|text| text.contains(MASKED_SECRET))
}
