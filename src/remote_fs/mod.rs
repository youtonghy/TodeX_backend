//! Remote file browsing over SFTP (system OpenSSH) and FTP/FTPS.
//!
//! Connections are in-memory sessions owned by [`RemoteSessions`]; nothing
//! about them is persisted, and passwords are dropped once a connection is
//! established. Paths are absolute POSIX paths on the remote side and are
//! normalized lexically; the remote account's own permissions are the only
//! access boundary.

pub(crate) mod askpass;
mod ftp;
mod sessions;
mod sftp;

use std::time::{SystemTime, UNIX_EPOCH};

use async_trait::async_trait;
use axum::body::Bytes;
use serde::Serialize;
use tokio::sync::mpsc;

use crate::error::AppError;

pub(crate) use ftp::FtpFs;
pub(crate) use sessions::{OpenedBy, RemoteSessions};
pub(crate) use sftp::SftpFs;

/// Directory listings are cut off after this many entries.
pub(crate) const MAX_LIST_ENTRIES: usize = 5000;
/// Upper bound for one uploaded or downloaded file.
pub(crate) const MAX_TRANSFER_BYTES: u64 = 100 * 1024 * 1024;
const MAX_PATH_BYTES: usize = 4096;
/// Chunk size used when streaming downloads.
const STREAM_CHUNK_BYTES: usize = 64 * 1024;

#[derive(Clone, Copy, Debug, Serialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub(crate) enum EntryKind {
    File,
    Directory,
    Symlink,
}

#[derive(Clone, Debug, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub(crate) struct RemoteEntry {
    pub name: String,
    pub path: String,
    pub kind: EntryKind,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub size_bytes: Option<u64>,
    /// Milliseconds since the Unix epoch.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub modified_at: Option<u64>,
    /// `rwxr-xr-x` style, when the server reports it.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub permissions: Option<String>,
}

/// Metadata of a path, following symlinks where the protocol allows it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct RemoteStat {
    pub kind: EntryKind,
    pub size: u64,
}

#[derive(Debug)]
pub(crate) struct Listing {
    pub entries: Vec<RemoteEntry>,
    pub truncated: bool,
}

/// One authenticated remote file system connection. Calls are serialized by
/// the session manager, so implementations may keep per-connection state.
/// A download client that stops reading must not hold its session (and
/// the session lock) forever.
const CHUNK_SEND_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(60);

/// Queues one download chunk. False when the receiver is gone or has not
/// accepted data for [`CHUNK_SEND_TIMEOUT`]; the transfer then stops.
pub(crate) async fn send_chunk(sink: &mpsc::Sender<std::io::Result<Bytes>>, bytes: Bytes) -> bool {
    matches!(
        tokio::time::timeout(CHUNK_SEND_TIMEOUT, sink.send(Ok(bytes))).await,
        Ok(Ok(()))
    )
}

#[async_trait]
pub(crate) trait RemoteFs: Send {
    /// Absolute directory the connection starts in.
    async fn home_dir(&mut self) -> Result<String, AppError>;
    async fn list(&mut self, path: &str) -> Result<Listing, AppError>;
    /// `Ok(None)` when the path does not exist.
    async fn stat(&mut self, path: &str) -> Result<Option<RemoteStat>, AppError>;
    /// Whole file content; fails when it exceeds `max` bytes.
    async fn read(&mut self, path: &str, max: u64) -> Result<Vec<u8>, AppError>;
    /// Replaces the content of an existing file, atomically where supported.
    async fn replace(&mut self, path: &str, data: &[u8]) -> Result<(), AppError>;
    /// Writes `data` at `offset`; offset 0 creates or truncates the file,
    /// any other offset must equal the current size (append).
    async fn write_at(&mut self, path: &str, offset: u64, data: &[u8]) -> Result<(), AppError>;
    /// Streams at most `limit` bytes of the file into `sink`. Stops early,
    /// without error, when the receiver goes away.
    async fn download(
        &mut self,
        path: &str,
        limit: u64,
        sink: &mpsc::Sender<std::io::Result<Bytes>>,
    ) -> Result<(), AppError>;
    async fn rename(&mut self, from: &str, to: &str) -> Result<(), AppError>;
    /// Removes a file, symlink or empty directory; never recursive.
    async fn remove(&mut self, path: &str) -> Result<(), AppError>;
    async fn mkdir(&mut self, path: &str) -> Result<(), AppError>;
    /// Ends the connection and releases its process or sockets.
    async fn close(&mut self);
    /// `false` once the underlying transport is known to be gone.
    fn is_connected(&mut self) -> bool {
        true
    }
}

/// Validates an absolute POSIX path and resolves `.` and `..` lexically.
pub(crate) fn normalize_path(input: &str) -> Result<String, AppError> {
    let invalid = |message: &str| AppError::InvalidRequest(message.to_owned());
    if input.is_empty() || input.len() > MAX_PATH_BYTES {
        return Err(invalid("path must be 1-4096 bytes"));
    }
    if input.contains(['\0', '\n', '\r']) {
        return Err(invalid("path must not contain NUL or line breaks"));
    }
    if !input.starts_with('/') {
        return Err(invalid("path must be absolute"));
    }
    let mut segments: Vec<&str> = Vec::new();
    for segment in input.split('/') {
        match segment {
            "" | "." => {}
            ".." => {
                segments.pop();
            }
            segment => segments.push(segment),
        }
    }
    Ok(format!("/{}", segments.join("/")))
}

/// Parent of a normalized path; `None` for the root.
pub(crate) fn parent_path(path: &str) -> Option<String> {
    if path == "/" {
        return None;
    }
    match path.rfind('/') {
        Some(0) => Some("/".to_owned()),
        Some(index) => Some(path[..index].to_owned()),
        None => None,
    }
}

