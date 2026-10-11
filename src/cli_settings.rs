//! Claude Code's user settings (`<claude_dir>/settings.json`), edited from
//! the CLI manager. Every write names the revision it was based on, so an
//! edit made meanwhile by Claude Code itself, a provider switch or a text
//! editor surfaces as `CONFLICT` instead of being overwritten. The settings
//! form sends an RFC 7386 merge patch (only the keys it changed); the raw
//! editor replaces the whole text, which is kept byte for byte.

use std::{
    fs, io,
    path::{Path, PathBuf},
    sync::Mutex,
};

use serde::Serialize;
use serde_json::{Map, Value};
use sha2::{Digest, Sha256};

use crate::error::AppError;

/// Largest settings file read or written.
pub(crate) const MAX_SETTINGS_BYTES: usize = 1024 * 1024;
/// The revision of a file that does not exist yet.
const MISSING_REVISION: &str = "missing";

/// Serializes this module's read-check-write cycles within the daemon.
static WRITE_LOCK: Mutex<()> = Mutex::new(());

pub(crate) fn claude_settings_path() -> PathBuf {
    crate::agent_providers::claude_config_dir().join("settings.json")
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct SettingsDocument {
    pub path: String,
    pub exists: bool,
    pub revision: String,
    /// The file as stored; empty when it does not exist.
    pub text: String,
    /// The parsed object; `None` when the text is not a JSON object.
    pub settings: Option<Map<String, Value>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub parse_error: Option<String>,
}

pub(crate) fn read(path: &Path) -> Result<SettingsDocument, AppError> {
    let bytes = match fs::read(path) {
        Ok(bytes) => Some(bytes),
        Err(error) if error.kind() == io::ErrorKind::NotFound => None,
        Err(error) => return Err(error.into()),
    };
    let display = path.display().to_string();
    let Some(bytes) = bytes else {
        return Ok(SettingsDocument {
            path: display,
            exists: false,
            revision: MISSING_REVISION.to_owned(),
            text: String::new(),
            settings: Some(Map::new()),
            parse_error: None,
        });
    };
    if bytes.len() > MAX_SETTINGS_BYTES {
        return Err(AppError::InvalidRequest(format!(
            "{display} exceeds the 1 MiB limit"
        )));
    }
    let revision = revision(&bytes);
    let text = String::from_utf8(bytes)
        .map_err(|_| AppError::InvalidRequest(format!("{display} is not UTF-8")))?;
    let (settings, parse_error) = match parse_object(&text) {
        Ok(settings) => (Some(settings), None),
        Err(error) => (None, Some(error)),
    };
    Ok(SettingsDocument {
        path: display,
        exists: true,
        revision,
        text,
        settings,
        parse_error,
    })
}

/// Applies `patch` to the stored settings: `null` removes a key, objects
/// merge recursively, anything else (arrays included) replaces the value.
pub(crate) fn apply_patch(
    path: &Path,
    expected_revision: &str,
    patch: &Map<String, Value>,
) -> Result<SettingsDocument, AppError> {
    let _guard = WRITE_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let current = checked(path, expected_revision)?;
    let Some(settings) = current.settings else {
        return Err(AppError::Conflict(format!(
            "{} is not a JSON object; fix it in the raw editor first",
            current.path
        )));
    };
    let mut document = Value::Object(settings);
    merge_patch(&mut document, &Value::Object(patch.clone()));
    let mut bytes = serde_json::to_vec_pretty(&document)?;
    bytes.push(b'\n');
    write(path, &bytes)
}

/// Replaces the file with `text`, which must be a JSON object.
pub(crate) fn replace_text(
    path: &Path,
    expected_revision: &str,
    text: &str,
) -> Result<SettingsDocument, AppError> {
    if text.len() > MAX_SETTINGS_BYTES {
        return Err(AppError::InvalidRequest(
            "settings text exceeds the 1 MiB limit".to_owned(),
        ));
    }
    parse_object(text).map_err(AppError::InvalidRequest)?;
    let _guard = WRITE_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    checked(path, expected_revision)?;
    write(path, text.as_bytes())
}

fn checked(path: &Path, expected_revision: &str) -> Result<SettingsDocument, AppError> {
    let current = read(path)?;
    if current.revision != expected_revision {
        return Err(AppError::Conflict(format!(
            "{} changed on disk; reload and try again",
            current.path
        )));
    }
    Ok(current)
}

fn write(path: &Path, bytes: &[u8]) -> Result<SettingsDocument, AppError> {
    crate::secure_fs::replace_keeping_mode(path, bytes)?;
    read(path)
}

fn parse_object(text: &str) -> Result<Map<String, Value>, String> {
    match serde_json::from_str::<Value>(text) {
        Ok(Value::Object(settings)) => Ok(settings),
        Ok(_) => Err("settings.json must contain a JSON object".to_owned()),
        Err(error) => Err(format!("settings.json is not valid JSON: {error}")),
    }
}

fn revision(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

fn merge_patch(target: &mut Value, patch: &Value) {
    let Value::Object(patch) = patch else {
        *target = patch.clone();
        return;
    };
    if !target.is_object() {
        *target = Value::Object(Map::new());
    }
    let Value::Object(target) = target else {
        unreachable!()
    };
    for (key, value) in patch {
        if value.is_null() {
            target.remove(key);
        } else {
            merge_patch(target.entry(key.clone()).or_insert(Value::Null), value);
        }
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn temp_dir() -> PathBuf {
        let dir = std::env::temp_dir().join(format!("todex-cli-settings-{}", uuid::Uuid::new_v4()));
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn object(value: Value) -> Map<String, Value> {
        let Value::Object(map) = value else {
            panic!("not an object")
        };
        map
    }

    #[test]
    fn patch_merges_and_removes_keys_keeping_the_rest() {
        let dir = temp_dir();
        let path = dir.join("settings.json");
        fs::write(
            &path,
            r#"{"model":"opus","permissions":{"allow":["Bash(ls)"],"defaultMode":"plan"},"hooks":{"Stop":[]}}"#,
        )
        .unwrap();
        let current = read(&path).unwrap();
        let saved = apply_patch(
            &path,
            &current.revision,
            &object(json!({
                "permissions": {"defaultMode": null, "deny": ["Read(.env)"]},
                "theme": "dark",
            })),
        )
        .unwrap();
        assert_eq!(
            Value::Object(saved.settings.unwrap()),
            json!({
                "model": "opus",
                "permissions": {"allow": ["Bash(ls)"], "deny": ["Read(.env)"]},
                "hooks": {"Stop": []},
                "theme": "dark",
            })
        );
        assert_ne!(saved.revision, current.revision);
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn stale_revisions_conflict_and_leave_the_file_alone() {
        let dir = temp_dir();
        let path = dir.join("settings.json");
        let missing = read(&path).unwrap();
        assert!(!missing.exists);
        apply_patch(&path, &missing.revision, &object(json!({"theme": "light"}))).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                fs::metadata(&path).unwrap().permissions().mode() & 0o777,
                0o600
            );
        }
        // Someone else edits the file after our read.
        fs::write(&path, r#"{"theme": "dark"}"#).unwrap();
        for result in [
            apply_patch(&path, &missing.revision, &object(json!({"verbose": true}))),
            replace_text(&path, &missing.revision, "{}"),
        ] {
            assert!(matches!(result, Err(AppError::Conflict(_))));
        }
        assert_eq!(fs::read_to_string(&path).unwrap(), r#"{"theme": "dark"}"#);
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn raw_text_must_be_an_object_and_is_kept_verbatim() {
        let dir = temp_dir();
        let path = dir.join("settings.json");
        fs::write(&path, "{broken").unwrap();
        let current = read(&path).unwrap();
        assert!(current.settings.is_none() && current.parse_error.is_some());
        assert!(matches!(
            apply_patch(&path, &current.revision, &object(json!({"theme": "dark"}))),
            Err(AppError::Conflict(_))
        ));
        for text in ["[]", "{\"a\":", ""] {
            assert!(matches!(
                replace_text(&path, &current.revision, text),
                Err(AppError::InvalidRequest(_))
            ));
        }
        let text = "{\n    \"theme\": \"dark\"\n}";
        let saved = replace_text(&path, &current.revision, text).unwrap();
        assert_eq!(saved.text, text);
        assert_eq!(fs::read_dir(&dir).unwrap().count(), 1, "no temp files left");
        fs::remove_dir_all(dir).unwrap();
    }
}
