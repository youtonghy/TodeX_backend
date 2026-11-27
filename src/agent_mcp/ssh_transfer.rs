//! `ssh_upload` and `ssh_download`: whole files between the TodeX machine
//! and an agent SSH host, over SFTP on the same OpenSSH config and shared
//! connection as `ssh_exec`. The bytes never pass through the agent: it
//! names a local path and the daemon streams the file itself, so the size
//! is bounded by [`MAX_TRANSFER_BYTES`], not by MCP messages or stdin.
//!
//! Files land under a temporary sibling name first and are moved into place
//! only once the size (and, when the host has `sha256sum`/`shasum`, the
//! SHA-256) matches; replacing a running binary therefore never leaves a
//! truncated file or fails with "Text file busy".

use std::{
    path::{Component, Path, PathBuf},
    time::Duration,
};

use serde::Deserialize;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use tokio::io::AsyncWriteExt;

use super::registry::{parse, Prepared};
use crate::{
    error::AppError,
    external_command::{self, prepare_captured, CommandLimits},
    remote_fs::{file_name, normalize_path, parent_path, RemoteFs, SftpFs, MAX_TRANSFER_BYTES},
    ssh::{SshFailureKind, SshMode, SshService},
};

const DEFAULT_TIMEOUT_SECONDS: u64 = 300;
const MAX_PATH_BYTES: usize = 4096;
/// The remote checksum of a whole file; generous for slow disks.
const REMOTE_HASH_TIMEOUT: Duration = Duration::from_secs(120);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Direction {
    Upload,
    Download,
}

impl Direction {
    fn as_str(self) -> &'static str {
        match self {
            Self::Upload => "upload",
            Self::Download => "download",
        }
    }
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct UploadArgs {
    host: String,
    local_path: String,
    remote_path: String,
    #[serde(default)]
    mode: Option<String>,
    #[serde(default)]
    overwrite: bool,
    #[serde(default)]
    sha256: Option<String>,
    #[serde(default)]
    timeout_sec: Option<u64>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct DownloadArgs {
    host: String,
    remote_path: String,
    local_path: String,
    #[serde(default)]
    overwrite: bool,
    #[serde(default)]
    timeout_sec: Option<u64>,
}

/// A validated `ssh_upload` or `ssh_download`.
#[derive(Debug)]
pub(super) struct TransferRequest {
    pub direction: Direction,
    pub host: String,
    /// As given: absolute, or relative to the conversation workspace.
    pub local_path: String,
    /// As given: absolute, `~/…`, or relative to the remote home.
    pub remote_path: String,
    /// Upload only: permission bits; `None` keeps the local file's.
    pub mode: Option<u32>,
    pub overwrite: bool,
    /// Upload only: the SHA-256 the agent expects the local file to have.
    pub expected_sha256: Option<String>,
    pub timeout: Duration,
}

impl TransferRequest {
    /// One line for the transfer's view: `local → host:remote` (or back).
    pub(super) fn display(&self) -> String {
        match self.direction {
            Direction::Upload => format!(
                "upload {} → {}:{}",
                self.local_path, self.host, self.remote_path
            ),
            Direction::Download => format!(
                "download {}:{} → {}",
                self.host, self.remote_path, self.local_path
            ),
        }
    }

    /// `transfer` of the `ssh.exec.started` event.
    pub(super) fn started_payload(&self, local: &Path) -> Value {
        json!({
            "direction": self.direction.as_str(),
            "localPath": local.display().to_string(),
            "remotePath": self.remote_path,
        })
    }
}

fn check_path(field: &str, value: &str) -> Result<(), String> {
    if value.trim().is_empty() {
        return Err(format!("{field} is required"));
    }
    if value.len() > MAX_PATH_BYTES || value.chars().any(char::is_control) {
        return Err(format!(
            "{field} must be at most {MAX_PATH_BYTES} bytes without control characters"
        ));
    }
    Ok(())
}

fn check_host(host: &str) -> Result<String, String> {
    let host = host.trim();
    if host.is_empty() {
        return Err("host is required".to_owned());
    }
    if crate::ssh::is_agent_local_alias(host) {
        return Err(format!(
            "{host} is the TodeX machine itself; copy files there with your own file tools \
             or ssh_exec instead"
        ));
    }
    Ok(host.to_owned())
}

fn timeout(value: Option<u64>) -> Result<Duration, String> {
    match value {
        None => Ok(Duration::from_secs(DEFAULT_TIMEOUT_SECONDS)),
        Some(seconds @ 1..=super::server::MAX_TIMEOUT_SECONDS) => Ok(Duration::from_secs(seconds)),
        Some(_) => Err(format!(
            "timeoutSec must be between 1 and {}",
            super::server::MAX_TIMEOUT_SECONDS
        )),
    }
}

/// `"755"`, `"0755"` or `"0o755"` → permission bits.
fn parse_mode(value: &str) -> Result<u32, String> {
    let digits = value.trim();
    let digits = digits.strip_prefix("0o").unwrap_or(digits);
    u32::from_str_radix(digits, 8)
        .ok()
        .filter(|mode| !digits.is_empty() && *mode <= 0o7777)
        .ok_or_else(|| format!("mode must be octal permission bits like \"755\" (got {value:?})"))
}

fn parse_sha256(value: &str) -> Result<String, String> {
    let hash = value.trim().to_ascii_lowercase();
    if hash.len() == 64 && hash.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        Ok(hash)
    } else {
        Err("sha256 must be 64 hexadecimal characters".to_owned())
    }
}

