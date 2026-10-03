//! SFTP through the system `ssh` binary: `ssh -s -- <alias> sftp` with the
//! child's stdin/stdout as the SFTP stream. OpenSSH keeps doing config, keys,
//! agents, ProxyJump, `known_hosts` and connection sharing.

use std::{
    process::Stdio,
    sync::{Arc, Mutex},
    time::Duration,
};

use async_trait::async_trait;
use axum::body::Bytes;
use russh_sftp::{
    client::{error::Error as SftpError, fs::Metadata, SftpSession},
    protocol::{FileAttributes, FileType, OpenFlags, StatusCode},
};
use tokio::{
    io::{AsyncReadExt, AsyncSeekExt, AsyncWriteExt},
    process::{Child, ChildStderr},
    sync::mpsc,
    task::JoinHandle,
};
use zeroize::Zeroizing;

use super::{
    askpass, file_name, finish_listing, join_path, listable_name, parent_path, permission_string,
    system_time_millis, too_large, EntryKind, Listing, RemoteEntry, RemoteFs, RemoteStat,
    STREAM_CHUNK_BYTES,
};
use crate::{
    error::AppError,
    external_command::bounded_text,
    ssh::{classify_failure, SshFailureKind, SshMode, SshService},
};

/// Covers the TCP connect, authentication and the SFTP version handshake.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(30);
/// Per-request SFTP response timeout.
const REQUEST_TIMEOUT_SECS: u64 = 30;
const STDERR_LIMIT: usize = 8 * 1024;
const EXIT_GRACE: Duration = Duration::from_secs(2);

pub(crate) struct SftpFs {
    sftp: SftpSession,
    child: Child,
    stderr_task: JoinHandle<()>,
}

impl SftpFs {
    /// Starts `ssh` for `alias` (which must be a known host) and performs the
    /// SFTP handshake. With a password, ssh may ask exactly once via the
    /// askpass helper; the password is gone once this returns.
    pub(crate) async fn connect(
        ssh: &SshService,
        alias: &str,
        password: Option<Zeroizing<String>>,
    ) -> Result<Self, AppError> {
        if password.is_some() {
            // Jump hosts and proxy programs inherit ssh's environment, and a
            // jump host's own password prompt would receive this secret.
            let resolved = ssh.resolved_host(alias).await?;
            if resolved.proxy_jump.is_some() || resolved.proxy_command.is_some() {
                return Err(AppError::InvalidRequest(
                    "password sign-in is not supported for hosts behind ProxyJump or \
                     ProxyCommand; log in from the TodeX Terminal first and the file \
                     session reuses that connection"
                        .to_owned(),
                ));
            }
        }
        let mut command = match &password {
            Some(password) => {
                let program = std::env::current_exe().map_err(|error| {
                    AppError::Unsupported(format!("askpass helper unavailable: {error}"))
                })?;
                let mut command = ssh.command(SshMode::Askpass);
                command
                    .env("SSH_ASKPASS", program)
                    .env(askpass::SECRET_ENV, password.as_str());
                command
            }
            None => ssh.command(SshMode::Batch),
        };
        drop(password);
        command
            .args([
                "-o",
                "RemoteCommand=none",
                "-o",
                "RequestTTY=no",
                "-o",
                "ForwardAgent=no",
                "-o",
                "ForwardX11=no",
                "-s",
                "--",
            ])
            .arg(alias)
            .arg("sftp")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        #[cfg(unix)]
        {
            // Keep terminal signals aimed at the daemon away from ssh. Only
            // the ssh pid is ever killed: a ControlPersist master it spawns
            // must survive for other sessions.
            use std::os::unix::process::CommandExt;
            command.as_std_mut().process_group(0);
        }
        let mut child = command.spawn().map_err(|error| {
            if error.kind() == std::io::ErrorKind::NotFound {
                AppError::Unsupported("ssh executable not found".to_owned())
            } else {
                AppError::RemoteFailed(format!("ssh could not be started: {error}"))
            }
        })?;
        // The child's copy of the environment is all that held the secret.
        drop(command);
        let stderr = Arc::new(Mutex::new(Vec::new()));
        let stderr_task = tokio::spawn(collect_stderr(child.stderr.take(), stderr.clone()));
        let (Some(stdout), Some(stdin)) = (child.stdout.take(), child.stdin.take()) else {
            stderr_task.abort();
            return Err(AppError::RemoteFailed("ssh pipes unavailable".to_owned()));
        };
        let session = tokio::time::timeout(
            CONNECT_TIMEOUT,
            SftpSession::new(tokio::io::join(stdout, stdin)),
        )
        .await;
        match session {
            Ok(Ok(sftp)) => {
                sftp.set_timeout(REQUEST_TIMEOUT_SECS);
                Ok(Self {
                    sftp,
                    child,
                    stderr_task,
                })
            }
            Ok(Err(error)) => {
                // ssh exits right after printing why; wait for it and its stderr.
                if tokio::time::timeout(EXIT_GRACE, child.wait())
                    .await
                    .is_err()
                {
                    let _ = child.kill().await;
                }
                let mut stderr_task = stderr_task;
                let _ = tokio::time::timeout(Duration::from_millis(500), &mut stderr_task).await;
                stderr_task.abort();
                let detail = stderr
                    .lock()
                    .map(|bytes| bounded_text(&bytes, STDERR_LIMIT))
                    .unwrap_or_default();
                Err(connect_error(alias, detail.trim(), Some(&error)))
            }
            Err(_elapsed) => {
                let _ = child.kill().await;
                stderr_task.abort();
                Err(AppError::RemoteUnreachable(format!(
                    "{alias}: SFTP connection timed out"
                )))
            }
        }
    }
}

