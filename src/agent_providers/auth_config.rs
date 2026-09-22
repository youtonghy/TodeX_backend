//! Shared projection for agents whose provider state is an `auth.json`
//! credential file plus a `config.toml` document (Codex CLI, Grok Build).
//!
//! A profile's `settingsConfig` is `{auth, config}`: `auth` is the whole
//! `auth.json` object (or `null`/absent to leave or remove it) and `config`
//! is the full `config.toml` text, the shape cc-switch stores for Codex.

use std::path::PathBuf;

use serde_json::{json, Value};
use toml_edit::{DocumentMut, Item, TableLike};

use super::files;
use super::{is_sensitive_key, MASKED_SECRET};
use crate::error::AppError;

/// Live file locations plus the labels used in error messages.
pub struct PairedFiles {
    pub config: PathBuf,
    pub auth: PathBuf,
    pub agent_label: &'static str,
}

/// Live state normalized as `{auth: <object|null>, config: <toml text|null>}`;
/// `None` when neither file exists.
pub fn read_live(paths: &PairedFiles) -> Result<Option<Value>, AppError> {
    let label = paths.agent_label;
    let config = files::read_text(&paths.config, &format!("{label} config"))?;
    let auth_label = format!("{label} auth");
    let auth = match files::read_json5(&paths.auth, &auth_label)? {
        Some(value) => {
            files::ensure_object(&value, &paths.auth, &auth_label)?;
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
pub fn write_live(
    paths: &PairedFiles,
    settings: &Value,
    remove_auth: bool,
) -> Result<(), AppError> {
    let label = paths.agent_label;
    let object = settings.as_object().ok_or_else(|| {
        AppError::InvalidRequest(format!("{label} settingsConfig must be an object"))
    })?;

    if let Some(config) = object.get("config") {
        match config {
            Value::String(text) => {
                text.parse::<DocumentMut>().map_err(|error| {
                    AppError::InvalidRequest(format!("{label} config is not valid TOML: {error}"))
                })?;
                files::atomic_write_private(&paths.config, text.as_bytes())?;
            }
            Value::Null => {}
            _ => {
                return Err(AppError::InvalidRequest(format!(
                    "{label} settingsConfig.config must be a TOML string"
                )))
            }
        }
    }

    match object.get("auth") {
        Some(auth @ Value::Object(_)) => {
            files::write_json_pretty(&paths.auth, auth)?;
        }
        Some(Value::Null) | None => {
            if remove_auth {
                files::remove_file_if_exists(&paths.auth)?;
            }
        }
        _ => {
            return Err(AppError::InvalidRequest(format!(
                "{label} settingsConfig.auth must be an object or null"
            )))
        }
    }
    Ok(())
}

/// The auth material a profile carries, if any.
pub fn has_auth(settings: &Value) -> bool {
    settings.get("auth").is_some_and(Value::is_object)
}

/// Whether the two `config` TOML texts describe the same document.
pub fn config_matches(live: &Value, settings: &Value) -> bool {
    match (
        live.get("config").and_then(Value::as_str),
        settings.get("config").and_then(Value::as_str),
    ) {
        (Some(a), Some(b)) => match (a.parse::<toml::Value>(), b.parse::<toml::Value>()) {
            (Ok(a), Ok(b)) => a == b,
            _ => a.trim() == b.trim(),
        },
        (a, b) => a == b,
    }
}

/// `auth` equality where an absent profile auth matches a missing file.
pub fn auth_matches(live: &Value, settings: &Value) -> bool {
    live.get("auth") == settings.get("auth")
        || (live.get("auth").is_some_and(Value::is_null)
            && settings.get("auth").is_none_or(Value::is_null))
}

/// Return a copy with secrets in both the `auth` object and the `config` TOML
/// text replaced by the mask sentinel. `header_tables` name TOML tables whose
/// every value is a literal header (e.g. Codex `http_headers`).
pub fn masked_settings(settings: &Value, header_tables: &[&str]) -> Value {
    let mut masked = super::mask_json_secrets(settings.clone());
    if let Some(object) = masked.as_object_mut() {
        if let Some(config) = object.get("config").and_then(Value::as_str) {
            let text = mask_config_text(config, header_tables);
            object.insert("config".to_owned(), Value::String(text));
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

/// Replace literal secret values inside the TOML text with the mask sentinel.
/// `env_key`/`env_http_headers` hold variable *names*, not secrets, so their
/// values are left alone.
pub fn mask_config_text(text: &str, header_tables: &[&str]) -> String {
    let Ok(mut doc) = text.parse::<DocumentMut>() else {
        return text.to_owned();
    };
    mask_toml_table(doc.as_table_mut(), &[], header_tables);
    doc.to_string()
}

/// After an update carries masked placeholders, restore the real values from
/// the previously stored config text wherever the new text still has the mask.
pub fn restore_config_text(new_text: &str, old_text: &str, header_tables: &[&str]) -> String {
    let Ok(mut new_doc) = new_text.parse::<DocumentMut>() else {
        return new_text.to_owned();
    };
    let Ok(old_doc) = old_text.parse::<DocumentMut>() else {
        return new_text.to_owned();
    };
    restore_toml_table(
        new_doc.as_table_mut(),
        old_doc.as_table(),
        &[],
        header_tables,
    );
    new_doc.to_string()
}

fn is_secret_entry(key: &str, path: &[String], header_tables: &[&str]) -> bool {
    let in_header_table = path
        .last()
        .is_some_and(|segment| header_tables.contains(&segment.as_str()));
    in_header_table || is_sensitive_key(key)
}

fn child_path(path: &[String], key: &str) -> Vec<String> {
    let mut next = path.to_vec();
    next.push(key.to_owned());
    next
}

fn mask_toml_table(table: &mut dyn TableLike, path: &[String], header_tables: &[&str]) {
    for (key, item) in table.iter_mut() {
        let key = key.get().to_owned();
        match item {
            Item::Value(toml_edit::Value::String(value))
                if is_secret_entry(&key, path, header_tables) =>
            {
                *value = toml_edit::Formatted::new(MASKED_SECRET.to_owned());
            }
            Item::Value(toml_edit::Value::InlineTable(child)) => {
                mask_toml_table(child, &child_path(path, &key), header_tables);
            }
            Item::Table(child) => {
                mask_toml_table(child, &child_path(path, &key), header_tables);
            }
            Item::ArrayOfTables(children) => {
                for child in children.iter_mut() {
                    mask_toml_table(child, &child_path(path, &key), header_tables);
                }
            }
            _ => {}
        }
    }
}

fn restore_toml_table(
    table: &mut dyn TableLike,
    old: &dyn TableLike,
    path: &[String],
    header_tables: &[&str],
) {
    let keys: Vec<String> = table.iter().map(|(key, _)| key.to_owned()).collect();
    for key in keys {
        let Some(old_item) = old.get(&key) else {
            continue;
        };
        let Some(new_item) = table.get_mut(&key) else {
            continue;
        };
        let next = child_path(path, &key);
        match (new_item, old_item) {
            (
                Item::Value(toml_edit::Value::String(new_value)),
                Item::Value(toml_edit::Value::String(old_value)),
            ) if new_value.value() == MASKED_SECRET
                && is_secret_entry(&key, path, header_tables) =>
            {
                *new_value = toml_edit::Formatted::new(old_value.value().clone());
            }
            (
                Item::Value(toml_edit::Value::InlineTable(new_child)),
                Item::Value(toml_edit::Value::InlineTable(old_child)),
            ) => restore_toml_table(new_child, old_child, &next, header_tables),
            (Item::Table(new_child), Item::Table(old_child)) => {
                restore_toml_table(new_child, old_child, &next, header_tables);
            }
            (Item::ArrayOfTables(new_children), Item::ArrayOfTables(old_children)) => {
                for (new_child, old_child) in new_children.iter_mut().zip(old_children.iter()) {
                    restore_toml_table(new_child, old_child, &next, header_tables);
                }
            }
            _ => {}
        }
    }
}
