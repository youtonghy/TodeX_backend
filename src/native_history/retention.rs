//! Claude Code's own transcript retention: the `cleanupPeriodDays` key of
//! `<claude_dir>/settings.json`. Claude deletes transcripts older than that
//! on its own start-up, regardless of TodeX references.

use std::{
    fs,
    io::{self, Write},
    path::{Path, PathBuf},
};

use serde_json::{Map, Value};

use crate::error::AppError;

pub(super) const CLEANUP_PERIOD_KEY: &str = "cleanupPeriodDays";
pub(super) const MAX_CLEANUP_PERIOD_DAYS: i64 = 3650;

pub(super) fn settings_path(claude_dir: &Path) -> PathBuf {
    claude_dir.join("settings.json")
}

/// The saved `cleanupPeriodDays` (any JSON number; `None` when the file or
/// key is absent). A file that is not a JSON object is a `CONFLICT`.
pub(super) fn read(path: &Path) -> Result<Option<Value>, AppError> {
    Ok(read_settings(path)?
        .and_then(|settings| settings.get(CLEANUP_PERIOD_KEY).cloned())
        .filter(Value::is_number))
}

/// Validates a `PUT` body: `cleanupPeriodDays` must be present and either
/// `null` or an integer in `1..=3650`.
pub(super) fn parse_request(body: &Value) -> Result<Option<u32>, AppError> {
    let invalid = || {
        AppError::InvalidRequest(format!(
            "{CLEANUP_PERIOD_KEY} must be null or an integer between 1 and {MAX_CLEANUP_PERIOD_DAYS}"
        ))
    };
    let object = body.as_object().ok_or_else(|| {
        AppError::InvalidRequest("the native retention must be a JSON object".to_owned())
    })?;
    match object.get(CLEANUP_PERIOD_KEY) {
        None => Err(invalid()),
        Some(Value::Null) => Ok(None),
        Some(value) => match value.as_i64() {
            Some(days) if (1..=MAX_CLEANUP_PERIOD_DAYS).contains(&days) => Ok(Some(days as u32)),
            _ => Err(invalid()),
        },
    }
}

/// Sets (or, for `None`, removes) `cleanupPeriodDays`, keeping every other
/// key and the file's permissions; a missing file is created owner-only.
/// The file is replaced atomically; a symlinked `settings.json` is updated
/// at its target.
pub(super) fn write(path: &Path, days: Option<u32>) -> Result<Option<Value>, AppError> {
    let mut settings = read_settings(path)?.unwrap_or_default();
    match days {
        Some(days) => {
            settings.insert(CLEANUP_PERIOD_KEY.to_owned(), Value::from(days));
        }
        None => {
            settings.remove(CLEANUP_PERIOD_KEY);
        }
    }
    let mut bytes = serde_json::to_vec_pretty(&Value::Object(settings))?;
    bytes.push(b'\n');
    let target = match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_symlink() => fs::canonicalize(path)?,
        _ => path.to_owned(),
    };
    let permissions = match fs::metadata(&target) {
        Ok(metadata) => Some(metadata.permissions()),
        Err(error) if error.kind() == io::ErrorKind::NotFound => None,
        Err(error) => return Err(error.into()),
    };
    if let Some(parent) = target.parent() {
        fs::create_dir_all(parent)?;
    }
    replace_atomic(&target, &bytes, permissions)?;
    Ok(days.map(Value::from))
}

fn read_settings(path: &Path) -> Result<Option<Map<String, Value>>, AppError> {
    let bytes = match fs::read(path) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error.into()),
    };
    match serde_json::from_slice::<Value>(&bytes) {
        Ok(Value::Object(settings)) => Ok(Some(settings)),
        _ => Err(AppError::Conflict(format!(
            "{} is not a JSON object; fix or remove it first",
            path.display()
        ))),
    }
}

fn replace_atomic(
    path: &Path,
    bytes: &[u8],
    permissions: Option<fs::Permissions>,
) -> io::Result<()> {
    let parent = path
        .parent()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "path has no parent"))?;
    let name = path
        .file_name()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "path has no file name"))?;
    let temporary = parent.join(format!(
        ".{}.{}.tmp",
        name.to_string_lossy(),
        uuid::Uuid::new_v4().simple()
    ));
    let result = (|| {
        let mut options = fs::OpenOptions::new();
        options.create_new(true).write(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let mut file = options.open(&temporary)?;
        file.write_all(bytes)?;
        file.sync_all()?;
        drop(file);
        if let Some(permissions) = permissions {
            fs::set_permissions(&temporary, permissions)?;
        }
        #[cfg(windows)]
        if path.exists() {
            fs::remove_file(path)?;
        }
        fs::rename(&temporary, path)
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temporary);
    }
    result
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn request_validation() {
        assert_eq!(
            parse_request(&json!({"cleanupPeriodDays": null})).unwrap(),
            None
        );
        assert_eq!(
            parse_request(&json!({"cleanupPeriodDays": 1})).unwrap(),
            Some(1)
        );
        assert_eq!(
            parse_request(&json!({"cleanupPeriodDays": 3650, "settingsPath": "/x"})).unwrap(),
            Some(3650)
        );
        for body in [
            json!({}),
            json!([]),
            json!({"cleanupPeriodDays": 0}),
            json!({"cleanupPeriodDays": 3651}),
            json!({"cleanupPeriodDays": 1.5}),
            json!({"cleanupPeriodDays": "5"}),
        ] {
            assert!(matches!(
                parse_request(&body),
                Err(AppError::InvalidRequest(_))
            ));
        }
    }

    #[test]
    fn edits_preserve_other_keys_and_permissions() {
        let dir = std::env::temp_dir().join(format!("todex-retention-{}", uuid::Uuid::new_v4()));
        fs::create_dir_all(&dir).unwrap();
        let path = settings_path(&dir);

        // A missing file is created owner-only.
        assert_eq!(read(&path).unwrap(), None);
        write(&path, Some(7)).unwrap();
        assert_eq!(read(&path).unwrap(), Some(json!(7)));
        #[cfg(unix)]
        use std::os::unix::fs::PermissionsExt;
        #[cfg(unix)]
        assert_eq!(
            fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
        fs::write(
            &path,
            r#"{"model": "opus", "env": {"A": "1"}, "cleanupPeriodDays": 30}"#,
        )
        .unwrap();
        #[cfg(unix)]
        fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).unwrap();
        write(&path, Some(5)).unwrap();
        #[cfg(unix)]
        assert_eq!(
            fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o644
        );
        let saved: Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
        assert_eq!(
            saved,
            json!({"model": "opus", "env": {"A": "1"}, "cleanupPeriodDays": 5})
        );
        write(&path, None).unwrap();
        let saved: Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
        assert_eq!(saved, json!({"model": "opus", "env": {"A": "1"}}));
        assert_eq!(read(&path).unwrap(), None);

        // Invalid JSON is a conflict and stays untouched.
        fs::write(&path, b"{not json").unwrap();
        assert!(matches!(read(&path), Err(AppError::Conflict(_))));
        assert!(matches!(write(&path, Some(3)), Err(AppError::Conflict(_))));
        assert_eq!(fs::read(&path).unwrap(), b"{not json");
        assert_eq!(fs::read_dir(&dir).unwrap().count(), 1, "no temp files left");
        fs::remove_dir_all(dir).unwrap();
    }
}
