//! Shared helpers for reading and atomically rewriting agent config files.
//! Mirrors the write discipline cc-switch uses on the same files: parse
//! tolerantly (JSONC/JSON5), refuse malformed roots instead of rebuilding them,
//! and verify a content revision before overwriting so edits made outside
//! TodeX surface as conflicts instead of being silently lost.

use std::fs;
use std::path::Path;

use serde_json::Value;
use sha2::{Digest, Sha256};

use crate::error::AppError;

pub const MAX_LIVE_FILE_BYTES: u64 = 1024 * 1024;
const MISSING_REVISION: &str = "missing";

fn revision(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

pub fn read_limited(path: &Path, label: &str) -> Result<Vec<u8>, AppError> {
    let file = fs::File::open(path).map_err(AppError::Io)?;
    let metadata = file.metadata().map_err(AppError::Io)?;
    if metadata.len() > MAX_LIVE_FILE_BYTES {
        return Err(AppError::InvalidRequest(format!(
            "{label} file exceeds the 1 MiB limit: {}",
            path.display()
        )));
    }
    use std::io::Read;
    let mut bytes = Vec::with_capacity(metadata.len() as usize);
    file.take(MAX_LIVE_FILE_BYTES + 1)
        .read_to_end(&mut bytes)
        .map_err(AppError::Io)?;
    if bytes.len() as u64 > MAX_LIVE_FILE_BYTES {
        return Err(AppError::InvalidRequest(format!(
            "{label} file exceeds the 1 MiB limit: {}",
            path.display()
        )));
    }
    Ok(bytes)
}

pub fn read_text(path: &Path, label: &str) -> Result<Option<String>, AppError> {
    match read_limited(path, label) {
        Ok(bytes) => Ok(Some(String::from_utf8(bytes).map_err(|error| {
            AppError::InvalidRequest(format!(
                "{label} file must be UTF-8 ({}): {error}",
                path.display()
            ))
        })?)),
        Err(AppError::Io(error)) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error),
    }
}

/// Parse a config file that may contain comments/trailing commas. JSON5 is a
/// strict superset of JSON, so plain JSON files parse identically.
pub fn parse_json5(text: &str, path: &Path, label: &str) -> Result<Value, AppError> {
    json5::from_str(text).map_err(|error| {
        AppError::InvalidRequest(format!(
            "{label} file is not valid JSON/JSONC ({}): {error}",
            path.display()
        ))
    })
}

pub fn read_json5(path: &Path, label: &str) -> Result<Option<Value>, AppError> {
    match read_text(path, label)? {
        Some(text) => parse_json5(&text, path, label).map(Some),
        None => Ok(None),
    }
}

pub fn ensure_object(value: &Value, path: &Path, label: &str) -> Result<(), AppError> {
    if value.is_object() {
        Ok(())
    } else {
        Err(AppError::InvalidRequest(format!(
            "{label} root must be a JSON object: {}",
            path.display()
        )))
    }
}

/// Atomically write `bytes` to `path` with owner-only permissions.
pub fn atomic_write_private(path: &Path, bytes: &[u8]) -> Result<(), AppError> {
    let parent = path.parent().ok_or_else(|| {
        AppError::InvalidRequest(format!("config path has no parent: {}", path.display()))
    })?;
    let created = !parent.exists();
    fs::create_dir_all(parent).map_err(AppError::Io)?;
    #[cfg(unix)]
    if created {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(parent, fs::Permissions::from_mode(0o700)).map_err(AppError::Io)?;
    }
    #[cfg(not(unix))]
    let _ = created;

    let tmp_path = path.with_file_name(format!(
        ".{}.{}.tmp",
        path.file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("config"),
        uuid::Uuid::new_v4().simple()
    ));
    let write = || -> Result<(), AppError> {
        fs::write(&tmp_path, bytes).map_err(AppError::Io)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&tmp_path, fs::Permissions::from_mode(0o600))
                .map_err(AppError::Io)?;
        }
        #[cfg(windows)]
        if path.exists() {
            fs::remove_file(path).map_err(AppError::Io)?;
        }
        fs::rename(&tmp_path, path).map_err(AppError::Io)?;
        Ok(())
    };
    let result = write();
    if result.is_err() {
        let _ = fs::remove_file(&tmp_path);
    }
    result
}

