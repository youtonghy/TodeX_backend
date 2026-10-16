//! Owner-only file writes for daemon state and user credentials.

use std::{
    fs,
    io::{self, Write},
    path::{Path, PathBuf},
    time::{Duration, Instant, SystemTime},
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
        replace_file(&temporary, path)?;
        set_owner_only(path, false)
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temporary);
    }
    result
}

/// [`write_owner_only_atomic`] unless `path` already is a regular owner-only
/// file holding exactly `bytes`; returns whether it wrote. Skipping saves the
/// temp file, fsync and rename when the content is unchanged.
pub(crate) fn write_owner_only_atomic_if_changed(path: &Path, bytes: &[u8]) -> io::Result<bool> {
    if is_owner_only_file_with(path, bytes) {
        return Ok(false);
    }
    write_owner_only_atomic(path, bytes)?;
    Ok(true)
}

fn is_owner_only_file_with(path: &Path, bytes: &[u8]) -> bool {
    let Ok(metadata) = fs::symlink_metadata(path) else {
        return false;
    };
    if !metadata.is_file() || metadata.len() != bytes.len() as u64 {
        return false;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if metadata.permissions().mode() & 0o777 != 0o600 {
            return false;
        }
    }
    fs::read(path).is_ok_and(|existing| existing == bytes)
}

/// Atomically replaces a user-owned config file, keeping its permissions: a
/// symlinked `path` is updated at its target, an existing file keeps its
/// mode, and a missing one (and its parents) is created owner-only.
pub(crate) fn replace_keeping_mode(path: &Path, bytes: &[u8]) -> io::Result<()> {
    let target = match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_symlink() => fs::canonicalize(path)?,
        _ => path.to_owned(),
    };
    let permissions = match fs::metadata(&target) {
        Ok(metadata) => Some(metadata.permissions()),
        Err(error) if error.kind() == io::ErrorKind::NotFound => None,
        Err(error) => return Err(error),
    };
    let parent = target
        .parent()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "path has no parent"))?;
    let file_name = target
        .file_name()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "path has no file name"))?;
    fs::create_dir_all(parent)?;
    let temporary = parent.join(format!(
        ".{}.{}.tmp",
        file_name.to_string_lossy(),
        Uuid::new_v4().simple()
    ));
    let result = (|| {
        write_new_file(&temporary, bytes)?;
        if let Some(permissions) = permissions {
            fs::set_permissions(&temporary, permissions)?;
        }
        replace_file(&temporary, &target)
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temporary);
    }
    result
}

/// Renames `from` over `to`, replacing an existing file in one step, so `to`
/// always holds either its old or its new content, even across a crash.
///
/// `fs::rename` replaces atomically on Windows too (POSIX rename semantics or
/// `MoveFileExW` with `MOVEFILE_REPLACE_EXISTING`); deleting `to` first, as
/// this code once did, left a window in which the file did not exist. A
/// target that another process (often a virus scanner) briefly holds open
/// without delete sharing fails with `PermissionDenied`, so Windows retries
/// a few times before giving up.
pub(crate) fn replace_file(from: &Path, to: &Path) -> io::Result<()> {
    #[cfg(windows)]
    {
        const RETRY_DELAYS_MS: [u64; 5] = [10, 30, 60, 150, 300];
        for delay in RETRY_DELAYS_MS {
            match fs::rename(from, to) {
                Err(error) if error.kind() == io::ErrorKind::PermissionDenied => {
                    std::thread::sleep(std::time::Duration::from_millis(delay));
                }
                result => return result,
            }
        }
    }
    fs::rename(from, to)
}

/// [`replace_file`] off the async runtime, as its Windows retries sleep.
pub(crate) async fn replace_file_async(from: &Path, to: &Path) -> io::Result<()> {
    let (from, to): (PathBuf, PathBuf) = (from.to_owned(), to.to_owned());
    tokio::task::spawn_blocking(move || replace_file(&from, &to))
        .await
        .map_err(io::Error::other)?
}

/// Identity of a file version for change detection: modification time,
/// length and, on Unix, inode. Every atomic replacement gets a new inode, so
/// Unix sees a rewrite even within one mtime tick (about 16 ms on Windows,
/// where an equal-length rewrite in the same tick stays invisible: stable Rust
/// exposes no Windows file ID).
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct FileStamp {
    modified: Option<SystemTime>,
    len: u64,
    #[cfg(unix)]
    inode: u64,
}

impl FileStamp {
    pub(crate) fn of(metadata: &fs::Metadata) -> Self {
        Self {
            modified: metadata.modified().ok(),
            len: metadata.len(),
            #[cfg(unix)]
            inode: std::os::unix::fs::MetadataExt::ino(metadata),
        }
    }

    /// The stamp of the file `path` resolves to.
    pub(crate) fn read(path: &Path) -> io::Result<Self> {
        fs::metadata(path).map(|metadata| Self::of(&metadata))
    }
}

/// How long writers of a shared state file wait for its [`lock_exclusive`]
/// lock. Holders only read, merge and atomically replace one small file.
pub(crate) const STATE_LOCK_TIMEOUT: Duration = Duration::from_secs(2);

/// `<path>.lock`: the lock file guarding read-modify-write cycles of `path`.
pub(crate) fn lock_path(path: &Path) -> PathBuf {
    let mut name = path.file_name().unwrap_or_default().to_os_string();
    name.push(".lock");
    path.with_file_name(name)
}