pub(crate) fn file_name(path: &str) -> &str {
    path.rsplit('/').next().unwrap_or(path)
}

fn join_path(dir: &str, name: &str) -> String {
    if dir.ends_with('/') {
        format!("{dir}{name}")
    } else {
        format!("{dir}/{name}")
    }
}

/// Names a listing may expose: no `.`/`..`, separators or line breaks.
fn listable_name(name: &str) -> bool {
    !name.is_empty() && name != "." && name != ".." && !name.contains(['/', '\0', '\n', '\r'])
}

/// Directories first, then by name (case-insensitive, then exact).
pub(crate) fn sort_entries(entries: &mut [RemoteEntry]) {
    entries.sort_by(|a, b| {
        let dir = |entry: &RemoteEntry| entry.kind != EntryKind::Directory;
        dir(a)
            .cmp(&dir(b))
            .then_with(|| a.name.to_lowercase().cmp(&b.name.to_lowercase()))
            .then_with(|| a.name.cmp(&b.name))
    });
}

/// Sorts and truncates raw entries into a listing.
fn finish_listing(mut entries: Vec<RemoteEntry>) -> Listing {
    sort_entries(&mut entries);
    let truncated = entries.len() > MAX_LIST_ENTRIES;
    entries.truncate(MAX_LIST_ENTRIES);
    Listing { entries, truncated }
}

/// `rwxr-xr-x` from the low nine mode bits.
fn permission_string(mode: u32) -> String {
    const FLAGS: [(u32, char); 9] = [
        (0o400, 'r'),
        (0o200, 'w'),
        (0o100, 'x'),
        (0o040, 'r'),
        (0o020, 'w'),
        (0o010, 'x'),
        (0o004, 'r'),
        (0o002, 'w'),
        (0o001, 'x'),
    ];
    FLAGS
        .iter()
        .map(|(bit, flag)| if mode & bit != 0 { *flag } else { '-' })
        .collect()
}

fn system_time_millis(time: SystemTime) -> Option<u64> {
    time.duration_since(UNIX_EPOCH)
        .ok()
        .map(|duration| duration.as_millis() as u64)
        .filter(|millis| *millis > 0)
}

pub(crate) fn now_millis() -> u64 {
    system_time_millis(SystemTime::now()).unwrap_or_default()
}

/// Reclassifies an operation error as a lost connection when the
/// transport has died underneath it.
pub(crate) fn check_connection(fs: &mut dyn RemoteFs, error: AppError) -> AppError {
    if matches!(error, AppError::RemoteUnreachable(_)) || fs.is_connected() {
        error
    } else {
        AppError::RemoteUnreachable("the connection to the remote host was lost".to_owned())
    }
}

fn too_large(limit: u64) -> AppError {
    AppError::InvalidRequest(format!("file is larger than {limit} bytes"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalizes_absolute_paths_lexically() {
        assert_eq!(normalize_path("/").unwrap(), "/");
        assert_eq!(normalize_path("/a/./b//c/").unwrap(), "/a/b/c");
        assert_eq!(normalize_path("/a/../../b/..").unwrap(), "/");
        assert_eq!(normalize_path("/home/me/../you").unwrap(), "/home/you");
        assert_eq!(normalize_path("/C:/Users").unwrap(), "/C:/Users");
        for bad in ["", "relative", "~/x", "/a\nb", "/a\0b", "/a\rb"] {
            assert!(
                matches!(normalize_path(bad), Err(AppError::InvalidRequest(_))),
                "{bad:?}"
            );
        }
        assert!(normalize_path(&format!("/{}", "a".repeat(MAX_PATH_BYTES))).is_err());
    }

    #[test]
    fn derives_parent_and_name() {
        assert_eq!(parent_path("/"), None);
        assert_eq!(parent_path("/a").as_deref(), Some("/"));
        assert_eq!(parent_path("/a/b").as_deref(), Some("/a"));
        assert_eq!(file_name("/a/b.txt"), "b.txt");
        assert_eq!(join_path("/", "x"), "/x");
        assert_eq!(join_path("/a", "x"), "/a/x");
        assert!(!listable_name(".."));
        assert!(!listable_name("a/b"));
        assert!(listable_name(".hidden"));
    }

    fn entry(name: &str, kind: EntryKind) -> RemoteEntry {
        RemoteEntry {
            name: name.into(),
            path: join_path("/", name),
            kind,
            size_bytes: None,
            modified_at: None,
            permissions: None,
        }
    }

    #[test]
    fn sorts_directories_first_then_names() {
        let listing = finish_listing(vec![
            entry("b.txt", EntryKind::File),
            entry("link", EntryKind::Symlink),
            entry("Zeta", EntryKind::Directory),
            entry("alpha", EntryKind::Directory),
            entry("A.txt", EntryKind::File),
        ]);
        let names: Vec<&str> = listing.entries.iter().map(|e| e.name.as_str()).collect();
        assert_eq!(names, ["alpha", "Zeta", "A.txt", "b.txt", "link"]);
        assert!(!listing.truncated);

        let many = (0..MAX_LIST_ENTRIES + 3)
            .map(|i| entry(&format!("f{i}"), EntryKind::File))
            .collect();
        let listing = finish_listing(many);
        assert!(listing.truncated);
        assert_eq!(listing.entries.len(), MAX_LIST_ENTRIES);
    }

    #[test]
    fn formats_permissions() {
        assert_eq!(permission_string(0o755), "rwxr-xr-x");
        assert_eq!(permission_string(0o100644), "rw-r--r--");
    }
}