impl Drop for SftpFs {
    fn drop(&mut self) {
        self.stderr_task.abort();
        // `kill_on_drop` reaps the ssh child itself.
    }
}

async fn collect_stderr(stderr: Option<ChildStderr>, sink: Arc<Mutex<Vec<u8>>>) {
    let Some(mut stderr) = stderr else {
        return;
    };
    let mut buffer = vec![0_u8; 4096];
    loop {
        match stderr.read(&mut buffer).await {
            Ok(0) | Err(_) => return,
            Ok(read) => {
                if let Ok(mut sink) = sink.lock() {
                    let room = STDERR_LIMIT.saturating_sub(sink.len());
                    sink.extend_from_slice(&buffer[..read.min(room)]);
                }
            }
        }
    }
}

/// Maps a failed ssh start to an API error. Messages carry ssh's own
/// diagnostics (host names, auth methods), never the password.
fn connect_error(alias: &str, stderr: &str, sftp_error: Option<&SftpError>) -> AppError {
    match classify_failure(stderr) {
        SshFailureKind::AuthenticationFailed => AppError::RemoteAuthFailed(format!(
            "{alias}: authentication failed; provide the password, or log in once via Terminal so the connection is reused"
        )),
        SshFailureKind::HostKeyUnverified => AppError::RemoteHostKeyUnverified(format!(
            "{alias}: the host key is not known; connect once in Terminal to confirm it"
        )),
        SshFailureKind::HostKeyChanged => AppError::RemoteHostKeyUnverified(format!(
            "{alias}: the host key changed since it was confirmed; verify the server before updating known_hosts"
        )),
        SshFailureKind::Unreachable | SshFailureKind::TimedOut => {
            AppError::RemoteUnreachable(format!("{alias}: {stderr}"))
        }
        SshFailureKind::Other => {
            let detail = if stderr.is_empty() {
                sftp_error.map(ToString::to_string).unwrap_or_default()
            } else {
                stderr.to_owned()
            };
            AppError::RemoteFailed(format!("{alias}: SFTP session could not start: {detail}"))
        }
    }
}

/// Maps SFTP status replies onto API errors; transport failures mean the
/// ssh process is gone.
fn map_error(path: &str, error: SftpError) -> AppError {
    match error {
        SftpError::Status(status) => match status.status_code {
            StatusCode::NoSuchFile => AppError::NotFound(path.to_owned()),
            StatusCode::PermissionDenied => AppError::RemotePermissionDenied(path.to_owned()),
            StatusCode::NoConnection | StatusCode::ConnectionLost => {
                AppError::RemoteUnreachable(status.error_message)
            }
            _ => AppError::RemoteFailed(format!("{path}: {}", status.error_message)),
        },
        SftpError::IO(message) => AppError::RemoteUnreachable(message),
        SftpError::Timeout => AppError::RemoteUnreachable("SFTP request timed out".to_owned()),
        other => AppError::RemoteFailed(format!("{path}: {other}")),
    }
}

fn io_error(path: &str, error: std::io::Error) -> AppError {
    match error.into_inner() {
        Some(inner) => match inner.downcast::<SftpError>() {
            Ok(sftp) => map_error(path, *sftp),
            Err(other) => AppError::RemoteFailed(format!("{path}: {other}")),
        },
        None => AppError::RemoteUnreachable(format!("{path}: connection lost")),
    }
}