/// Takes an exclusive cross-process advisory lock on `lock_path`, creating
/// it (owner-only) and its parent directory when missing. Waits at most
/// `timeout` for another holder and then fails with `TimedOut`. The lock is
/// released when the returned file is dropped; std opens it close-on-exec,
/// so spawned programs do not keep it. The lock file itself is never
/// removed, as unlinking it would let two writers lock different files.
pub(crate) fn lock_exclusive(lock_path: &Path, timeout: Duration) -> io::Result<fs::File> {
    const POLL: Duration = Duration::from_millis(5);
    if let Some(parent) = lock_path.parent() {
        fs::create_dir_all(parent)?;
    }
    let mut options = fs::OpenOptions::new();
    options.create(true).write(true).truncate(false);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let file = options.open(lock_path)?;
    let deadline = Instant::now() + timeout;
    loop {
        match file.try_lock() {
            Ok(()) => return Ok(file),
            Err(fs::TryLockError::WouldBlock) => {}
            Err(fs::TryLockError::Error(error)) => return Err(error),
        }
        let now = Instant::now();
        if now >= deadline {
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                format!("{} is held by another process", lock_path.display()),
            ));
        }
        std::thread::sleep(POLL.min(deadline - now));
    }
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
    fn unchanged_content_is_not_rewritten() {
        let dir = temp_dir("unchanged");
        ensure_owner_only_dir(&dir).unwrap();
        let path = dir.join("config.json");
        assert!(write_owner_only_atomic_if_changed(&path, b"one").unwrap());
        let modified = fs::metadata(&path).unwrap().modified().unwrap();
        #[cfg(unix)]
        let inode = {
            use std::os::unix::fs::MetadataExt;
            fs::metadata(&path).unwrap().ino()
        };
        assert!(!write_owner_only_atomic_if_changed(&path, b"one").unwrap());
        assert_eq!(fs::metadata(&path).unwrap().modified().unwrap(), modified);
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt;
            assert_eq!(fs::metadata(&path).unwrap().ino(), inode);
        }
        assert!(write_owner_only_atomic_if_changed(&path, b"two").unwrap());
        assert_eq!(fs::read(&path).unwrap(), b"two");
        // A file that lost its owner-only mode is rewritten even when equal.
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).unwrap();
            assert!(write_owner_only_atomic_if_changed(&path, b"two").unwrap());
            assert_eq!(
                fs::metadata(&path).unwrap().permissions().mode() & 0o777,
                0o600
            );
        }
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn replace_file_swaps_content_and_creates_a_missing_target() {
        let dir = temp_dir("replace");
        ensure_owner_only_dir(&dir).unwrap();
        let target = dir.join("state.json");
        let first = dir.join("first.tmp");
        fs::write(&first, b"one").unwrap();
        replace_file(&first, &target).unwrap();
        assert_eq!(fs::read(&target).unwrap(), b"one");
        let before = FileStamp::read(&target).unwrap();
        let second = dir.join("second.tmp");
        fs::write(&second, b"two!").unwrap();
        replace_file(&second, &target).unwrap();
        assert_eq!(fs::read(&target).unwrap(), b"two!");
        assert_ne!(FileStamp::read(&target).unwrap(), before);
        assert!(!first.exists() && !second.exists());
        fs::remove_dir_all(dir).unwrap();
    }

    /// The target never disappears while it is being replaced, which a
    /// delete-then-rename on Windows violated.
    #[test]
    fn replaced_file_is_always_present_to_readers() {
        let dir = temp_dir("replace-race");
        ensure_owner_only_dir(&dir).unwrap();
        let target = dir.join("manifest.json");
        write_owner_only_atomic(&target, b"0").unwrap();
        let done = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let reader = {
            let (target, done) = (target.clone(), done.clone());
            std::thread::spawn(move || {
                let mut missing = 0;
                while !done.load(std::sync::atomic::Ordering::Relaxed) {
                    if fs::metadata(&target).is_err() {
                        missing += 1;
                    }
                }
                missing
            })
        };
        for round in 0..300 {
            write_owner_only_atomic(&target, round.to_string().as_bytes()).unwrap();
        }
        done.store(true, std::sync::atomic::Ordering::Relaxed);
        assert_eq!(reader.join().unwrap(), 0);
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn exclusive_lock_waits_for_the_holder_and_times_out() {
        let dir = temp_dir("lock");
        let lock = lock_path(&dir.join("state.json"));
        assert_eq!(lock.file_name().unwrap(), "state.json.lock");
        // The parent is created on demand.
        let held = lock_exclusive(&lock, Duration::from_millis(100)).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                fs::metadata(&lock).unwrap().permissions().mode() & 0o777,
                0o600
            );
        }
        let started = Instant::now();
        let error = lock_exclusive(&lock, Duration::from_millis(50)).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::TimedOut);
        assert!(started.elapsed() >= Duration::from_millis(50));

        let waiter = {
            let lock = lock.clone();
            std::thread::spawn(move || lock_exclusive(&lock, Duration::from_secs(5)).is_ok())
        };
        std::thread::sleep(Duration::from_millis(50));
        assert!(!waiter.is_finished(), "the lock is still held");
        drop(held);
        assert!(waiter.join().unwrap());
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
