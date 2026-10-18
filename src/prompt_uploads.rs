//! Files the clients attach to a prompt (`PUT /v2/uploads`).
//!
//! The composer uploads dropped and pasted files, and long pasted text as
//! Markdown, to `/tmp/todex/file/<uploadId>/<name>` on the daemon's host, then
//! names that path in the prompt: the client may be on another machine, so a
//! path on its own disk would mean nothing to the agent. Prompt content may
//! point inside this directory as well as the workspace, and the provider
//! drivers grant the agents read access to it.
//!
//! Uploads stay until [`cleanup_expired`] removes them a week later, so a
//! retried or queued prompt still finds its files.

use std::{
    io,
    path::{Path, PathBuf},
    time::{Duration, SystemTime},
};

use tokio::io::{AsyncSeekExt, AsyncWriteExt};
use uuid::Uuid;

use crate::error::AppError;

/// Uploads arrive in chunks so each signed request body stays small.
pub(crate) const MAX_UPLOAD_CHUNK_BYTES: usize = 8 * 1024 * 1024;
pub(crate) const MAX_UPLOAD_BYTES: u64 = 100 * 1024 * 1024;
/// How long an upload is kept after its last write.
pub(crate) const UPLOAD_RETENTION: Duration = Duration::from_secs(7 * 24 * 60 * 60);
pub(crate) const CLEANUP_INTERVAL: Duration = Duration::from_secs(6 * 60 * 60);
const MAX_NAME_BYTES: usize = 200;

/// `/tmp/todex/file` on Unix; `%TEMP%\todex\file` elsewhere.
pub(crate) fn upload_root() -> PathBuf {
    #[cfg(unix)]
    let temp = PathBuf::from("/tmp");
    #[cfg(not(unix))]
    let temp = std::env::temp_dir();
    temp.join("todex").join("file")
}

/// A stored upload, as `PUT /v2/uploads` reports it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct StoredUpload {
    pub upload_id: String,
    pub path: PathBuf,
    pub name: String,
    pub size_bytes: u64,
}

/// Creates `root` and its parent owner-only. Fails when either is a symlink
/// or belongs to another user, since `/tmp` is shared: a directory someone
/// else made there could read or swap the uploaded files.
pub(crate) async fn ensure_root(root: &Path) -> io::Result<PathBuf> {
    let root = root.to_path_buf();
    tokio::task::spawn_blocking(move || {
        if let Some(parent) = root.parent() {
            ensure_private_dir(parent)?;
        }
        ensure_private_dir(&root)?;
        std::fs::canonicalize(&root)
    })
    .await
    .map_err(io::Error::other)?
}

fn ensure_private_dir(path: &Path) -> io::Result<()> {
    match std::fs::create_dir(path) {
        Ok(()) => {}
        Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            std::fs::create_dir_all(path)?;
        }
        Err(error) => return Err(error),
    }
    let metadata = std::fs::symlink_metadata(path)?;
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        return Err(io::Error::other(format!(
            "{} is not a directory",
            path.display()
        )));
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::{MetadataExt, PermissionsExt};
        if metadata.uid() != unsafe { libc::geteuid() } {
            return Err(io::Error::other(format!(
                "{} belongs to another user",
                path.display()
            )));
        }
        if metadata.permissions().mode() & 0o077 != 0 {
            std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700))?;
        }
    }
    Ok(())
}

/// The file name an upload is stored under: the last path component of
/// `name` without control characters, at most [`MAX_NAME_BYTES`] bytes,
/// keeping the extension when shortened.
pub(crate) fn sanitize_name(name: &str) -> String {
    let base = name
        .rsplit(['/', '\\'])
        .next()
        .unwrap_or_default()
        .chars()
        .filter(|c| !c.is_control())
        .collect::<String>();
    let base = base.trim().trim_start_matches('.').trim();
    if base.is_empty() {
        return "file".to_owned();
    }
    if base.len() <= MAX_NAME_BYTES {
        return base.to_owned();
    }
    let (stem, extension) = match base.rfind('.') {
        Some(dot) if base.len() - dot <= 16 => base.split_at(dot),
        _ => (base, ""),
    };
    let mut stem = stem.to_owned();
    while stem.len() + extension.len() > MAX_NAME_BYTES {
        stem.pop();
    }
    format!("{stem}{extension}")
}

fn valid_upload_id(id: &str) -> bool {
    Uuid::parse_str(id).is_ok_and(|uuid| uuid.simple().to_string() == id)
}