fn not_empty(path: &str) -> AppError {
    AppError::Conflict(format!("{path} is not empty or cannot be removed"))
}

fn kind_of(metadata: &Metadata) -> EntryKind {
    match metadata.file_type() {
        FileType::Dir => EntryKind::Directory,
        FileType::Symlink => EntryKind::Symlink,
        FileType::File | FileType::Other => EntryKind::File,
    }
}

#[async_trait]
impl RemoteFs for SftpFs {
    async fn home_dir(&mut self) -> Result<String, AppError> {
        let home = self
            .sftp
            .canonicalize(".")
            .await
            .map_err(|error| map_error(".", error))?;
        Ok(if home.starts_with('/') {
            home
        } else {
            "/".to_owned()
        })
    }

    async fn list(&mut self, path: &str) -> Result<Listing, AppError> {
        let entries = self
            .sftp
            .read_dir(path)
            .await
            .map_err(|error| map_error(path, error))?
            .filter_map(|entry| {
                let name = entry.file_name();
                if !listable_name(&name) {
                    return None;
                }
                let metadata = entry.metadata();
                let kind = kind_of(&metadata);
                Some(RemoteEntry {
                    path: join_path(path, &name),
                    name,
                    kind,
                    size_bytes: (kind == EntryKind::File).then_some(metadata.size).flatten(),
                    modified_at: metadata.modified().ok().and_then(system_time_millis),
                    permissions: metadata.permissions.map(permission_string),
                })
            })
            .collect();
        Ok(finish_listing(entries))
    }

    async fn stat(&mut self, path: &str) -> Result<Option<RemoteStat>, AppError> {
        match self.sftp.metadata(path).await {
            Ok(metadata) => Ok(Some(RemoteStat {
                kind: kind_of(&metadata),
                size: metadata.len(),
            })),
            Err(SftpError::Status(status)) if status.status_code == StatusCode::NoSuchFile => {
                Ok(None)
            }
            Err(error) => Err(map_error(path, error)),
        }
    }

    async fn read(&mut self, path: &str, max: u64) -> Result<Vec<u8>, AppError> {
        let file = self
            .sftp
            .open(path)
            .await
            .map_err(|error| map_error(path, error))?;
        let mut bytes = Vec::new();
        let mut limited = file.take(max + 1);
        limited
            .read_to_end(&mut bytes)
            .await
            .map_err(|error| io_error(path, error))?;
        let _ = limited.into_inner().close().await;
        if bytes.len() as u64 > max {
            return Err(too_large(max));
        }
        Ok(bytes)
    }

    async fn replace(&mut self, path: &str, data: &[u8]) -> Result<(), AppError> {
        let current = self
            .sftp
            .symlink_metadata(path)
            .await
            .map_err(|error| map_error(path, error))?;
        // Replacing a symlink by rename would turn it into a regular file.
        if current.file_type() == FileType::Symlink {
            return self.write_at(path, 0, data).await;
        }
        let parent = parent_path(path).unwrap_or_else(|| "/".to_owned());
        let temporary = join_path(
            &parent,
            &format!(
                ".{}.todex-save-{}",
                file_name(path),
                uuid::Uuid::new_v4().simple()
            ),
        );
        let attributes = FileAttributes {
            permissions: current.permissions.map(|mode| mode & 0o7777),
            ..FileAttributes::empty()
        };
        let result = async {
            let mut file = self
                .sftp
                .open_with_flags_and_attributes(
                    temporary.as_str(),
                    OpenFlags::CREATE | OpenFlags::EXCLUDE | OpenFlags::WRITE,
                    attributes,
                )
                .await
                .map_err(|error| map_error(path, error))?;
            file.write_all(data)
                .await
                .map_err(|error| io_error(path, error))?;
            file.close().await.map_err(|error| io_error(path, error))?;
            self.rename_over(&temporary, path).await
        }
        .await;
        if result.is_err() {
            let _ = self.sftp.remove_file(temporary.as_str()).await;
        }
        result
    }

    async fn write_at(&mut self, path: &str, offset: u64, data: &[u8]) -> Result<(), AppError> {
        let flags = if offset == 0 {
            OpenFlags::CREATE | OpenFlags::TRUNCATE | OpenFlags::WRITE
        } else {
            OpenFlags::WRITE
        };
        let mut file = self
            .sftp
            .open_with_flags(path, flags)
            .await
            .map_err(|error| map_error(path, error))?;
        if offset > 0 {
            file.seek(std::io::SeekFrom::Start(offset))
                .await
                .map_err(|error| io_error(path, error))?;
        }
        file.write_all(data)
            .await
            .map_err(|error| io_error(path, error))?;
        file.close().await.map_err(|error| io_error(path, error))
    }