pub(super) fn validate_upload(arguments: Value) -> Result<TransferRequest, String> {
    let args: UploadArgs = parse("ssh_upload", arguments)?;
    check_path("localPath", &args.local_path)?;
    check_path("remotePath", &args.remote_path)?;
    Ok(TransferRequest {
        direction: Direction::Upload,
        host: check_host(&args.host)?,
        local_path: args.local_path,
        remote_path: args.remote_path,
        mode: args.mode.as_deref().map(parse_mode).transpose()?,
        overwrite: args.overwrite,
        expected_sha256: args.sha256.as_deref().map(parse_sha256).transpose()?,
        timeout: timeout(args.timeout_sec)?,
    })
}

pub(super) fn validate_download(arguments: Value) -> Result<TransferRequest, String> {
    let args: DownloadArgs = parse("ssh_download", arguments)?;
    check_path("remotePath", &args.remote_path)?;
    check_path("localPath", &args.local_path)?;
    Ok(TransferRequest {
        direction: Direction::Download,
        host: check_host(&args.host)?,
        local_path: args.local_path,
        remote_path: args.remote_path,
        mode: None,
        overwrite: args.overwrite,
        expected_sha256: None,
        timeout: timeout(args.timeout_sec)?,
    })
}

/// What the approver sees: both paths, and for uploads the local size, so a
/// file the user did not expect to leave the machine stands out.
pub(super) fn prepared<C>(
    request: TransferRequest,
    call: impl FnOnce(TransferRequest) -> C,
) -> Prepared<C> {
    let mut input = json!({
        "host": request.host,
        "localPath": request.local_path,
        "remotePath": request.remote_path,
        "overwrite": request.overwrite,
    });
    if let Some(mode) = request.mode {
        input["mode"] = json!(format!("{mode:o}"));
    }
    if request.direction == Direction::Upload {
        // Relative paths resolve in the workspace only when the call runs;
        // an absolute one shows its size already.
        let path = Path::new(&request.local_path);
        if let Some(metadata) = path
            .is_absolute()
            .then(|| std::fs::metadata(path).ok())
            .flatten()
        {
            input["sizeBytes"] = json!(metadata.len());
        }
    }
    Prepared {
        // An upload reads any file the TodeX user can read, outside the
        // sandbox the provider may run its own tools in.
        escapes_sandbox: request.direction == Direction::Upload,
        summary: request.display(),
        input,
        call: call(request),
    }
}

/// `path` resolved against `workspace` (absolute paths stay as given),
/// with `.` and `..` resolved lexically.
pub(super) fn local_path(workspace: &Path, path: &str) -> PathBuf {
    let path = Path::new(path);
    let joined = if path.is_absolute() {
        path.to_path_buf()
    } else {
        workspace.join(path)
    };
    let mut normal = PathBuf::new();
    for component in joined.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                normal.pop();
            }
            other => normal.push(other),
        }
    }
    normal
}

