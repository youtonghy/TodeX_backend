//! Private folders of private (RAM-only) conversations.
//!
//! What a private conversation cannot avoid putting on a filesystem — its
//! prompt uploads, the agent CLI's own leftovers (Pi subagent sessions,
//! Codex's state and log databases) — goes into one owner-only folder per
//! conversation:
//!
//! `<root>/<namespace>/<conversationId>/`
//!
//! `root` prefers memory-backed storage: `$XDG_RUNTIME_DIR/todex-private`
//! when that is a directory, else `/dev/shm/todex-private-<uid>` on Linux,
//! else `<temp dir>/todex-private`. `namespace` is derived from the daemon's
//! data directory, so daemons with different data directories never touch
//! each other's folders. A conversation's folder is removed when the
//! conversation is deleted and when its agent process ends for good; the
//! daemon's whole namespace is swept at startup, since a restart forgets
//! every private conversation.

use std::io;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use sha2::{Digest, Sha256};

/// Where private folders live and whether that storage is memory-backed.
#[derive(Clone, Debug)]
pub(crate) struct PrivateRoot {
    /// `<root>/<namespace>`.
    base: PathBuf,
    /// The root is on a RAM filesystem (tmpfs / ramfs). Swap may still page
    /// it out; that is reported in `docs/API.md`.
    memory_backed: bool,
}

static ROOT: OnceLock<PrivateRoot> = OnceLock::new();

impl PrivateRoot {
    fn for_data_dir(data_dir: &Path) -> Self {
        let root = preferred_root();
        let memory_backed = is_memory_backed(&root);
        let canonical = std::fs::canonicalize(data_dir).unwrap_or_else(|_| data_dir.to_owned());
        let digest = Sha256::digest(canonical.to_string_lossy().as_bytes());
        let namespace = digest[..8]
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect::<String>();
        Self {
            base: root.join(namespace),
            memory_backed,
        }
    }

    /// `"memory"` when the folders sit on a RAM filesystem, else `"disk"`.
    pub(crate) fn storage(&self) -> &'static str {
        if self.memory_backed {
            "memory"
        } else {
            "disk"
        }
    }

    /// Whether the root and the namespace folder are real directories of
    /// this user (or missing), so removing below them cannot follow a
    /// symlink planted in a shared folder such as `/dev/shm`. Logs refusals.
    fn base_is_trusted(&self) -> bool {
        let Some(root) = self.base.parent() else {
            return false;
        };
        for path in [root, self.base.as_path()] {
            match owned_directory(path) {
                Ok(_) => {}
                Err(error) => {
                    tracing::warn!(
                        path = %path.display(),
                        error = %error,
                        "refusing to clean up an untrusted private conversation folder"
                    );
                    return false;
                }
            }
        }
        true
    }

    /// The conversation's folder (not created).
    pub(crate) fn conversation_path(&self, conversation_id: &str) -> PathBuf {
        self.base.join(conversation_id)
    }

    /// The conversation's folder, created owner-only with its parents.
    /// Fails when any of them is a symlink or belongs to another user.
    pub(crate) fn ensure_conversation_dir(&self, conversation_id: &str) -> io::Result<PathBuf> {
        validate_conversation_id(conversation_id)?;
        let root = self
            .base
            .parent()
            .ok_or_else(|| io::Error::other("the private folder root has no parent"))?;
        crate::prompt_uploads::ensure_private_dir(root)?;
        crate::prompt_uploads::ensure_private_dir(&self.base)?;
        let folder = self.conversation_path(conversation_id);
        crate::prompt_uploads::ensure_private_dir(&folder)?;
        Ok(folder)
    }

    /// `name` inside the conversation's folder, created owner-only.
    pub(crate) fn ensure_subdir(&self, conversation_id: &str, name: &str) -> io::Result<PathBuf> {
        let folder = self.ensure_conversation_dir(conversation_id)?.join(name);
        crate::prompt_uploads::ensure_private_dir(&folder)?;
        Ok(folder)
    }

    /// Removes the conversation's folder; a missing one is fine.
    pub(crate) fn remove_conversation(&self, conversation_id: &str) {
        if validate_conversation_id(conversation_id).is_err() {
            return;
        }
        if !self.base_is_trusted() {
            return;
        }
        let folder = self.conversation_path(conversation_id);
        match std::fs::remove_dir_all(&folder) {
            Ok(()) => {}
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => tracing::warn!(
                conversation_id,
                error = %error,
                "could not remove the private conversation folder"
            ),
        }
    }

    /// Removes every folder of this daemon's namespace; run at startup,
    /// when no private conversation exists yet. Returns how many it removed.
    pub(crate) fn sweep(&self) -> usize {
        if !self.base_is_trusted() {
            return 0;
        }
        let entries = match std::fs::read_dir(&self.base) {
            Ok(entries) => entries,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return 0,
            Err(error) => {
                tracing::warn!(error = %error, "could not list leftover private conversation folders");
                return 0;
            }
        };
        let mut removed = 0;
        for entry in entries.flatten() {
            let path = entry.path();
            let result = match entry.file_type() {
                Ok(kind) if kind.is_dir() => std::fs::remove_dir_all(&path),
                Ok(_) => std::fs::remove_file(&path),
                Err(error) => Err(error),
            };
            match result {
                Ok(()) => removed += 1,
                Err(error) => tracing::warn!(
                    error = %error,
                    "could not remove a leftover private conversation folder"
                ),
            }
        }
        removed
    }

    /// Whether `canonical` (already canonicalized) is a file in the
    /// conversation's private uploads folder.
    pub(crate) fn contains_upload(&self, conversation_id: &str, canonical: &Path) -> bool {
        if validate_conversation_id(conversation_id).is_err() {
            return false;
        }
        let uploads = self.conversation_path(conversation_id).join(UPLOADS_DIR);
        match std::fs::canonicalize(&uploads) {
            Ok(root) => canonical.starts_with(&root) && canonical != root,
            Err(_) => false,
        }
    }
}

