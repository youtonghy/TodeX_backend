use std::{
    env, fs,
    path::{Path, PathBuf},
};

use crate::error::AppError;

pub fn canonical_workspace_root(root: &Path) -> Result<PathBuf, AppError> {
    let root = expand_home(root);
    let metadata = fs::metadata(&root).map_err(workspace_path_error)?;
    if !metadata.is_dir() {
        return Err(AppError::InvalidRequest(format!(
            "workspace root must be an existing directory: {}",
            root.display()
        )));
    }
    fs::canonicalize(&root).map_err(AppError::Io)
}

pub fn validate_workspace_directory(root: &Path, path: &Path) -> Result<PathBuf, AppError> {
    let root = canonical_workspace_root(root)?;
    let path = expand_home(path);
    if !path.is_absolute() {
        return Err(AppError::InvalidRequest(
            "workspace path must be absolute".to_owned(),
        ));
    }

    let metadata = fs::metadata(&path).map_err(workspace_path_error)?;
    if !metadata.is_dir() {
        return Err(AppError::InvalidRequest(format!(
            "workspace path must be an existing directory: {}",
            path.display()
        )));
    }

    let canonical = fs::canonicalize(&path).map_err(AppError::Io)?;
    if canonical.starts_with(&root) {
        return Ok(canonical);
    }
    Err(AppError::WorkspacePathOutsideRoot)
}

pub fn validate_workspace_directory_text(root: &Path, path: &str) -> Result<PathBuf, AppError> {
    let trimmed = path.trim();
    if trimmed.is_empty() {
        return Err(AppError::InvalidRequest(
            "workspace path is required".to_owned(),
        ));
    }
    validate_workspace_directory(root, Path::new(trimmed))
}

pub fn expand_home(path: &Path) -> PathBuf {
    let raw = path.as_os_str().to_string_lossy();
    if raw == "~" {
        return env::var_os("HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|| path.to_path_buf());
    }
    if let Some(rest) = raw.strip_prefix("~/") {
        if let Some(home) = env::var_os("HOME") {
            return PathBuf::from(home).join(rest);
        }
    }
    path.to_path_buf()
}

fn workspace_path_error(error: std::io::Error) -> AppError {
    match error.kind() {
        std::io::ErrorKind::NotFound => AppError::WorkspacePathNotFound,
        std::io::ErrorKind::PermissionDenied => {
            AppError::Unauthorized("permission denied accessing workspace path".to_owned())
        }
        _ => AppError::Io(error),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{SystemTime, UNIX_EPOCH};

    #[test]
    fn validates_directory_inside_workspace_root() {
        let root = make_temp_dir("todex-workspace-paths-root");
        let child = root.join("project");
        fs::create_dir_all(&child).unwrap();

        let validated = validate_workspace_directory(&root, &child).unwrap();

        assert_eq!(validated, fs::canonicalize(&child).unwrap());
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn rejects_directory_outside_workspace_root() {
        let root = make_temp_dir("todex-workspace-paths-root");
        let outside = make_temp_dir("todex-workspace-paths-outside");

        let error = validate_workspace_directory(&root, &outside).expect_err("outside root");

        assert!(matches!(error, AppError::WorkspacePathOutsideRoot));
        let _ = fs::remove_dir_all(root);
        let _ = fs::remove_dir_all(outside);
    }

    fn make_temp_dir(prefix: &str) -> PathBuf {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let path = env::temp_dir().join(format!("{prefix}-{nonce}-{}", std::process::id()));
        fs::create_dir_all(&path).unwrap();
        path
    }
}