/// A finished transfer.
#[derive(Debug)]
pub(super) struct TransferReport {
    pub local_path: PathBuf,
    pub remote_path: String,
    pub size: u64,
    pub sha256: String,
    /// Computed on the host; `None` when it has neither `sha256sum` nor
    /// `shasum` (the size was still checked).
    pub remote_sha256: Option<String>,
    /// An existing file was replaced.
    pub replaced: bool,
}

impl TransferReport {
    /// `transfer` of `ssh.exec.completed` and the agent's result.
    pub(super) fn payload(&self, direction: Direction) -> Value {
        json!({
            "direction": direction.as_str(),
            "localPath": self.local_path.display().to_string(),
            "remotePath": self.remote_path,
            "sizeBytes": self.size,
            "sha256": self.sha256,
            "remoteSha256": self.remote_sha256,
            "verified": if self.remote_sha256.is_some() { "sha256" } else { "size" },
            "replaced": self.replaced,
        })
    }
}

/// How a failed transfer is classified, as `ssh_exec` failures are.
pub(super) fn failure_kind(error: &AppError) -> SshFailureKind {
    match error {
        AppError::RemoteAuthFailed(_) => SshFailureKind::AuthenticationFailed,
        AppError::RemoteHostKeyUnverified(_) => SshFailureKind::HostKeyUnverified,
        AppError::RemoteUnreachable(_) => SshFailureKind::Unreachable,
        _ => SshFailureKind::Other,
    }
}

/// The message the agent and the user see for a failed transfer.
pub(super) fn failure_message(error: &AppError) -> String {
    match error {
        AppError::NotFound(path) => format!("{path} does not exist"),
        AppError::RemotePermissionDenied(path) => {
            format!("permission denied on the host for {path}")
        }
        AppError::Conflict(message) => format!("{message}; pass overwrite: true to replace it"),
        other => other.to_string(),
    }
}

/// Absolute remote path for `path`: `~/x` and relative paths start in the
/// SFTP home directory.
async fn remote_target(fs: &mut SftpFs, path: &str) -> Result<String, AppError> {
    let trimmed = path.trim();
    let absolute = if trimmed.starts_with('/') {
        trimmed.to_owned()
    } else {
        let relative = trimmed
            .strip_prefix("~/")
            .or_else(|| (trimmed == "~").then_some(""))
            .unwrap_or(trimmed);
        format!("{}/{relative}", fs.home_dir().await?)
    };
    normalize_path(&absolute)
}

fn temporary_name(directory: &str, name: &str) -> String {
    let leaf = format!(".{name}.todex-part-{}", uuid::Uuid::new_v4().simple());
    if directory.ends_with('/') {
        format!("{directory}{leaf}")
    } else {
        format!("{directory}/{leaf}")
    }
}