pub fn write_json_pretty(path: &Path, value: &Value) -> Result<(), AppError> {
    let mut bytes = serde_json::to_vec_pretty(value)?;
    bytes.push(b'\n');
    atomic_write_private(path, &bytes)
}

/// Read-modify-write a JSONC document guarded by a content revision: if the
/// file changed on disk between the read and the write (another tool or a
/// manual edit raced us), fail with Conflict so callers retry instead of
/// losing the external edit.
pub fn modify_json5_file(
    path: &Path,
    label: &str,
    modify: impl FnOnce(&mut Value) -> Result<(), AppError>,
) -> Result<(), AppError> {
    let (mut document, expected_revision) = read_with_revision(path, label)?;
    modify(&mut document)?;
    ensure_revision(path, label, &expected_revision)?;
    write_json_pretty(path, &document)
}

fn read_with_revision(path: &Path, label: &str) -> Result<(Value, String), AppError> {
    match read_limited(path, label) {
        Ok(bytes) => {
            let rev = revision(&bytes);
            let text = String::from_utf8(bytes).map_err(|error| {
                AppError::InvalidRequest(format!(
                    "{label} file must be UTF-8 ({}): {error}",
                    path.display()
                ))
            })?;
            Ok((parse_json5(&text, path, label)?, rev))
        }
        Err(AppError::Io(error)) if error.kind() == std::io::ErrorKind::NotFound => Ok((
            Value::Object(serde_json::Map::new()),
            MISSING_REVISION.to_owned(),
        )),
        Err(error) => Err(error),
    }
}

fn ensure_revision(path: &Path, label: &str, expected: &str) -> Result<(), AppError> {
    let actual = match fs::File::open(path) {
        Ok(_) => revision(&read_limited(path, label)?),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => MISSING_REVISION.to_owned(),
        Err(error) => return Err(AppError::Io(error)),
    };
    if actual == expected {
        Ok(())
    } else {
        Err(AppError::Conflict(format!(
            "{label} changed on disk; reload and try again: {}",
            path.display()
        )))
    }
}

pub fn remove_file_if_exists(path: &Path) -> Result<bool, AppError> {
    match fs::remove_file(path) {
        Ok(()) => Ok(true),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(AppError::Io(error)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::path::PathBuf;

    fn test_dir() -> PathBuf {
        let root = std::env::temp_dir().join(format!(
            "todex-agent-files-{}",
            uuid::Uuid::new_v4().simple()
        ));
        fs::create_dir_all(&root).unwrap();
        root
    }

    #[test]
    fn modify_detects_external_change_between_read_and_write() {
        let root = test_dir();
        let path = root.join("settings.json");
        fs::write(&path, r#"{"a": 1}"#).unwrap();

        let result = modify_json5_file(&path, "test config", |document| {
            // Simulate another writer landing between our read and write.
            fs::write(&path, r#"{"a": 2, "external": true}"#).unwrap();
            document["b"] = json!(3);
            Ok(())
        });
        assert!(matches!(result, Err(AppError::Conflict(_))));
        // The external edit survives.
        let saved: Value = serde_json::from_str(&fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(saved["external"], true);
        assert!(saved.get("b").is_none());
    }

    #[test]
    fn modify_creates_missing_file_and_preserves_comments_on_parse() {
        let root = test_dir();
        let path = root.join("nested").join("config.json");
        modify_json5_file(&path, "test config", |document| {
            document["created"] = json!(true);
            Ok(())
        })
        .unwrap();
        let saved: Value = serde_json::from_str(&fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(saved["created"], true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = fs::metadata(&path).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o600);
        }
    }

    #[test]
    fn parse_json5_accepts_comments_and_trailing_commas() {
        let value = parse_json5(
            "{\n  // a comment\n  \"a\": 1,\n}\n",
            Path::new("x.json"),
            "test",
        )
        .unwrap();
        assert_eq!(value["a"], 1);
        assert!(parse_json5("[1,2]", Path::new("x.json"), "test").is_ok());
    }
}