    async fn download(
        &mut self,
        path: &str,
        limit: u64,
        sink: &mpsc::Sender<std::io::Result<Bytes>>,
    ) -> Result<(), AppError> {
        let file = self
            .sftp
            .open(path)
            .await
            .map_err(|error| map_error(path, error))?;
        let mut reader = file.take(limit);
        let mut buffer = vec![0_u8; STREAM_CHUNK_BYTES];
        loop {
            let read = reader
                .read(&mut buffer)
                .await
                .map_err(|error| io_error(path, error))?;
            if read == 0 {
                break;
            }
            if !super::send_chunk(sink, Bytes::copy_from_slice(&buffer[..read])).await {
                break;
            }
        }
        let _ = reader.into_inner().close().await;
        Ok(())
    }

    async fn rename(&mut self, from: &str, to: &str) -> Result<(), AppError> {
        self.sftp
            .rename(from, to)
            .await
            .map_err(|error| map_error(from, error))
    }

    async fn remove(&mut self, path: &str) -> Result<(), AppError> {
        let metadata = self
            .sftp
            .symlink_metadata(path)
            .await
            .map_err(|error| map_error(path, error))?;
        if metadata.file_type() != FileType::Dir {
            return self
                .sftp
                .remove_file(path)
                .await
                .map_err(|error| map_error(path, error));
        }
        // SFTP v3 reports a non-empty directory only as a generic failure.
        self.sftp
            .remove_dir(path)
            .await
            .map_err(|error| match map_error(path, error) {
                AppError::RemoteFailed(_) => not_empty(path),
                other => other,
            })
    }

    async fn mkdir(&mut self, path: &str) -> Result<(), AppError> {
        self.sftp
            .create_dir(path)
            .await
            .map_err(|error| map_error(path, error))
    }

    async fn close(&mut self) {
        let _ = self.sftp.close().await;
        // Closing the stream ends ssh; kill it if it lingers.
        if tokio::time::timeout(EXIT_GRACE, self.child.wait())
            .await
            .is_err()
        {
            let _ = self.child.kill().await;
        }
        self.stderr_task.abort();
    }

    fn is_connected(&mut self) -> bool {
        matches!(self.child.try_wait(), Ok(None))
    }
}