fn hex(digest: impl AsRef<[u8]>) -> String {
    digest
        .as_ref()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

/// Opens an SFTP session to `host`, as `ssh_exec` connects (BatchMode,
/// shared master); authentication failures come back as
/// [`AppError::RemoteAuthFailed`] for the caller's login retry.
pub(super) async fn connect(ssh: &SshService, host: &str) -> Result<SftpFs, AppError> {
    SftpFs::connect(ssh, host, None).await
}

/// Uploads `request.local_path` (resolved to `local`) over `fs`.
/// The local side of an upload, opened before connecting so a wrong path
/// fails without reaching the host.
pub(super) struct LocalSource {
    path: PathBuf,
    file: tokio::fs::File,
    metadata: std::fs::Metadata,
}

/// Opens `local` for [`upload`]: a regular file within the size limit.
pub(super) async fn open_upload(local: &Path) -> Result<LocalSource, AppError> {
    let file = tokio::fs::File::open(local).await.map_err(|error| {
        AppError::InvalidRequest(format!("cannot open {}: {error}", local.display()))
    })?;
    let metadata = file.metadata().await?;
    if !metadata.is_file() {
        return Err(AppError::InvalidRequest(format!(
            "{} is not a regular file",
            local.display()
        )));
    }
    if metadata.len() > MAX_TRANSFER_BYTES {
        return Err(AppError::InvalidRequest(format!(
            "{} is {} bytes; files may be at most {MAX_TRANSFER_BYTES} bytes",
            local.display(),
            metadata.len()
        )));
    }
    Ok(LocalSource {
        path: local.to_path_buf(),
        file,
        metadata,
    })
}

/// Uploads `source` over `fs` as `request` says.
pub(super) async fn upload(
    ssh: &SshService,
    fs: &mut SftpFs,
    request: &TransferRequest,
    source: LocalSource,
) -> Result<TransferReport, AppError> {
    let LocalSource {
        path: local,
        mut file,
        metadata,
    } = source;
    let local = local.as_path();
    let mode = request.mode.unwrap_or_else(|| local_mode(&metadata));
    let local_name = local
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_default();

    let mut target = remote_target(fs, &request.remote_path).await?;
    // scp semantics: a directory (or a path ending in `/`) receives the file
    // under its own name.
    let into_directory = request.remote_path.trim_end().ends_with('/')
        || matches!(fs.stat(&target).await?, Some(stat) if stat.kind == crate::remote_fs::EntryKind::Directory);
    if into_directory {
        if local_name.is_empty() {
            return Err(AppError::InvalidRequest(
                "remotePath is a directory and the local file has no name".to_owned(),
            ));
        }
        target = normalize_path(&format!("{target}/{local_name}"))?;
    }
    let existing = fs.stat(&target).await?;
    if let Some(stat) = existing {
        if stat.kind == crate::remote_fs::EntryKind::Directory {
            return Err(AppError::Conflict(format!("{target} is a directory")));
        }
        if !request.overwrite {
            return Err(AppError::Conflict(format!("{target} already exists")));
        }
    }
    let directory = parent_path(&target).unwrap_or_else(|| "/".to_owned());
    let temporary = temporary_name(&directory, file_name(&target));

    let mut hasher = Sha256::new();
    let result = async {
        let size = fs
            .create_from(
                &temporary,
                mode,
                &mut file,
                MAX_TRANSFER_BYTES,
                &mut |chunk| hasher.update(chunk),
            )
            .await?;
        let sha256 = hex(std::mem::take(&mut hasher).finalize());
        if let Some(expected) = &request.expected_sha256 {
            if *expected != sha256 {
                return Err(AppError::InvalidRequest(format!(
                    "the local file's SHA-256 is {sha256}, not the expected {expected}; \
                     nothing was replaced"
                )));
            }
        }
        match fs.stat(&temporary).await? {
            Some(stat) if stat.size == size => {}
            other => {
                return Err(AppError::RemoteFailed(format!(
                    "{temporary} holds {} bytes after sending {size}; nothing was replaced",
                    other.map_or(0, |stat| stat.size)
                )))
            }
        }
        let remote_sha256 = remote_sha256(ssh, &request.host, &temporary).await;
        if let Some(remote) = &remote_sha256 {
            if *remote != sha256 {
                return Err(AppError::RemoteFailed(format!(
                    "the uploaded file's SHA-256 on {} is {remote}, not {sha256}; nothing was \
                     replaced",
                    request.host
                )));
            }
        }
        fs.move_into_place(&temporary, &target, request.overwrite)
            .await?;
        Ok((size, sha256, remote_sha256))
    }
    .await;
    match result {
        Ok((size, sha256, remote_sha256)) => Ok(TransferReport {
            local_path: local.to_path_buf(),
            remote_path: target,
            size,
            sha256,
            remote_sha256,
            replaced: existing.is_some(),
        }),
        Err(error) => {
            // Best effort: the connection may be what failed.
            let _ = fs.remove(&temporary).await;
            Err(error)
        }
    }
}

#[cfg(unix)]
fn local_mode(metadata: &std::fs::Metadata) -> u32 {
    use std::os::unix::fs::PermissionsExt;
    metadata.permissions().mode() & 0o777
}

#[cfg(not(unix))]
fn local_mode(metadata: &std::fs::Metadata) -> u32 {
    if metadata.permissions().readonly() {
        0o444
    } else {
        0o644
    }
}

/// Downloads into `local`, which must lie inside `workspace`.
pub(super) async fn download(
    ssh: &SshService,
    fs: &mut SftpFs,
    request: &TransferRequest,
    workspace: &Path,
    local: &Path,
) -> Result<TransferReport, AppError> {
    let source = remote_target(fs, &request.remote_path).await?;
    match fs.stat(&source).await? {
        None => return Err(AppError::NotFound(source)),
        Some(stat) if stat.kind == crate::remote_fs::EntryKind::Directory => {
            return Err(AppError::InvalidRequest(format!(
                "{source} is a directory; only single files can be downloaded"
            )))
        }
        Some(stat) if stat.size > MAX_TRANSFER_BYTES => {
            return Err(AppError::InvalidRequest(format!(
                "{source} is {} bytes; files may be at most {MAX_TRANSFER_BYTES} bytes",
                stat.size
            )))
        }
        Some(_) => {}
    }
    let mut target = local.to_path_buf();
    let into_directory = request.local_path.ends_with('/')
        || request.local_path.ends_with(std::path::MAIN_SEPARATOR)
        || tokio::fs::metadata(&target)
            .await
            .is_ok_and(|metadata| metadata.is_dir());
    if into_directory {
        target.push(file_name(&source));
    }
    let target = inside_workspace(workspace, &target).await?;
    let existing = match tokio::fs::symlink_metadata(&target).await {
        Ok(metadata) if metadata.is_dir() => {
            return Err(AppError::Conflict(format!(
                "{} is a directory",
                target.display()
            )))
        }
        Ok(_) if !request.overwrite => {
            return Err(AppError::Conflict(format!(
                "{} already exists",
                target.display()
            )))
        }
        Ok(_) => true,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => false,
        Err(error) => return Err(error.into()),
    };
    let directory = target
        .parent()
        .ok_or_else(|| AppError::InvalidRequest("localPath has no parent directory".to_owned()))?;
    let name = target
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_default();
    let temporary = directory.join(format!(
        ".{name}.todex-part-{}",
        uuid::Uuid::new_v4().simple()
    ));
    let mut hasher = Sha256::new();
    let result = async {
        let mut file = tokio::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temporary)
            .await?;
        let size = fs
            .copy_to(&source, &mut file, MAX_TRANSFER_BYTES, &mut |chunk| {
                hasher.update(chunk)
            })
            .await?;
        file.flush().await?;
        file.sync_all().await?;
        drop(file);
        let sha256 = hex(std::mem::take(&mut hasher).finalize());
        let remote_sha256 = remote_sha256(ssh, &request.host, &source).await;
        if let Some(remote) = &remote_sha256 {
            if *remote != sha256 {
                return Err(AppError::RemoteFailed(format!(
                    "{source} on {} has SHA-256 {remote}, but {sha256} arrived (did it change \
                     during the download?); nothing was written",
                    request.host
                )));
            }
        }
        if request.overwrite {
            tokio::fs::rename(&temporary, &target).await?;
        } else {
            // A hard link fails when the target appeared meanwhile, so an
            // existing file is never clobbered.
            match tokio::fs::hard_link(&temporary, &target).await {
                Ok(()) => {
                    let _ = tokio::fs::remove_file(&temporary).await;
                }
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                    return Err(AppError::Conflict(format!(
                        "{} already exists",
                        target.display()
                    )))
                }
                Err(error) => return Err(error.into()),
            }
        }
        Ok((size, sha256, remote_sha256))
    }
    .await;
    match result {
        Ok((size, sha256, remote_sha256)) => Ok(TransferReport {
            local_path: target,
            remote_path: source,
            size,
            sha256,
            remote_sha256,
            replaced: existing,
        }),
        Err(error) => {
            let _ = tokio::fs::remove_file(&temporary).await;
            Err(error)
        }
    }
}