/// Points a private conversation's agent process at its private folder for
/// temporary files: `TMPDIR` (and `TMP` / `TEMP`), which CLIs and their
/// extensions use for scratch output (pi-subagents task output, shell
/// tool temp files). Returns the folder.
pub(crate) fn private_temp_env(
    spec: &mut crate::provider::process::CommandSpec,
    conversation_id: &str,
) -> Result<PathBuf, crate::error::AppError> {
    let folder = root()
        .ensure_subdir(conversation_id, "tmp")
        .map_err(|error| {
            crate::error::AppError::Io(io::Error::other(format!(
                "the private conversation folder is unavailable: {error}"
            )))
        })?;
    let value = folder.to_string_lossy().into_owned();
    for key in ["TMPDIR", "TMP", "TEMP"] {
        spec.env.insert(key.to_owned(), value.clone());
    }
    Ok(folder)
}

/// Subfolder of a conversation's folder holding its prompt uploads.
pub(crate) const UPLOADS_DIR: &str = "uploads";

/// The daemon's private folder root; set from the data directory at startup
/// ([`init`]). Before that (unit tests) a namespace of the temp directory is
/// used.
pub(crate) fn root() -> &'static PrivateRoot {
    ROOT.get_or_init(|| PrivateRoot::for_data_dir(&std::env::temp_dir().join("todex-unscoped")))
}

/// Fixes the namespace for `data_dir` and removes what an earlier run of
/// this daemon left behind. A second call keeps the first namespace.
#[cfg_attr(test, allow(dead_code))]
pub(crate) fn init(data_dir: &Path) -> &'static PrivateRoot {
    let mut fresh = false;
    let root = ROOT.get_or_init(|| {
        fresh = true;
        PrivateRoot::for_data_dir(data_dir)
    });
    if fresh {
        let removed = root.sweep();
        if removed > 0 {
            tracing::info!(removed, "removed leftover private conversation folders");
        }
        if !root.memory_backed {
            tracing::warn!(
                path = %root.base.display(),
                "private conversation folders are not on a RAM filesystem"
            );
        }
    }
    root
}

/// `Ok(true)` when `path` is a directory (not a symlink) owned by this
/// user, `Ok(false)` when it does not exist, an error otherwise.
fn owned_directory(path: &Path) -> io::Result<bool> {
    let metadata = match std::fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(false),
        Err(error) => return Err(error),
    };
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        return Err(io::Error::other("not a directory"));
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        // SAFETY: geteuid has no preconditions.
        if metadata.uid() != unsafe { libc::geteuid() } {
            return Err(io::Error::other("belongs to another user"));
        }
    }
    Ok(true)
}

fn validate_conversation_id(conversation_id: &str) -> io::Result<()> {
    match uuid::Uuid::parse_str(conversation_id) {
        Ok(parsed) if parsed.hyphenated().to_string() == conversation_id => Ok(()),
        _ => Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "conversation id must be a UUID",
        )),
    }
}

fn preferred_root() -> PathBuf {
    if let Some(runtime) = std::env::var_os("XDG_RUNTIME_DIR").map(PathBuf::from) {
        if runtime.is_absolute() && runtime.is_dir() {
            return runtime.join("todex-private");
        }
    }
    #[cfg(target_os = "linux")]
    {
        let shm = Path::new("/dev/shm");
        if shm.is_dir() {
            // SAFETY: geteuid has no preconditions.
            let uid = unsafe { libc::geteuid() };
            return shm.join(format!("todex-private-{uid}"));
        }
    }
    std::env::temp_dir().join("todex-private")
}