/// Writes one chunk. `offset` 0 without an `upload_id` starts a new upload;
/// later chunks name the upload and must start where the stored file ends.
pub(crate) async fn write_chunk(
    root: &Path,
    upload_id: Option<&str>,
    name: &str,
    offset: u64,
    bytes: &[u8],
) -> Result<StoredUpload, AppError> {
    if bytes.len() > MAX_UPLOAD_CHUNK_BYTES {
        return Err(AppError::InvalidRequest(format!(
            "upload chunks must be at most {MAX_UPLOAD_CHUNK_BYTES} bytes"
        )));
    }
    let end = offset.saturating_add(bytes.len() as u64);
    if end > MAX_UPLOAD_BYTES {
        return Err(AppError::InvalidRequest(format!(
            "uploads may be at most {MAX_UPLOAD_BYTES} bytes"
        )));
    }
    let name = sanitize_name(name);
    let root = ensure_root(root).await.map_err(|error| {
        AppError::Io(io::Error::other(format!(
            "the upload directory is unavailable: {error}"
        )))
    })?;
    let (upload_id, created) = match upload_id {
        None if offset == 0 => (Uuid::new_v4().simple().to_string(), true),
        None => {
            return Err(AppError::InvalidRequest(
                "uploadId is required after the first chunk".to_owned(),
            ))
        }
        Some(id) if valid_upload_id(id) => (id.to_owned(), false),
        Some(_) => {
            return Err(AppError::InvalidRequest("uploadId is invalid".to_owned()));
        }
    };
    let dir = root.join(&upload_id);
    let path = dir.join(&name);
    if created {
        tokio::fs::create_dir(&dir).await?;
    }
    let mut options = tokio::fs::OpenOptions::new();
    options.write(true);
    if created {
        options.create_new(true);
    }
    #[cfg(unix)]
    options.mode(0o600);
    let mut file = match options.open(&path).await {
        Ok(file) => file,
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            return Err(AppError::NotFound("upload not found".to_owned()));
        }
        Err(error) => return Err(error.into()),
    };
    let size = file.metadata().await?.len();
    if size != offset {
        return Err(AppError::Conflict(format!(
            "upload offset {offset} does not match the stored size {size}"
        )));
    }
    file.seek(io::SeekFrom::Start(offset)).await?;
    file.write_all(bytes).await?;
    file.flush().await?;
    Ok(StoredUpload {
        upload_id,
        path,
        name,
        size_bytes: end,
    })
}

/// Whether `canonical` (already canonicalized) lies inside an upload.
pub(crate) async fn contains(canonical: &Path) -> bool {
    match tokio::fs::canonicalize(upload_root()).await {
        Ok(root) => canonical.starts_with(&root) && canonical != root,
        Err(_) => false,
    }
}

/// The upload directory, created if needed, for the provider drivers to
/// grant read access to; `None` when it cannot be used safely.
pub(crate) async fn readable_root() -> Option<PathBuf> {
    match ensure_root(&upload_root()).await {
        Ok(root) => Some(root),
        Err(error) => {
            tracing::warn!(%error, "the prompt upload directory is unavailable");
            None
        }
    }
}

/// Removes uploads last written before `cutoff`. Returns how many it removed.
pub(crate) async fn cleanup_expired(root: &Path, cutoff: SystemTime) -> io::Result<usize> {
    let mut entries = match tokio::fs::read_dir(root).await {
        Ok(entries) => entries,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(0),
        Err(error) => return Err(error),
    };
    let mut removed = 0;
    while let Some(entry) = entries.next_entry().await? {
        let Some(name) = entry.file_name().to_str().map(str::to_owned) else {
            continue;
        };
        if !valid_upload_id(&name) {
            continue;
        }
        let metadata = tokio::fs::symlink_metadata(entry.path()).await?;
        if !metadata.is_dir() {
            continue;
        }
        let mut newest = metadata.modified()?;
        let mut files = tokio::fs::read_dir(entry.path()).await?;
        while let Some(file) = files.next_entry().await? {
            if let Ok(modified) = file.metadata().await.and_then(|meta| meta.modified()) {
                newest = newest.max(modified);
            }
        }
        if newest < cutoff {
            tokio::fs::remove_dir_all(entry.path()).await?;
            removed += 1;
        }
    }
    Ok(removed)
}

