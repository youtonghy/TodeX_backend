//! Owner-only file writes for daemon state and user credentials.

use std::{
    fs,
    io::{self, Write},
    path::Path,
};

use uuid::Uuid;

/// Creates `path` (and parents) and restricts it to the owner (0700 on Unix).
pub(crate) fn ensure_owner_only_dir(path: &Path) -> io::Result<()> {
    fs::create_dir_all(path)?;
    set_owner_only(path, true)
}

/// Atomically replaces `path` with `bytes`: a 0600 sibling temp file is
/// written and synced, then renamed over the target, so readers never see a
/// partial file and the content is never world-readable.
pub(crate) fn write_owner_only_atomic(path: &Path, bytes: &[u8]) -> io::Result<()> {
    let parent = path
        .parent()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "path has no parent"))?;
    let file_name = path
        .file_name()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "path has no file name"))?;
    let temporary = parent.join(format!(
        ".{}.{}.tmp",
        file_name.to_string_lossy(),
        Uuid::new_v4().simple()
    ));
    let result = (|| {
        write_new_file(&temporary, bytes)?;
        #[cfg(windows)]
        if path.exists() {
            fs::remove_file(path)?;
        }
        fs::rename(&temporary, path)?;
        set_owner_only(path, false)
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temporary);
    }
    result
}

/// Writes `bytes` to a new 0600 file and fails with `AlreadyExists` instead of
/// replacing an existing one. A partially written file is removed.
pub(crate) fn create_owner_only(path: &Path, bytes: &[u8]) -> io::Result<()> {
    let result = write_new_file(path, bytes);
    if let Err(error) = &result {
        if error.kind() != io::ErrorKind::AlreadyExists {
            let _ = fs::remove_file(path);
        }
    }
    result
}

/// Restricts an existing path to its owner: 0700 for directories, 0600 for
/// files. A no-op on platforms without Unix permissions.
pub(crate) fn set_owner_only(path: &Path, directory: bool) -> io::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = if directory { 0o700 } else { 0o600 };
        fs::set_permissions(path, fs::Permissions::from_mode(mode))?;
    }
    #[cfg(not(unix))]
    let _ = (path, directory);
    Ok(())
}

fn write_new_file(path: &Path, bytes: &[u8]) -> io::Result<()> {
    let mut options = fs::OpenOptions::new();
    options.create_new(true).write(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options.open(path)?;
    file.write_all(bytes)?;
    file.sync_all()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(label: &str) -> std::path::PathBuf {
        std::env::temp_dir().join(format!("todex-secure-fs-{label}-{}", Uuid::new_v4()))
    }

    #[test]
    fn atomic_write_replaces_content_with_owner_only_mode() {
        let dir = temp_dir("atomic");
        ensure_owner_only_dir(&dir).unwrap();
        let path = dir.join("state.json");
        write_owner_only_atomic(&path, b"one").unwrap();
        write_owner_only_atomic(&path, b"two").unwrap();
        assert_eq!(fs::read(&path).unwrap(), b"two");
        assert_eq!(fs::read_dir(&dir).unwrap().count(), 1, "no temp files left");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                fs::metadata(&path).unwrap().permissions().mode() & 0o777,
                0o600
            );
            assert_eq!(
                fs::metadata(&dir).unwrap().permissions().mode() & 0o777,
                0o700
            );
        }
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn create_owner_only_never_overwrites() {
        let dir = temp_dir("create");
        ensure_owner_only_dir(&dir).unwrap();
        let path = dir.join("id_ed25519");
        create_owner_only(&path, b"secret").unwrap();
        let error = create_owner_only(&path, b"other").unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::AlreadyExists);
        assert_eq!(fs::read(&path).unwrap(), b"secret");
        fs::remove_dir_all(dir).unwrap();
    }
}