/// Whether the filesystem holding `path` (or its nearest existing parent)
/// keeps its files in memory.
fn is_memory_backed(path: &Path) -> bool {
    #[cfg(target_os = "linux")]
    {
        use std::os::unix::ffi::OsStrExt;
        const TMPFS_MAGIC: i64 = 0x0102_1994;
        const RAMFS_MAGIC: i64 = 0x8584_58f6;
        let mut probe = path;
        while !probe.exists() {
            match probe.parent() {
                Some(parent) => probe = parent,
                None => return false,
            }
        }
        let Ok(c_path) = std::ffi::CString::new(probe.as_os_str().as_bytes()) else {
            return false;
        };
        // SAFETY: `c_path` is NUL-terminated and `stat` is a valid out
        // buffer for the duration of the call.
        let mut stat: libc::statfs = unsafe { std::mem::zeroed() };
        if unsafe { libc::statfs(c_path.as_ptr(), &mut stat) } != 0 {
            return false;
        }
        #[allow(clippy::unnecessary_cast)]
        let kind = stat.f_type as i64;
        kind == TMPFS_MAGIC || kind == RAMFS_MAGIC
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = path;
        false
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn root_in(dir: &Path) -> PrivateRoot {
        PrivateRoot {
            base: dir.join("ns"),
            memory_backed: false,
        }
    }

    #[test]
    fn folders_are_owner_only_and_removed() {
        let temp =
            std::env::temp_dir().join(format!("todex-private-test-{}", uuid::Uuid::new_v4()));
        let root = root_in(&temp);
        let id = uuid::Uuid::new_v4().to_string();
        let uploads = root.ensure_subdir(&id, UPLOADS_DIR).unwrap();
        std::fs::write(uploads.join("a.txt"), b"x").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            for path in [&root.base, &root.conversation_path(&id), &uploads] {
                let mode = std::fs::metadata(path).unwrap().permissions().mode();
                assert_eq!(mode & 0o077, 0, "{}", path.display());
            }
        }
        let file = std::fs::canonicalize(uploads.join("a.txt")).unwrap();
        assert!(root.contains_upload(&id, &file));
        assert!(!root.contains_upload(&uuid::Uuid::new_v4().to_string(), &file));
        root.remove_conversation(&id);
        assert!(!root.conversation_path(&id).exists());
        assert!(root.ensure_conversation_dir("../escape").is_err());
        let _ = std::fs::remove_dir_all(temp);
    }

    #[test]
    fn sweep_removes_only_this_namespace() {
        let temp =
            std::env::temp_dir().join(format!("todex-private-test-{}", uuid::Uuid::new_v4()));
        let root = root_in(&temp);
        let other = PrivateRoot {
            base: temp.join("other"),
            memory_backed: false,
        };
        let first = uuid::Uuid::new_v4().to_string();
        let second = uuid::Uuid::new_v4().to_string();
        root.ensure_subdir(&first, "pi-sessions").unwrap();
        root.ensure_conversation_dir(&second).unwrap();
        other.ensure_conversation_dir(&first).unwrap();
        assert_eq!(root.sweep(), 2);
        assert!(!root.conversation_path(&first).exists());
        assert!(other.conversation_path(&first).exists());
        let _ = std::fs::remove_dir_all(temp);
    }

    #[cfg(unix)]
    #[test]
    fn sweep_and_remove_never_follow_a_symlinked_namespace() {
        let temp =
            std::env::temp_dir().join(format!("todex-private-test-{}", uuid::Uuid::new_v4()));
        let victim = temp.join("victim");
        let id = uuid::Uuid::new_v4().to_string();
        std::fs::create_dir_all(victim.join(&id)).unwrap();
        std::fs::write(victim.join("keep.txt"), "keep").unwrap();
        std::fs::create_dir_all(temp.join("root")).unwrap();
        std::os::unix::fs::symlink(&victim, temp.join("root").join("ns")).unwrap();
        let root = root_in(&temp.join("root"));
        assert_eq!(root.sweep(), 0);
        root.remove_conversation(&id);
        assert!(victim.join("keep.txt").exists());
        assert!(victim.join(&id).exists());
        let _ = std::fs::remove_dir_all(temp);
    }

    #[cfg(unix)]
    #[test]
    fn a_symlinked_namespace_is_refused() {
        let temp =
            std::env::temp_dir().join(format!("todex-private-test-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(temp.join("target")).unwrap();
        std::os::unix::fs::symlink(temp.join("target"), temp.join("ns")).unwrap();
        let root = root_in(&temp);
        assert!(root
            .ensure_conversation_dir(&uuid::Uuid::new_v4().to_string())
            .is_err());
        let _ = std::fs::remove_dir_all(temp);
    }
}