/// `target` when it lies inside `workspace`, checked on the real paths of
/// both (an existing parent directory is required), so symlinks cannot point
/// a download elsewhere.
async fn inside_workspace(workspace: &Path, target: &Path) -> Result<PathBuf, AppError> {
    let outside = || {
        AppError::InvalidRequest(format!(
            "localPath must be inside the conversation workspace {}",
            workspace.display()
        ))
    };
    let root = tokio::fs::canonicalize(workspace).await?;
    let parent = target.parent().ok_or_else(outside)?;
    let parent = tokio::fs::canonicalize(parent).await.map_err(|error| {
        if error.kind() == std::io::ErrorKind::NotFound {
            AppError::InvalidRequest(format!("the directory {} does not exist", parent.display()))
        } else {
            error.into()
        }
    })?;
    if !parent.starts_with(&root) {
        return Err(outside());
    }
    let name = target.file_name().ok_or_else(outside)?;
    let resolved = parent.join(name);
    // An existing symlink at the target could lead out of the workspace.
    if tokio::fs::symlink_metadata(&resolved)
        .await
        .is_ok_and(|metadata| metadata.file_type().is_symlink())
    {
        return Err(AppError::InvalidRequest(format!(
            "{} is a symbolic link; download to a regular path",
            resolved.display()
        )));
    }
    Ok(resolved)
}