impl SftpFs {
    /// SFTP v3 `rename` refuses existing targets on OpenSSH. Move the old
    /// file aside first and put it back if the second rename fails.
    async fn rename_over(&mut self, from: &str, to: &str) -> Result<(), AppError> {
        if self.sftp.rename(from, to).await.is_ok() {
            return Ok(());
        }
        let backup = format!("{from}.old");
        self.sftp
            .rename(to, backup.as_str())
            .await
            .map_err(|error| map_error(to, error))?;
        match self.sftp.rename(from, to).await {
            Ok(()) => {
                let _ = self.sftp.remove_file(backup.as_str()).await;
                Ok(())
            }
            Err(error) => {
                let _ = self.sftp.rename(backup.as_str(), to).await;
                Err(map_error(to, error))
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use russh_sftp::protocol::Status;

    fn status(code: StatusCode) -> SftpError {
        SftpError::Status(Status {
            id: 1,
            status_code: code,
            error_message: "server says".into(),
            language_tag: String::new(),
        })
    }

    #[test]
    fn maps_sftp_errors() {
        assert!(matches!(
            map_error("/x", status(StatusCode::NoSuchFile)),
            AppError::NotFound(_)
        ));
        assert!(matches!(
            map_error("/x", status(StatusCode::PermissionDenied)),
            AppError::RemotePermissionDenied(_)
        ));
        assert!(matches!(
            map_error("/x", status(StatusCode::Failure)),
            AppError::RemoteFailed(_)
        ));
        assert!(matches!(
            map_error("/x", SftpError::IO("broken pipe".into())),
            AppError::RemoteUnreachable(_)
        ));
        assert!(matches!(
            io_error("/x", std::io::Error::from(status(StatusCode::NoSuchFile))),
            AppError::NotFound(_)
        ));
    }

    #[test]
    fn maps_ssh_start_failures_without_leaking_detail_into_auth_errors() {
        let error = connect_error(
            "db",
            "alice@db: Permission denied (publickey,password).",
            None,
        );
        assert_eq!(error.code(), "REMOTE_AUTH_FAILED");
        assert!(error.to_string().contains("Terminal"));
        let error = connect_error("db", "Host key verification failed.", None);
        assert_eq!(error.code(), "REMOTE_HOST_KEY_UNVERIFIED");
        let error = connect_error(
            "db",
            "ssh: connect to host db port 22: Connection refused",
            None,
        );
        assert_eq!(error.code(), "REMOTE_UNREACHABLE");
        let error = connect_error("db", "subsystem request failed on channel 0", None);
        assert_eq!(error.code(), "REMOTE_OPERATION_FAILED");
    }

    /// Real roundtrip against `TODEX_REMOTE_FS_SFTP_TEST=<alias>`, an ssh
    /// host reachable with key/agent auth. `TODEX_REMOTE_FS_SFTP_HOME`
    /// optionally points at a home whose `.ssh/config` defines the alias
    /// (for a throwaway local sshd); otherwise `$HOME` is used read-only.
    /// `TODEX_REMOTE_FS_SFTP_DIR` picks the remote scratch directory.
    #[cfg(unix)]
    #[tokio::test]
    async fn sftp_roundtrip_against_real_host() {
        let Ok(alias) = std::env::var("TODEX_REMOTE_FS_SFTP_TEST") else {
            return;
        };
        let home = std::env::var_os("TODEX_REMOTE_FS_SFTP_HOME")
            .or_else(|| std::env::var_os("HOME"))
            .map(std::path::PathBuf::from);
        let data =
            std::env::temp_dir().join(format!("todex-sftp-{}", uuid::Uuid::new_v4().simple()));
        let ssh = SshService::with_home(&data, "ssh".into(), home)
            .await
            .unwrap();
        ssh.require_host(&alias).await.unwrap();
        let mut fs = SftpFs::connect(&ssh, &alias, None).await.unwrap();

        let home = fs.home_dir().await.unwrap();
        assert!(home.starts_with('/'));
        // Scratch location on the remote side; defaults to the remote home.
        let base = std::env::var("TODEX_REMOTE_FS_SFTP_DIR").unwrap_or(home);
        let dir = join_path(
            &base,
            &format!("todex-sftp-test-{}", uuid::Uuid::new_v4().simple()),
        );
        fs.mkdir(&dir).await.unwrap();
        let file = join_path(&dir, "a.txt");
        fs.write_at(&file, 0, b"hello ").await.unwrap();
        fs.write_at(&file, 6, b"world").await.unwrap();
        assert_eq!(fs.read(&file, 1024).await.unwrap(), b"hello world");
        assert!(matches!(
            fs.read(&file, 4).await,
            Err(AppError::InvalidRequest(_))
        ));
        // Replace goes through a temporary sibling and a rename over the file.
        fs.replace(&file, b"replaced").await.unwrap();
        assert_eq!(fs.read(&file, 1024).await.unwrap(), b"replaced");

        let renamed = join_path(&dir, "b.txt");
        fs.rename(&file, &renamed).await.unwrap();
        fs.mkdir(&join_path(&dir, "sub")).await.unwrap();
        let listing = fs.list(&dir).await.unwrap();
        let names: Vec<(&str, EntryKind)> = listing
            .entries
            .iter()
            .map(|entry| (entry.name.as_str(), entry.kind))
            .collect();
        assert_eq!(
            names,
            [("sub", EntryKind::Directory), ("b.txt", EntryKind::File)]
        );
        assert_eq!(listing.entries[1].size_bytes, Some(8));
        assert!(listing.entries[1].permissions.is_some());
        assert_eq!(
            fs.stat(&renamed).await.unwrap(),
            Some(RemoteStat {
                kind: EntryKind::File,
                size: 8
            })
        );
        assert_eq!(fs.stat(&file).await.unwrap(), None);

        let (tx, mut rx) = mpsc::channel(4);
        fs.download(&renamed, 8, &tx).await.unwrap();
        drop(tx);
        let mut downloaded = Vec::new();
        while let Some(chunk) = rx.recv().await {
            downloaded.extend_from_slice(&chunk.unwrap());
        }
        assert_eq!(downloaded, b"replaced");

        // Non-empty directories are never removed.
        assert!(fs.remove(&dir).await.is_err());
        fs.remove(&renamed).await.unwrap();
        fs.remove(&join_path(&dir, "sub")).await.unwrap();
        fs.remove(&dir).await.unwrap();
        assert!(matches!(fs.list(&dir).await, Err(AppError::NotFound(_))));

        fs.close().await;
        assert!(
            fs.child.try_wait().unwrap().is_some(),
            "ssh child must exit"
        );
        let _ = std::fs::remove_dir_all(data);
    }
}