/// Runs [`cleanup_expired`] now and every [`CLEANUP_INTERVAL`].
pub(crate) fn spawn_cleanup() -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let mut ticker = tokio::time::interval(CLEANUP_INTERVAL);
        loop {
            ticker.tick().await;
            let cutoff = SystemTime::now() - UPLOAD_RETENTION;
            match cleanup_expired(&upload_root(), cutoff).await {
                Ok(0) => {}
                Ok(removed) => tracing::info!(removed, "removed expired prompt uploads"),
                Err(error) => tracing::warn!(%error, "prompt upload cleanup failed"),
            }
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_root() -> PathBuf {
        std::env::temp_dir()
            .join(format!("todex-uploads-{}", Uuid::new_v4()))
            .join("file")
    }

    #[test]
    fn names_are_reduced_to_a_safe_file_name() {
        assert_eq!(sanitize_name("../../etc/passwd"), "passwd");
        assert_eq!(sanitize_name("C:\\Users\\a\\报告.pdf"), "报告.pdf");
        assert_eq!(sanitize_name("..hidden"), "hidden");
        assert_eq!(sanitize_name("a\u{0}b\nc.txt"), "abc.txt");
        assert_eq!(sanitize_name(""), "file");
        assert_eq!(sanitize_name(".."), "file");
        let long = format!("{}.md", "x".repeat(400));
        let short = sanitize_name(&long);
        assert!(short.len() <= MAX_NAME_BYTES);
        assert!(short.ends_with(".md"));
    }

    #[tokio::test]
    async fn chunks_append_in_order_and_reject_gaps() {
        let root = temp_root();
        let first = write_chunk(&root, None, "notes.md", 0, b"hello ")
            .await
            .unwrap();
        assert_eq!(first.size_bytes, 6);
        assert_eq!(first.name, "notes.md");
        assert!(first
            .path
            .ends_with(format!("{}/notes.md", first.upload_id)));

        let gap = write_chunk(&root, Some(&first.upload_id), "notes.md", 9, b"x").await;
        assert!(matches!(gap, Err(AppError::Conflict(_))));

        let second = write_chunk(&root, Some(&first.upload_id), "notes.md", 6, b"world")
            .await
            .unwrap();
        assert_eq!(second.path, first.path);
        assert_eq!(second.size_bytes, 11);
        assert_eq!(std::fs::read(&first.path).unwrap(), b"hello world");

        let unknown = Uuid::new_v4().simple().to_string();
        let missing = write_chunk(&root, Some(&unknown), "a", 1, b"x").await;
        assert!(matches!(missing, Err(AppError::NotFound(_))));
        let traversal = write_chunk(&root, Some("../x"), "a", 1, b"x").await;
        assert!(matches!(traversal, Err(AppError::InvalidRequest(_))));
        let without_id = write_chunk(&root, None, "a", 4, b"x").await;
        assert!(matches!(without_id, Err(AppError::InvalidRequest(_))));
        let too_large = write_chunk(&root, Some(&first.upload_id), "a", MAX_UPLOAD_BYTES, b"x");
        assert!(matches!(too_large.await, Err(AppError::InvalidRequest(_))));

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = |path: &Path| std::fs::metadata(path).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode(&root), 0o700);
            assert_eq!(mode(root.parent().unwrap()), 0o700);
            assert_eq!(mode(&first.path), 0o600);
        }
        std::fs::remove_dir_all(root.parent().unwrap()).unwrap();
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn a_symlinked_root_is_refused() {
        let base = std::env::temp_dir().join(format!("todex-uploads-{}", Uuid::new_v4()));
        let elsewhere = base.join("elsewhere");
        std::fs::create_dir_all(&elsewhere).unwrap();
        std::os::unix::fs::symlink(&elsewhere, base.join("file")).unwrap();
        assert!(ensure_root(&base.join("file")).await.is_err());
        std::fs::remove_dir_all(&base).unwrap();
    }

    #[tokio::test]
    async fn cleanup_removes_only_expired_uploads() {
        let root = temp_root();
        let upload = write_chunk(&root, None, "a.txt", 0, b"a").await.unwrap();
        std::fs::write(root.join("not-an-upload"), b"keep").unwrap();

        let past = SystemTime::now() - Duration::from_secs(60);
        assert_eq!(cleanup_expired(&root, past).await.unwrap(), 0);
        assert!(upload.path.exists());

        let future = SystemTime::now() + Duration::from_secs(60);
        assert_eq!(cleanup_expired(&root, future).await.unwrap(), 1);
        assert!(!upload.path.exists());
        assert!(root.join("not-an-upload").exists());
        std::fs::remove_dir_all(root.parent().unwrap()).unwrap();
    }
}