/// SHA-256 of `path` on `host`, via `sha256sum` or `shasum`; `None` when
/// neither exists or the run fails.
async fn remote_sha256(ssh: &SshService, host: &str, path: &str) -> Option<String> {
    let quoted = shell_quote(path);
    let mut command = ssh.command(SshMode::Batch);
    command.arg("--").arg(host).arg(format!(
        "sha256sum -- {quoted} 2>/dev/null || shasum -a 256 -- {quoted}"
    ));
    prepare_captured(&mut command, false);
    let output = external_command::run(
        command,
        None,
        CommandLimits {
            timeout: REMOTE_HASH_TIMEOUT,
            output_limit: 64 * 1024,
        },
    )
    .await
    .ok()?;
    if !output.status.success() {
        return None;
    }
    parse_hash_line(&String::from_utf8_lossy(&output.stdout))
}

fn parse_hash_line(stdout: &str) -> Option<String> {
    let hash = stdout.split_whitespace().next()?.trim_start_matches('\\');
    parse_sha256(hash).ok()
}

fn shell_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', r"'\''"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn upload_args(extra: Value) -> Value {
        let mut base =
            json!({ "host": "web", "localPath": "target/app", "remotePath": "/opt/app" });
        if let (Value::Object(base), Value::Object(extra)) = (&mut base, extra) {
            base.extend(extra);
        }
        base
    }

    #[test]
    fn validates_upload_arguments() {
        let request = validate_upload(upload_args(
            json!({ "mode": "0755", "sha256": "AB".repeat(32) }),
        ))
        .unwrap();
        assert_eq!(request.mode, Some(0o755));
        assert_eq!(
            request.expected_sha256.as_deref(),
            Some("ab".repeat(32).as_str())
        );
        assert_eq!(
            request.timeout,
            Duration::from_secs(DEFAULT_TIMEOUT_SECONDS)
        );
        assert_eq!(request.display(), "upload target/app → web:/opt/app");

        for (extra, message) in [
            (json!({ "mode": "999" }), "mode"),
            (json!({ "mode": "17777" }), "mode"),
            (json!({ "sha256": "abc" }), "sha256"),
            (json!({ "timeoutSec": 0 }), "timeoutSec"),
            (json!({ "remotePath": "" }), "remotePath"),
            (json!({ "localPath": "a\nb" }), "localPath"),
            (json!({ "host": "local" }), "TodeX machine"),
            (json!({ "extra": true }), "unknown field"),
        ] {
            let error = validate_upload(upload_args(extra.clone())).unwrap_err();
            assert!(error.contains(message), "{extra}: {error}");
        }
    }

    #[test]
    fn approval_shows_both_paths_and_marks_uploads_as_leaving_the_sandbox() {
        let upload = validate_upload(upload_args(json!({ "overwrite": true }))).unwrap();
        let shown = prepared(upload, |_| ());
        assert!(shown.escapes_sandbox);
        assert_eq!(shown.input["localPath"], "target/app");
        assert_eq!(shown.input["remotePath"], "/opt/app");
        assert_eq!(shown.input["overwrite"], true);

        let download = validate_download(
            json!({ "host": "web", "remotePath": "/var/log/x", "localPath": "logs/" }),
        )
        .unwrap();
        let shown = prepared(download, |_| ());
        assert!(!shown.escapes_sandbox);
        assert_eq!(shown.summary, "download web:/var/log/x → logs/");
    }

    #[test]
    fn resolves_local_paths_in_the_workspace() {
        let workspace = Path::new("/w/project");
        assert_eq!(
            local_path(workspace, "a/./b"),
            PathBuf::from("/w/project/a/b")
        );
        assert_eq!(
            local_path(workspace, "../other/x"),
            PathBuf::from("/w/other/x")
        );
        assert_eq!(local_path(workspace, "/abs/x"), PathBuf::from("/abs/x"));
    }

    #[test]
    fn parses_checksum_tool_output() {
        let hash = "fbbe71c1164d78bad444557641e8e77f4df93c74ddb1a2c422c586a0692eccf3";
        assert_eq!(
            parse_hash_line(&format!("{hash}  /tmp/x\n")).as_deref(),
            Some(hash)
        );
        // GNU escapes names with backslashes or newlines with a leading `\`.
        assert_eq!(
            parse_hash_line(&format!("\\{hash}  /tmp/a\\nb\n")).as_deref(),
            Some(hash)
        );
        assert_eq!(parse_hash_line("sha256sum: not found"), None);
        assert_eq!(parse_hash_line(""), None);
    }

    #[test]
    fn names_failures_like_ssh_exec() {
        assert_eq!(
            failure_kind(&AppError::RemoteAuthFailed("x".into())),
            SshFailureKind::AuthenticationFailed
        );
        assert_eq!(
            failure_kind(&AppError::Conflict("x".into())),
            SshFailureKind::Other
        );
        assert!(
            failure_message(&AppError::Conflict("/opt/app already exists".into()))
                .contains("overwrite: true")
        );
    }

    #[tokio::test]
    async fn downloads_stay_inside_the_workspace() {
        let root =
            std::env::temp_dir().join(format!("todex-transfer-{}", uuid::Uuid::new_v4().simple()));
        let workspace = root.join("ws");
        std::fs::create_dir_all(workspace.join("sub")).unwrap();
        std::fs::create_dir_all(root.join("elsewhere")).unwrap();

        let inside = inside_workspace(&workspace, &workspace.join("sub/file"))
            .await
            .unwrap();
        assert!(inside.ends_with("sub/file"));
        for outside in [
            root.join("elsewhere/file"),
            local_path(&workspace, "../elsewhere/file"),
        ] {
            assert!(
                inside_workspace(&workspace, &outside).await.is_err(),
                "{}",
                outside.display()
            );
        }
        assert!(
            inside_workspace(&workspace, &workspace.join("missing/file"))
                .await
                .is_err()
        );
        #[cfg(unix)]
        {
            std::os::unix::fs::symlink(root.join("elsewhere"), workspace.join("escape")).unwrap();
            assert!(inside_workspace(&workspace, &workspace.join("escape/file"))
                .await
                .is_err());
            std::os::unix::fs::symlink(root.join("elsewhere/file"), workspace.join("link"))
                .unwrap();
            assert!(inside_workspace(&workspace, &workspace.join("link"))
                .await
                .is_err());
        }
        let _ = std::fs::remove_dir_all(root);
    }

    /// Real upload/download roundtrip against `TODEX_REMOTE_FS_SFTP_TEST`
    /// (see `remote_fs::sftp`'s roundtrip test for the variables).
    #[cfg(unix)]
    #[tokio::test]
    async fn transfer_roundtrip_against_real_host() {
        use std::os::unix::fs::PermissionsExt;
        let Ok(alias) = std::env::var("TODEX_REMOTE_FS_SFTP_TEST") else {
            return;
        };
        let home = std::env::var_os("TODEX_REMOTE_FS_SFTP_HOME")
            .or_else(|| std::env::var_os("HOME"))
            .map(PathBuf::from);
        let root = std::env::temp_dir().join(format!(
            "todex-transfer-real-{}",
            uuid::Uuid::new_v4().simple()
        ));
        let workspace = root.join("ws");
        std::fs::create_dir_all(&workspace).unwrap();
        let ssh = SshService::with_home(&root.join("data"), "ssh".into(), home)
            .await
            .unwrap();
        let mut fs = connect(&ssh, &alias).await.unwrap();
        let base = match std::env::var("TODEX_REMOTE_FS_SFTP_DIR") {
            Ok(dir) => dir,
            Err(_) => fs.home_dir().await.unwrap(),
        };
        let dir = format!(
            "{base}/todex-transfer-test-{}",
            uuid::Uuid::new_v4().simple()
        );
        fs.mkdir(&dir).await.unwrap();

        // A few MiB of varied bytes, executable, as a compiled binary is.
        let binary = workspace.join("app");
        let bytes: Vec<u8> = (0..3_000_000_u32)
            .map(|i| (i.wrapping_mul(2_654_435_761) >> 13) as u8)
            .collect();
        std::fs::write(&binary, &bytes).unwrap();
        std::fs::set_permissions(&binary, std::fs::Permissions::from_mode(0o755)).unwrap();
        let expected = hex(Sha256::digest(&bytes));

        let upload_to = |remote: &str, overwrite: bool| {
            validate_upload(json!({ "host": alias, "localPath": "app", "remotePath": remote, "overwrite": overwrite })).unwrap()
        };
        let local = local_path(&workspace, "app");
        // Into a directory: keeps the name and the mode, verified by hash.
        let report = upload(
            &ssh,
            &mut fs,
            &upload_to(&format!("{dir}/"), false),
            open_upload(&local).await.unwrap(),
        )
        .await
        .unwrap();
        assert_eq!(report.remote_path, format!("{dir}/app"));
        assert_eq!(report.size, bytes.len() as u64);
        assert_eq!(report.sha256, expected);
        assert_eq!(report.remote_sha256.as_deref(), Some(expected.as_str()));
        assert!(!report.replaced);
        let listing = fs.list(&dir).await.unwrap();
        let names: Vec<_> = listing
            .entries
            .iter()
            .map(|entry| entry.name.as_str())
            .collect();
        assert_eq!(names, ["app"], "no temporary file is left behind");
        assert_eq!(listing.entries[0].permissions.as_deref(), Some("rwxr-xr-x"));

        // An existing file needs overwrite; the expected hash guards it.
        let target = format!("{dir}/app");
        assert!(matches!(
            upload(
                &ssh,
                &mut fs,
                &upload_to(&target, false),
                open_upload(&local).await.unwrap()
            )
            .await,
            Err(AppError::Conflict(_))
        ));
        let mut wrong = upload_to(&target, true);
        wrong.expected_sha256 = Some("0".repeat(64));
        assert!(
            upload(&ssh, &mut fs, &wrong, open_upload(&local).await.unwrap())
                .await
                .is_err()
        );
        // The refused upload left the file and its directory as they were.
        assert_eq!(
            fs.stat(&target).await.unwrap().map(|stat| stat.size),
            Some(bytes.len() as u64)
        );
        assert_eq!(fs.list(&dir).await.unwrap().entries.len(), 1);
        let report = upload(
            &ssh,
            &mut fs,
            &upload_to(&target, true),
            open_upload(&local).await.unwrap(),
        )
        .await
        .unwrap();
        assert!(report.replaced);
        assert_eq!(fs.list(&dir).await.unwrap().entries.len(), 1);

        // And back into the workspace, refusing to clobber or leave it.
        let back = |local: &str, overwrite: bool| {
            validate_download(json!({ "host": alias, "remotePath": target, "localPath": local, "overwrite": overwrite })).unwrap()
        };
        std::fs::create_dir_all(workspace.join("out")).unwrap();
        let request = back("out/", false);
        let report = download(
            &ssh,
            &mut fs,
            &request,
            &workspace,
            &local_path(&workspace, "out/"),
        )
        .await
        .unwrap();
        assert!(report.local_path.ends_with("out/app"));
        assert_eq!(report.remote_sha256.as_deref(), Some(expected.as_str()));
        assert_eq!(std::fs::read(&report.local_path).unwrap(), bytes);
        let request = back("out/app", false);
        assert!(matches!(
            download(
                &ssh,
                &mut fs,
                &request,
                &workspace,
                &local_path(&workspace, "out/app")
            )
            .await,
            Err(AppError::Conflict(_))
        ));
        let request = back("../escape", false);
        assert!(download(
            &ssh,
            &mut fs,
            &request,
            &workspace,
            &local_path(&workspace, "../escape")
        )
        .await
        .is_err());
        let leftovers: Vec<_> = std::fs::read_dir(workspace.join("out"))
            .unwrap()
            .map(|entry| entry.unwrap().file_name())
            .collect();
        assert_eq!(leftovers, ["app"]);

        fs.remove(&target).await.unwrap();
        fs.remove(&dir).await.unwrap();
        RemoteFs::close(&mut fs).await;
        let _ = std::fs::remove_dir_all(root);
    }
}
