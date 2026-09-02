use std::{
    collections::{HashSet, VecDeque},
    io,
    path::{Path, PathBuf},
    process::{ExitStatus, Stdio},
    time::Duration,
};

use tokio::{
    io::{AsyncRead, AsyncReadExt},
    process::{Child, Command},
    time::timeout,
};

use crate::{
    error::{AppError, Result},
    server::protocol::{
        GitAction, GitFileChange, GitRepositorySummary, GitRunRequest, GitRunResponse,
        GitScanResponse,
    },
    workspace_paths::{canonical_workspace_root, validate_workspace_directory},
};

/// Git is an external process, so both execution time and captured output are
/// bounded even when a repository contains a very large status or hook log.
pub(crate) const GIT_COMMAND_TIMEOUT: Duration = Duration::from_secs(15);
pub(crate) const GIT_COMMAND_OUTPUT_LIMIT: usize = 4 * 1024 * 1024;

const GIT_SCAN_DEPTH: usize = 2;
const GIT_SCAN_CANDIDATE_LIMIT: usize = 512;
const GIT_STATUS_FILE_LIMIT: usize = 2_000;
const GIT_UNTRACKED_FILE_LIMIT: u64 = 4 * 1024 * 1024;
const GIT_UNTRACKED_TOTAL_LIMIT: u64 = 32 * 1024 * 1024;
const GIT_ERROR_DETAIL_LIMIT: usize = 512;
const GIT_COMMIT_MESSAGE_LIMIT: usize = 512;
const GIT_RESPONSE_OUTPUT_LIMIT: usize = 1024 * 1024;
const GIT_PATH_OUTPUT_LIMIT: usize = 64 * 1024;
const GIT_METADATA_ENTRY_LIMIT: usize = 100_000;
const GIT_SCAN_TIMEOUT: Duration = Duration::from_secs(30);
const GIT_SCAN_QUEUE_TIMEOUT: Duration = Duration::from_secs(2);
const GIT_WRITE_QUEUE_TIMEOUT: Duration = Duration::from_secs(2);
const GIT_MUTATION_TIMEOUT: Duration = Duration::from_secs(120);

static GIT_WRITE_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());
static GIT_SCAN_SEMAPHORE: tokio::sync::Semaphore = tokio::sync::Semaphore::const_new(2);

#[derive(Debug)]
struct GitCommandOutput {
    status: ExitStatus,
    stdout: Vec<u8>,
    stderr: Vec<u8>,
}

struct ProcessGroupGuard {
    process_group_id: Option<u32>,
}

impl ProcessGroupGuard {
    fn new(process_group_id: Option<u32>) -> Self {
        Self { process_group_id }
    }

    fn disarm(&mut self) {
        self.process_group_id = None;
    }
}

impl Drop for ProcessGroupGuard {
    fn drop(&mut self) {
        #[cfg(unix)]
        if let Some(pid) = self.process_group_id {
            // This guard also runs when an outer request deadline cancels the
            // future before the normal async cleanup path can finish.
            unsafe {
                libc::kill(-(pid as i32), libc::SIGKILL);
            }
        }
    }
}

#[derive(Debug)]
enum LimitedReadError {
    Limit,
    Io(io::Error),
}

impl From<io::Error> for LimitedReadError {
    fn from(error: io::Error) -> Self {
        Self::Io(error)
    }
}

/// Execute only the `git` binary with an explicit argument vector. No shell is
/// involved, and stdin is closed so credential/editor prompts cannot block the
/// HTTP worker indefinitely.
async fn run_git_command(cwd: &Path, args: &[String], operation: &str) -> Result<GitCommandOutput> {
    let mut command = secure_git_command();
    command
        .arg("-C")
        .arg(cwd)
        .args(args)
        .env("GIT_TERMINAL_PROMPT", "0")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        command.as_std_mut().process_group(0);
    }

    let mut child = command.spawn().map_err(map_spawn_error)?;
    let process_group_id = child.id();
    let mut process_group_guard = ProcessGroupGuard::new(process_group_id);
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| AppError::GitProcess("git stdout pipe was not available".to_owned()))?;
    let stderr = child
        .stderr
        .take()
        .ok_or_else(|| AppError::GitProcess("git stderr pipe was not available".to_owned()))?;

    let command_result = timeout(GIT_COMMAND_TIMEOUT, async {
        let wait = async { child.wait().await.map_err(LimitedReadError::Io) };
        tokio::try_join!(
            read_limited(stdout, GIT_COMMAND_OUTPUT_LIMIT),
            read_limited(stderr, GIT_COMMAND_OUTPUT_LIMIT),
            wait,
        )
    })
    .await;

    let (stdout, stderr, status) = match command_result {
        Err(_elapsed) => {
            terminate_child(&mut child, process_group_id).await;
            process_group_guard.disarm();
            return Err(AppError::GitCommandTimedOut(operation.to_owned()));
        }
        Ok(Err(LimitedReadError::Limit)) => {
            terminate_child(&mut child, process_group_id).await;
            process_group_guard.disarm();
            return Err(AppError::GitOutputLimitExceeded(GIT_COMMAND_OUTPUT_LIMIT));
        }
        Ok(Err(LimitedReadError::Io(error))) => {
            terminate_child(&mut child, process_group_id).await;
            process_group_guard.disarm();
            return Err(AppError::GitProcess(error.to_string()));
        }
        Ok(Ok((stdout, stderr, status))) => (stdout, stderr, status),
    };
    process_group_guard.disarm();

    Ok(GitCommandOutput {
        status,
        stdout,
        stderr,
    })
}

fn secure_git_command() -> Command {
    let mut command = Command::new("git");
    command.env_clear().args([
        "-c",
        "core.fsmonitor=false",
        "-c",
        "core.untrackedCache=false",
    ]);
    // Keep the runtime environment Git needs for user configuration, locale,
    // network certificates, and SSH agent access. In particular, do not pass
    // daemon configuration or Git's path override variables to hooks/helpers.
    for key in [
        "PATH",
        "HOME",
        "USER",
        "LOGNAME",
        "SHELL",
        "TERM",
        "TMPDIR",
        "TMP",
        "TEMP",
        "LANG",
        "LC_ALL",
        "XDG_CONFIG_HOME",
        "XDG_DATA_HOME",
        "XDG_CACHE_HOME",
        "XDG_RUNTIME_DIR",
        "SSH_AUTH_SOCK",
        "HTTP_PROXY",
        "HTTPS_PROXY",
        "NO_PROXY",
        "http_proxy",
        "https_proxy",
        "no_proxy",
        "SSL_CERT_FILE",
        "SSL_CERT_DIR",
        "REQUESTS_CA_BUNDLE",
        "NODE_EXTRA_CA_CERTS",
        "USERPROFILE",
        "APPDATA",
        "LOCALAPPDATA",
        "SYSTEMROOT",
        "COMSPEC",
        "PATHEXT",
    ] {
        if let Some(value) = std::env::var_os(key) {
            command.env(key, value);
        }
    }
    command
}

async fn read_limited<R>(
    mut reader: R,
    limit: usize,
) -> std::result::Result<Vec<u8>, LimitedReadError>
where
    R: AsyncRead + Unpin,
{
    let mut output = Vec::with_capacity(limit.min(8192));
    let mut buffer = [0_u8; 8192];
    loop {
        let read = reader.read(&mut buffer).await?;
        if read == 0 {
            break;
        }
        if output.len().saturating_add(read) > limit {
            return Err(LimitedReadError::Limit);
        }
        output.extend_from_slice(&buffer[..read]);
    }
    Ok(output)
}

async fn terminate_child(child: &mut Child, process_group_id: Option<u32>) {
    // `Child::kill` waits for the process as well as sending SIGKILL, which
    // avoids leaving a zombie after a timeout or output-limit violation.
    #[cfg(unix)]
    if let Some(pid) = process_group_id {
        // Git hooks and credential helpers can inherit the pipes. The child
        // is isolated in its own process group so they are terminated too.
        unsafe {
            libc::kill(-(pid as i32), libc::SIGKILL);
        }
    }
    let _ = child.kill().await;
}

fn map_spawn_error(error: io::Error) -> AppError {
    if error.kind() == io::ErrorKind::NotFound {
        AppError::GitUnavailable
    } else {
        AppError::GitProcess(error.to_string())
    }
}

async fn run_checked(cwd: &Path, args: &[String], operation: &str) -> Result<GitCommandOutput> {
    let output = run_git_command(cwd, args, operation).await?;
    if output.status.success() {
        Ok(output)
    } else {
        Err(command_failure(operation, &output))
    }
}

fn command_failure(operation: &str, output: &GitCommandOutput) -> AppError {
    let detail = bounded_text(&output.stderr, GIT_ERROR_DETAIL_LIMIT);
    let detail = if detail.trim().is_empty() {
        match output.status.code() {
            Some(code) => format!("exit status {code}"),
            None => "process terminated by signal".to_owned(),
        }
    } else {
        detail.trim().to_owned()
    };
    AppError::GitCommandFailed {
        operation: operation.to_owned(),
        detail,
    }
}

pub(crate) async fn scan(
    workspace_root: &Path,
    requested_workspace: &Path,
) -> Result<GitScanResponse> {
    let _permit = timeout(GIT_SCAN_QUEUE_TIMEOUT, GIT_SCAN_SEMAPHORE.acquire())
        .await
        .map_err(|_| AppError::Conflict("Git scan capacity is busy".to_owned()))?
        .map_err(|_| AppError::GitProcess("Git scan semaphore is closed".to_owned()))?;
    timeout(
        GIT_SCAN_TIMEOUT,
        scan_inner(workspace_root, requested_workspace),
    )
    .await
    .map_err(|_| AppError::GitCommandTimedOut("git scan".to_owned()))?
}

async fn scan_inner(workspace_root: &Path, requested_workspace: &Path) -> Result<GitScanResponse> {
    let configured_root = canonical_workspace_root(workspace_root)?;
    let workspace = validate_workspace_directory(&configured_root, requested_workspace)?;
    let candidates = collect_candidates(&workspace).await?;
    let mut repository_roots = HashSet::new();

    for candidate in candidates {
        let args = git_args(&["rev-parse", "--show-toplevel"]);
        let output = run_git_command(&candidate, &args, "git rev-parse --show-toplevel").await?;
        if !output.status.success() {
            continue;
        }
        if let Some(repository) =
            parse_repository_root(&configured_root, &candidate, &output.stdout)?
        {
            repository_roots.insert(repository);
        }
    }

    let mut repository_roots: Vec<_> = repository_roots.into_iter().collect();
    repository_roots.sort();
    let mut repositories = Vec::with_capacity(repository_roots.len() + 1);
    for repository in repository_roots {
        match summarize_repository(&configured_root, &repository).await {
            Ok(summary) => repositories.push(summary),
            Err(error) if should_propagate_scan_error(&error) => return Err(error),
            Err(error) => repositories.push(summary_error(&repository, error)),
        }
    }

    if !repositories
        .iter()
        .any(|summary| summary.path == workspace.display().to_string())
    {
        repositories.insert(0, uninitialized_summary(&workspace));
    }

    Ok(GitScanResponse { repositories })
}

pub(crate) async fn run(
    workspace_root: &Path,
    data_dir: &Path,
    requested_workspace: &Path,
    request: &GitRunRequest,
) -> Result<GitRunResponse> {
    #[cfg(not(unix))]
    {
        let _ = (workspace_root, data_dir, requested_workspace, request);
        Err(AppError::Unsupported(
            "Git write operations require Unix process-group cleanup".to_owned(),
        ))
    }
    #[cfg(unix)]
    {
        run_supported(workspace_root, data_dir, requested_workspace, request).await
    }
}

#[cfg(unix)]
async fn run_supported(
    workspace_root: &Path,
    data_dir: &Path,
    requested_workspace: &Path,
    request: &GitRunRequest,
) -> Result<GitRunResponse> {
    let configured_root = canonical_workspace_root(workspace_root)?;
    let workspace = validate_workspace_directory(&configured_root, requested_workspace)?;
    validate_run_request(request)?;
    let _write_guard = timeout(GIT_WRITE_QUEUE_TIMEOUT, GIT_WRITE_LOCK.lock())
        .await
        .map_err(|_| AppError::Conflict("Git write capacity is busy".to_owned()))?;

    match timeout(
        GIT_MUTATION_TIMEOUT,
        run_locked(&configured_root, data_dir, &workspace, request),
    )
    .await
    {
        Ok(result) => result,
        Err(_) => Err(AppError::GitPartialSuccess {
            repository_path: workspace.display().to_string(),
            operation: "Git action reached its total deadline".to_owned(),
            detail: "repository or remote state may have changed; refresh before retrying"
                .to_owned(),
        }),
    }
}

async fn run_locked(
    configured_root: &Path,
    data_dir: &Path,
    workspace: &Path,
    request: &GitRunRequest,
) -> Result<GitRunResponse> {
    let repository = resolve_repository(&configured_root, &workspace).await?;
    let repository = match (&request.action, repository) {
        (GitAction::Initial, Some(repository)) if repository == workspace => repository,
        (GitAction::Initial, Some(_)) => {
            return Err(AppError::InvalidRequest(
                "initial requires the exact repository root or an uninitialized directory"
                    .to_owned(),
            ));
        }
        (GitAction::Initial, None) => workspace.to_path_buf(),
        (_, Some(repository)) if repository == workspace => repository,
        (_, Some(_)) => {
            return Err(AppError::InvalidRequest(
                "workspacePath must identify the exact repository root".to_owned(),
            ));
        }
        (_, None) => return Err(AppError::GitRepositoryNotFound),
    };
    if repository.join(".git").exists() {
        validate_mutation_repository_metadata(&configured_root, &repository).await?;
        validate_mutation_execution_config(&repository).await?;
    }
    if request.action == GitAction::Initial {
        let head = run_git_command(
            &repository,
            &git_args(&["rev-parse", "--verify", "HEAD"]),
            "git rev-parse --verify HEAD",
        )
        .await?;
        if head.status.success() {
            return Err(AppError::InvalidRequest(
                "initial is only valid before the first commit".to_owned(),
            ));
        }
    }
    let hooks_path = prepare_disabled_hooks_directory(data_dir).await?;

    let mut output = String::new();
    match request.action {
        GitAction::Initial => {
            run_step(&repository, git_args(&["init"]), "git init", &mut output).await?;
            validate_mutation_repository_metadata(&configured_root, &repository).await?;
            validate_mutation_execution_config(&repository).await?;
            if request.include_unstaged {
                run_step(
                    &repository,
                    mutation_git_args(&hooks_path, &["add", "-A"]),
                    "git add -A",
                    &mut output,
                )
                .await?;
            }
            let message = commit_message(request.message.as_deref(), "Initial commit")?;
            run_step(
                &repository,
                mutation_git_args_owned(
                    &hooks_path,
                    vec!["commit".to_owned(), "-m".to_owned(), message],
                ),
                "git commit",
                &mut output,
            )
            .await?;
        }
        GitAction::Commit | GitAction::CommitPush => {
            if request.include_unstaged {
                run_step(
                    &repository,
                    mutation_git_args(&hooks_path, &["add", "-A"]),
                    "git add -A",
                    &mut output,
                )
                .await?;
            }
            let summary = summarize_repository(&configured_root, &repository).await?;
            let fallback = format_commit_message(&summary);
            let message = commit_message(request.message.as_deref(), &fallback)?;
            run_step(
                &repository,
                mutation_git_args_owned(
                    &hooks_path,
                    vec!["commit".to_owned(), "-m".to_owned(), message],
                ),
                "git commit",
                &mut output,
            )
            .await?;
            if request.action.is_push() {
                if let Err(error) = run_step(
                    &repository,
                    mutation_git_args(&hooks_path, &["push"]),
                    "git push",
                    &mut output,
                )
                .await
                {
                    return Err(AppError::GitPartialSuccess {
                        repository_path: repository.display().to_string(),
                        operation: "commit succeeded but push failed".to_owned(),
                        detail: truncate_text(&error.to_string(), GIT_ERROR_DETAIL_LIMIT),
                    });
                }
            }
        }
        GitAction::Push => {
            run_step(
                &repository,
                mutation_git_args(&hooks_path, &["push"]),
                "git push",
                &mut output,
            )
            .await?;
        }
    }

    if output.is_empty() {
        output.push_str("Operation completed");
    }
    Ok(GitRunResponse {
        repository_path: repository.display().to_string(),
        action: request.action.clone(),
        output,
    })
}

fn validate_run_request(request: &GitRunRequest) -> Result<()> {
    if matches!(request.action, GitAction::Push) && request.message.is_some() {
        return Err(AppError::InvalidRequest(
            "message is only valid for commit actions".to_owned(),
        ));
    }
    if let Some(message) = request.message.as_deref() {
        if message.len() > GIT_COMMIT_MESSAGE_LIMIT {
            return Err(AppError::InvalidRequest(format!(
                "commit message exceeds {GIT_COMMIT_MESSAGE_LIMIT} bytes"
            )));
        }
        if message.as_bytes().contains(&0) {
            return Err(AppError::InvalidRequest(
                "commit message cannot contain NUL".to_owned(),
            ));
        }
        if message
            .chars()
            .any(|character| character.is_control() && !matches!(character, '\n' | '\r' | '\t'))
        {
            return Err(AppError::InvalidRequest(
                "commit message contains an unsupported control character".to_owned(),
            ));
        }
    }
    Ok(())
}

fn commit_message(message: Option<&str>, fallback: &str) -> Result<String> {
    let selected = message
        .map(str::trim)
        .filter(|message| !message.is_empty())
        .unwrap_or(fallback)
        .to_owned();
    if selected.len() > GIT_COMMIT_MESSAGE_LIMIT {
        return Err(AppError::InvalidRequest(format!(
            "commit message exceeds {GIT_COMMIT_MESSAGE_LIMIT} bytes"
        )));
    }
    Ok(selected)
}

async fn prepare_disabled_hooks_directory(data_dir: &Path) -> Result<PathBuf> {
    tokio::fs::create_dir_all(data_dir).await?;
    let hooks_path = data_dir.join("git-hooks-disabled");
    match tokio::fs::symlink_metadata(&hooks_path).await {
        Ok(metadata) if metadata.file_type().is_symlink() || !metadata.is_dir() => {
            return Err(AppError::GitProcess(
                "disabled hooks path is not a real directory".to_owned(),
            ));
        }
        Ok(_) => {}
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            tokio::fs::create_dir(&hooks_path).await?;
        }
        Err(error) => return Err(AppError::Io(error)),
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        tokio::fs::set_permissions(&hooks_path, std::fs::Permissions::from_mode(0o700)).await?;
    }
    let canonical_data_dir = std::fs::canonicalize(data_dir)?;
    let canonical_hooks_path = std::fs::canonicalize(&hooks_path)?;
    if !canonical_hooks_path.starts_with(&canonical_data_dir) {
        return Err(AppError::GitProcess(
            "disabled hooks path escapes the data directory".to_owned(),
        ));
    }
    Ok(canonical_hooks_path)
}

async fn validate_mutation_execution_config(repository: &Path) -> Result<()> {
    for (scope, operation) in [
        ("--local", "git config --local --includes --list"),
        ("--worktree", "git config --worktree --includes --list"),
    ] {
        let output = run_checked(
            repository,
            &git_args(&["config", scope, "--includes", "--null", "--list"]),
            operation,
        )
        .await?;
        validate_executable_config_entries(&output.stdout)?;
    }
    Ok(())
}

fn validate_executable_config_entries(output: &[u8]) -> Result<()> {
    for entry in output.split(|byte| *byte == 0) {
        if entry.is_empty() {
            continue;
        }
        let key_end = entry
            .iter()
            .position(|byte| matches!(*byte, b'\n' | b'='))
            .unwrap_or(entry.len());
        let key = String::from_utf8_lossy(&entry[..key_end]).to_ascii_lowercase();
        let value = entry
            .get(key_end.saturating_add(1)..)
            .map(String::from_utf8_lossy)
            .unwrap_or_default();
        let executable_filter =
            key.starts_with("filter.") && (key.ends_with(".clean") || key.ends_with(".process"));
        let remote_url =
            key.starts_with("remote.") && (key.ends_with(".url") || key.ends_with(".pushurl"));
        let remote_url_rewrite = key.starts_with("url.")
            && (key.ends_with(".insteadof") || key.ends_with(".pushinsteadof"));
        let executable_helper = key == "core.sshcommand"
            || key == "core.gitproxy"
            || key == "core.askpass"
            || key == "credential.helper"
            || (key.starts_with("credential.") && key.ends_with(".helper"))
            || key == "gpg.program"
            || (key.starts_with("gpg.") && key.ends_with(".program"))
            || (key.starts_with("remote.") && key.ends_with(".vcs"));
        if executable_filter
            || executable_helper
            || remote_url_rewrite
            || (remote_url && !is_supported_remote_url(&value))
        {
            return Err(AppError::InvalidRequest(format!(
                "repository-local executable Git config is not supported: {key}"
            )));
        }
    }
    Ok(())
}

fn is_supported_remote_url(value: &str) -> bool {
    let value = value.trim();
    let lower = value.to_ascii_lowercase();
    if ["http://", "https://", "ssh://", "git://"]
        .iter()
        .any(|prefix| lower.starts_with(prefix))
    {
        return true;
    }
    if value.is_empty()
        || value.starts_with(['/', '.', '\\'])
        || value.contains("://")
        || value.contains("::")
        || value.contains(['\n', '\r', '\0'])
    {
        return false;
    }
    let Some((host, path)) = value.split_once(':') else {
        return false;
    };
    !host.is_empty() && !path.is_empty() && !host.contains(['/', '\\'])
}

fn mutation_git_args(hooks_path: &Path, values: &[&str]) -> Vec<String> {
    mutation_git_args_owned(hooks_path, git_args(values))
}

fn mutation_git_args_owned(hooks_path: &Path, values: Vec<String>) -> Vec<String> {
    let mut args = vec![
        "-c".to_owned(),
        format!("core.hooksPath={}", hooks_path.display()),
        "-c".to_owned(),
        "commit.gpgSign=false".to_owned(),
        "-c".to_owned(),
        "push.gpgSign=false".to_owned(),
        "-c".to_owned(),
        "protocol.ext.allow=never".to_owned(),
        "-c".to_owned(),
        "protocol.file.allow=never".to_owned(),
    ];
    args.extend(values);
    args
}

fn format_commit_message(summary: &GitRepositorySummary) -> String {
    let count = summary.files.len();
    format!(
        "Update {count} file{} in {}",
        if count == 1 { "" } else { "s" },
        summary.name
    )
}

async fn run_step(
    cwd: &Path,
    args: Vec<String>,
    operation: &str,
    output: &mut String,
) -> Result<()> {
    let command_output = run_checked(cwd, &args, operation).await?;
    append_output(output, &command_output.stdout);
    append_output(output, &command_output.stderr);
    Ok(())
}

fn append_output(destination: &mut String, bytes: &[u8]) {
    let text = bounded_text(bytes, GIT_RESPONSE_OUTPUT_LIMIT);
    let text = text.trim();
    if text.is_empty() || destination.len() >= GIT_RESPONSE_OUTPUT_LIMIT {
        return;
    }
    if !destination.is_empty() {
        destination.push('\n');
    }
    let remaining = GIT_RESPONSE_OUTPUT_LIMIT.saturating_sub(destination.len());
    destination.push_str(&truncate_text(text, remaining));
}

async fn resolve_repository(configured_root: &Path, workspace: &Path) -> Result<Option<PathBuf>> {
    let args = git_args(&["rev-parse", "--show-toplevel"]);
    let output = run_git_command(workspace, &args, "git rev-parse --show-toplevel").await?;
    if !output.status.success() {
        return Ok(None);
    }
    parse_repository_root(configured_root, workspace, &output.stdout)
}

async fn validate_repository_metadata(configured_root: &Path, repository: &Path) -> Result<()> {
    repository_metadata_roots(configured_root, repository)
        .await
        .map(|_| ())
}

async fn repository_metadata_roots(
    configured_root: &Path,
    repository: &Path,
) -> Result<Vec<PathBuf>> {
    let mut roots = Vec::new();
    for (arguments, operation) in [
        (
            ["rev-parse", "--absolute-git-dir"],
            "git rev-parse --absolute-git-dir",
        ),
        (
            ["rev-parse", "--git-common-dir"],
            "git rev-parse --git-common-dir",
        ),
    ] {
        let output = run_checked(repository, &git_args(&arguments), operation).await?;
        let path = parse_git_path_output(&output.stdout)?;
        let path = if path.is_absolute() {
            path
        } else {
            repository.join(path)
        };
        let canonical = std::fs::canonicalize(&path)?;
        if !canonical.is_dir() || !canonical.starts_with(configured_root) {
            return Err(AppError::WorkspacePathOutsideRoot);
        }
        if path != canonical {
            return Err(AppError::InvalidRequest(
                "Git metadata paths cannot contain symbolic links".to_owned(),
            ));
        }
        if !roots.contains(&canonical) {
            roots.push(canonical);
        }
    }
    Ok(roots)
}

async fn validate_mutation_repository_metadata(
    configured_root: &Path,
    repository: &Path,
) -> Result<()> {
    let roots = repository_metadata_roots(configured_root, repository).await?;
    let mut entry_count = 0_usize;
    for root in roots {
        validate_metadata_root(&root, &mut entry_count).await?;
    }
    Ok(())
}

async fn validate_metadata_root(root: &Path, entry_count: &mut usize) -> Result<()> {
    validate_real_directory(root).await?;
    let mut entries = tokio::fs::read_dir(root).await?;
    while let Some(entry) = entries.next_entry().await? {
        count_metadata_entry(entry_count)?;
        if entry.file_type().await?.is_symlink() {
            return Err(unsupported_metadata_layout(&entry.path()));
        }
    }

    for directory in ["refs", "logs", "reftable"] {
        validate_metadata_subtree(&root.join(directory), entry_count).await?;
    }
    validate_object_metadata(&root.join("objects"), entry_count).await
}

async fn validate_object_metadata(objects: &Path, entry_count: &mut usize) -> Result<()> {
    if !validate_optional_real_directory(objects).await? {
        return Ok(());
    }
    let info = objects.join("info");
    for alternate in [info.join("alternates"), info.join("http-alternates")] {
        match tokio::fs::symlink_metadata(&alternate).await {
            Ok(_) => {
                return Err(AppError::InvalidRequest(format!(
                    "Git object alternates are not supported for write operations: {}",
                    alternate.display()
                )));
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => return Err(AppError::Io(error)),
        }
    }

    let mut entries = tokio::fs::read_dir(objects).await?;
    while let Some(entry) = entries.next_entry().await? {
        count_metadata_entry(entry_count)?;
        let file_type = entry.file_type().await?;
        if file_type.is_symlink() {
            return Err(unsupported_metadata_layout(&entry.path()));
        }
        if file_type.is_dir() {
            let name = entry.file_name();
            if name == "info" || name == "pack" {
                validate_metadata_subtree(&entry.path(), entry_count).await?;
            }
        }
    }
    Ok(())
}

async fn validate_metadata_subtree(path: &Path, entry_count: &mut usize) -> Result<()> {
    if !validate_optional_real_directory(path).await? {
        return Ok(());
    }
    let mut queue = VecDeque::from([path.to_path_buf()]);
    while let Some(directory) = queue.pop_front() {
        let mut entries = tokio::fs::read_dir(&directory).await?;
        while let Some(entry) = entries.next_entry().await? {
            count_metadata_entry(entry_count)?;
            let file_type = entry.file_type().await?;
            if file_type.is_symlink() {
                return Err(unsupported_metadata_layout(&entry.path()));
            }
            if file_type.is_dir() {
                queue.push_back(entry.path());
            }
        }
    }
    Ok(())
}

async fn validate_optional_real_directory(path: &Path) -> Result<bool> {
    match tokio::fs::symlink_metadata(path).await {
        Ok(metadata) if metadata.file_type().is_symlink() || !metadata.is_dir() => {
            Err(unsupported_metadata_layout(path))
        }
        Ok(_) => Ok(true),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(AppError::Io(error)),
    }
}

async fn validate_real_directory(path: &Path) -> Result<()> {
    if validate_optional_real_directory(path).await? {
        Ok(())
    } else {
        Err(AppError::GitRepositoryNotFound)
    }
}

fn count_metadata_entry(entry_count: &mut usize) -> Result<()> {
    *entry_count = entry_count.saturating_add(1);
    if *entry_count > GIT_METADATA_ENTRY_LIMIT {
        return Err(AppError::InvalidRequest(format!(
            "Git metadata exceeds the {GIT_METADATA_ENTRY_LIMIT} entry validation limit"
        )));
    }
    Ok(())
}

fn unsupported_metadata_layout(path: &Path) -> AppError {
    AppError::InvalidRequest(format!(
        "symbolic links are not supported in Git write metadata: {}",
        path.display()
    ))
}

fn parse_repository_root(
    configured_root: &Path,
    cwd: &Path,
    bytes: &[u8],
) -> Result<Option<PathBuf>> {
    if bytes.iter().all(|byte| matches!(*byte, b'\n' | b'\r')) {
        return Ok(None);
    }
    let path = parse_git_path_output(bytes)?;
    let path = if path.is_absolute() {
        path
    } else {
        cwd.join(path)
    };
    let canonical = std::fs::canonicalize(&path).map_err(|error| {
        if error.kind() == io::ErrorKind::NotFound {
            AppError::GitRepositoryNotFound
        } else {
            AppError::Io(error)
        }
    })?;
    if !canonical.starts_with(configured_root) {
        return Err(AppError::WorkspacePathOutsideRoot);
    }
    if !canonical.is_dir() {
        return Err(AppError::GitRepositoryNotFound);
    }
    Ok(Some(canonical))
}

fn parse_git_path_output(bytes: &[u8]) -> Result<PathBuf> {
    if bytes.len() > GIT_PATH_OUTPUT_LIMIT {
        return Err(AppError::GitProcess(format!(
            "Git path output exceeds {GIT_PATH_OUTPUT_LIMIT} bytes"
        )));
    }
    let raw = std::str::from_utf8(bytes)
        .map_err(|_| AppError::GitProcess("Git returned a non-UTF-8 path".to_owned()))?
        .trim_end_matches(['\n', '\r']);
    if raw.is_empty() || raw.as_bytes().contains(&0) || raw.contains(['\n', '\r']) {
        return Err(AppError::GitProcess(
            "Git returned an invalid path".to_owned(),
        ));
    }
    Ok(PathBuf::from(raw))
}

async fn collect_candidates(workspace: &Path) -> Result<Vec<PathBuf>> {
    let mut candidates = Vec::new();
    let mut seen = HashSet::new();
    let mut queue = VecDeque::from([(workspace.to_path_buf(), 0_usize)]);
    while let Some((directory, depth)) = queue.pop_front() {
        if !seen.insert(directory.clone()) {
            continue;
        }
        candidates.push(directory.clone());
        if candidates.len() > GIT_SCAN_CANDIDATE_LIMIT {
            return Err(AppError::GitScanLimitExceeded);
        }
        if depth >= GIT_SCAN_DEPTH {
            continue;
        }

        let mut entries = match tokio::fs::read_dir(&directory).await {
            Ok(entries) => entries,
            Err(error)
                if matches!(
                    error.kind(),
                    io::ErrorKind::PermissionDenied | io::ErrorKind::NotFound
                ) =>
            {
                continue;
            }
            Err(error) => return Err(AppError::Io(error)),
        };
        while let Some(entry) = entries.next_entry().await? {
            let name = entry.file_name().to_string_lossy().to_string();
            if should_skip_directory(&name) {
                continue;
            }
            let file_type = match entry.file_type().await {
                Ok(file_type) => file_type,
                Err(error)
                    if matches!(
                        error.kind(),
                        io::ErrorKind::PermissionDenied | io::ErrorKind::NotFound
                    ) =>
                {
                    continue;
                }
                Err(error) => return Err(AppError::Io(error)),
            };
            if !file_type.is_dir() {
                continue;
            }
            let child = match std::fs::canonicalize(entry.path()) {
                Ok(child) if child.starts_with(workspace) => child,
                Ok(_) => continue,
                Err(error)
                    if matches!(
                        error.kind(),
                        io::ErrorKind::PermissionDenied | io::ErrorKind::NotFound
                    ) =>
                {
                    continue;
                }
                Err(error) => return Err(AppError::Io(error)),
            };
            if candidates
                .len()
                .saturating_add(queue.len())
                .saturating_add(1)
                > GIT_SCAN_CANDIDATE_LIMIT
            {
                return Err(AppError::GitScanLimitExceeded);
            }
            queue.push_back((child, depth + 1));
        }
    }
    candidates.sort();
    Ok(candidates)
}

fn should_skip_directory(name: &str) -> bool {
    matches!(name, ".git" | "node_modules" | "target")
}

async fn summarize_repository(
    configured_root: &Path,
    repository: &Path,
) -> Result<GitRepositorySummary> {
    let repository = validate_workspace_directory(configured_root, repository)?;
    validate_repository_metadata(configured_root, &repository).await?;
    let branch_output = run_git_command(
        &repository,
        &git_args(&["branch", "--show-current"]),
        "git branch --show-current",
    )
    .await?;
    let branch = if branch_output.status.success() {
        let branch = bounded_text(&branch_output.stdout, GIT_ERROR_DETAIL_LIMIT)
            .trim()
            .to_owned();
        if branch.is_empty() {
            "HEAD".to_owned()
        } else {
            branch
        }
    } else {
        "HEAD".to_owned()
    };

    let status = run_checked(
        &repository,
        &git_args(&["status", "--short", "--untracked-files=all"]),
        "git status --short",
    )
    .await?;
    let parsed_status = parse_status(&status.stdout);
    let untracked_additions =
        count_untracked_lines(&repository, &parsed_status.untracked_paths).await?;

    let diff = run_git_command(
        &repository,
        &git_args(&[
            "diff",
            "--no-ext-diff",
            "--no-textconv",
            "--numstat",
            "HEAD",
        ]),
        "git diff --numstat HEAD",
    )
    .await?;
    let (mut additions, deletions) = if diff.status.success() {
        parse_numstat(&diff.stdout)
    } else {
        (0, 0)
    };
    additions = additions.saturating_add(untracked_additions);

    let head = run_git_command(
        &repository,
        &git_args(&["rev-parse", "--verify", "HEAD"]),
        "git rev-parse --verify HEAD",
    )
    .await?;

    let path = repository.display().to_string();
    let name = repository
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or(&path)
        .to_owned();
    Ok(GitRepositorySummary {
        path,
        name,
        branch,
        files: parsed_status.files,
        additions,
        deletions,
        initial_eligible: !head.status.success(),
        error: None,
        files_truncated: parsed_status.files_truncated,
    })
}

#[derive(Debug)]
struct ParsedStatus {
    files: Vec<GitFileChange>,
    untracked_paths: Vec<PathBuf>,
    files_truncated: bool,
}

fn parse_status(bytes: &[u8]) -> ParsedStatus {
    let text = String::from_utf8_lossy(bytes);
    let mut files = Vec::new();
    let mut untracked_paths = Vec::new();
    let mut files_truncated = false;
    for line in text.lines() {
        if line.len() < 3 {
            continue;
        }
        let Some(path) = line.get(3..) else {
            continue;
        };
        let path = unquote_git_path(path.trim());
        if path.is_empty() {
            continue;
        }
        let status = line[..2].to_owned();
        if files.len() < GIT_STATUS_FILE_LIMIT {
            files.push(GitFileChange {
                path: path.clone(),
                status: status.clone(),
            });
        } else {
            files_truncated = true;
        }
        if status == "??" && untracked_paths.len() < GIT_STATUS_FILE_LIMIT {
            untracked_paths.push(PathBuf::from(path));
        }
    }
    ParsedStatus {
        files,
        untracked_paths,
        files_truncated,
    }
}

fn unquote_git_path(path: &str) -> String {
    path.strip_prefix('"')
        .and_then(|value| value.strip_suffix('"'))
        .unwrap_or(path)
        .to_owned()
}

fn parse_numstat(bytes: &[u8]) -> (u64, u64) {
    let text = String::from_utf8_lossy(bytes);
    let mut additions = 0_u64;
    let mut deletions = 0_u64;
    for line in text.lines() {
        let mut columns = line.split('\t');
        if let Some(value) = columns.next().and_then(|value| value.parse::<u64>().ok()) {
            additions = additions.saturating_add(value);
        }
        if let Some(value) = columns.next().and_then(|value| value.parse::<u64>().ok()) {
            deletions = deletions.saturating_add(value);
        }
    }
    (additions, deletions)
}

async fn count_untracked_lines(repository: &Path, paths: &[PathBuf]) -> Result<u64> {
    let mut total_bytes = 0_u64;
    let mut additions = 0_u64;
    for relative in paths {
        if relative.is_absolute()
            || relative
                .components()
                .any(|component| matches!(component, std::path::Component::ParentDir))
        {
            continue;
        }
        let path = repository.join(relative);
        let metadata = match tokio::fs::symlink_metadata(&path).await {
            Ok(metadata) => metadata,
            Err(error)
                if matches!(
                    error.kind(),
                    io::ErrorKind::PermissionDenied | io::ErrorKind::NotFound
                ) =>
            {
                continue;
            }
            Err(error) => return Err(AppError::Io(error)),
        };
        if !metadata.file_type().is_file() || metadata.len() > GIT_UNTRACKED_FILE_LIMIT {
            continue;
        }
        if total_bytes.saturating_add(metadata.len()) > GIT_UNTRACKED_TOTAL_LIMIT {
            break;
        }
        let canonical = match std::fs::canonicalize(&path) {
            Ok(canonical) if canonical.starts_with(repository) => canonical,
            _ => continue,
        };
        let bytes = match tokio::fs::read(canonical).await {
            Ok(bytes) => bytes,
            Err(error)
                if matches!(
                    error.kind(),
                    io::ErrorKind::PermissionDenied | io::ErrorKind::NotFound
                ) =>
            {
                continue;
            }
            Err(error) => return Err(AppError::Io(error)),
        };
        total_bytes = total_bytes.saturating_add(bytes.len() as u64);
        if !bytes.is_empty() {
            additions = additions.saturating_add(
                bytes.iter().filter(|byte| **byte == b'\n').count() as u64
                    + u64::from(!bytes.ends_with(b"\n")),
            );
        }
    }
    Ok(additions)
}

fn should_propagate_scan_error(error: &AppError) -> bool {
    matches!(
        error,
        AppError::GitUnavailable
            | AppError::GitCommandTimedOut(_)
            | AppError::GitOutputLimitExceeded(_)
            | AppError::GitProcess(_)
            | AppError::WorkspacePathOutsideRoot
    )
}

fn summary_error(repository: &Path, error: AppError) -> GitRepositorySummary {
    let path = repository.display().to_string();
    let name = repository
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or(&path)
        .to_owned();
    GitRepositorySummary {
        path,
        name,
        branch: "UNKNOWN".to_owned(),
        files: Vec::new(),
        additions: 0,
        deletions: 0,
        initial_eligible: false,
        error: Some(bounded_text(
            error.to_string().as_bytes(),
            GIT_ERROR_DETAIL_LIMIT,
        )),
        files_truncated: false,
    }
}

fn uninitialized_summary(workspace: &Path) -> GitRepositorySummary {
    let path = workspace.display().to_string();
    let name = workspace
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or(&path)
        .to_owned();
    GitRepositorySummary {
        path,
        name,
        branch: "UNINITIALIZED".to_owned(),
        files: Vec::new(),
        additions: 0,
        deletions: 0,
        initial_eligible: true,
        error: None,
        files_truncated: false,
    }
}

fn git_args(values: &[&str]) -> Vec<String> {
    values.iter().map(|value| (*value).to_owned()).collect()
}

fn bounded_text(bytes: &[u8], limit: usize) -> String {
    truncate_text(&String::from_utf8_lossy(bytes), limit)
}

fn truncate_text(text: &str, limit: usize) -> String {
    if text.len() <= limit {
        return text.to_owned();
    }
    let mut end = limit;
    while end > 0 && !text.is_char_boundary(end) {
        end -= 1;
    }
    text[..end].to_owned()
}

#[cfg(test)]
mod tests {
    use std::{
        env, fs,
        path::PathBuf,
        time::{SystemTime, UNIX_EPOCH},
    };

    use super::*;

    fn temp_dir(prefix: &str) -> PathBuf {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock")
            .as_nanos();
        let path = env::temp_dir().join(format!("{prefix}-{nonce}-{}", std::process::id()));
        fs::create_dir_all(&path).expect("create temp directory");
        path
    }

    #[test]
    fn parses_porcelain_status_and_marks_overflow() {
        let parsed = parse_status(b" M src/main.rs\n?? notes.txt\n");
        assert_eq!(parsed.files.len(), 2);
        assert_eq!(parsed.files[0].status, " M");
        assert_eq!(parsed.untracked_paths, vec![PathBuf::from("notes.txt")]);
        assert!(!parsed.files_truncated);
    }

    #[test]
    fn parses_git_paths_without_truncating_long_valid_output() {
        let raw = format!("/workspace/{}\n", "nested/".repeat(100));
        let parsed = parse_git_path_output(raw.as_bytes()).expect("long path");
        assert_eq!(parsed, PathBuf::from(raw.trim_end()));
        assert!(parse_git_path_output(b"/workspace/one\n/two\n").is_err());
    }

    #[test]
    fn accepts_standard_remotes_and_rejects_custom_helpers() {
        for url in [
            "https://example.com/org/repo.git",
            "ssh://git@example.com/org/repo.git",
            "git://example.com/org/repo.git",
            "git@example.com:org/repo.git",
        ] {
            assert!(
                is_supported_remote_url(url),
                "expected supported URL: {url}"
            );
        }
        for url in [
            "custom::payload",
            "custom://example.com/repo",
            "../outside.git",
            "/outside.git",
        ] {
            assert!(
                !is_supported_remote_url(url),
                "expected rejected URL: {url}"
            );
        }
        assert!(matches!(
            validate_executable_config_entries(b"remote.origin.url\ncustom::payload\0"),
            Err(AppError::InvalidRequest(_))
        ));
        assert!(matches!(
            validate_executable_config_entries(b"url.custom::.insteadof\nhttps://example.com/\0"),
            Err(AppError::InvalidRequest(_))
        ));
    }

    #[test]
    fn rejects_relative_or_parent_untracked_paths() {
        let root = temp_dir("todex-git-path");
        let parsed = parse_status(b"?? ../outside.txt\n");
        let runtime = tokio::runtime::Runtime::new().expect("runtime");
        let additions = runtime
            .block_on(count_untracked_lines(&root, &parsed.untracked_paths))
            .expect("count succeeds");
        assert_eq!(additions, 0);
        let _ = fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn scans_repository_and_reports_uninitialized_workspace() {
        let root = temp_dir("todex-git-scan");
        let workspace = root.join("workspace");
        fs::create_dir_all(&workspace).expect("workspace");
        fs::write(workspace.join("README.md"), "hello\n").expect("file");
        let response = scan(&root, &workspace).await.expect("scan");
        assert_eq!(response.repositories.len(), 1);
        assert!(response.repositories[0].initial_eligible);
        assert_eq!(response.repositories[0].branch, "UNINITIALIZED");
        let _ = fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn rejects_repository_local_executable_filters() {
        let root = temp_dir("todex-git-filter");
        let repository = root.join("repository");
        fs::create_dir_all(&repository).expect("repository");
        let initialized = std::process::Command::new("git")
            .args(["-C", repository.to_str().unwrap(), "init"])
            .status()
            .expect("git init");
        assert!(initialized.success());
        let configured = std::process::Command::new("git")
            .args([
                "-C",
                repository.to_str().unwrap(),
                "config",
                "filter.untrusted.clean",
                "echo unsafe",
            ])
            .status()
            .expect("git config");
        assert!(configured.success());
        assert!(matches!(
            validate_mutation_execution_config(&repository).await,
            Err(AppError::InvalidRequest(_))
        ));
        let _ = fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn rejects_included_filters_and_repository_askpass() {
        let root = temp_dir("todex-git-included-filter");
        let repository = root.join("repository");
        fs::create_dir_all(&repository).expect("repository");
        let initialized = std::process::Command::new("git")
            .args(["-C", repository.to_str().unwrap(), "init"])
            .status()
            .expect("git init");
        assert!(initialized.success());

        let included_config = root.join("included.config");
        fs::write(
            &included_config,
            "[filter \"untrusted\"]\n\tprocess = echo unsafe\n",
        )
        .expect("included config");
        let configured_include = std::process::Command::new("git")
            .args([
                "-C",
                repository.to_str().unwrap(),
                "config",
                "include.path",
                included_config.to_str().unwrap(),
            ])
            .status()
            .expect("git config include");
        assert!(configured_include.success());
        assert!(matches!(
            validate_mutation_execution_config(&repository).await,
            Err(AppError::InvalidRequest(_))
        ));

        let removed_include = std::process::Command::new("git")
            .args([
                "-C",
                repository.to_str().unwrap(),
                "config",
                "--unset-all",
                "include.path",
            ])
            .status()
            .expect("remove include");
        assert!(removed_include.success());
        let configured_askpass = std::process::Command::new("git")
            .args([
                "-C",
                repository.to_str().unwrap(),
                "config",
                "core.askPass",
                "echo unsafe",
            ])
            .status()
            .expect("git config askpass");
        assert!(configured_askpass.success());
        assert!(matches!(
            validate_mutation_execution_config(&repository).await,
            Err(AppError::InvalidRequest(_))
        ));
        let _ = fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn rejects_worktree_scoped_executable_config() {
        let root = temp_dir("todex-git-worktree-config");
        let repository = root.join("repository");
        fs::create_dir_all(&repository).expect("repository");
        let initialized = std::process::Command::new("git")
            .args(["-C", repository.to_str().unwrap(), "init"])
            .status()
            .expect("git init");
        assert!(initialized.success());
        let enabled = std::process::Command::new("git")
            .args([
                "-C",
                repository.to_str().unwrap(),
                "config",
                "extensions.worktreeConfig",
                "true",
            ])
            .status()
            .expect("enable worktree config");
        assert!(enabled.success());
        let configured = std::process::Command::new("git")
            .args([
                "-C",
                repository.to_str().unwrap(),
                "config",
                "--worktree",
                "filter.untrusted.process",
                "echo unsafe",
            ])
            .status()
            .expect("git config --worktree");
        assert!(configured.success());
        assert!(matches!(
            validate_mutation_execution_config(&repository).await,
            Err(AppError::InvalidRequest(_))
        ));
        let _ = fs::remove_dir_all(root);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn rejects_symlinked_object_directory_for_mutation() {
        use std::os::unix::fs::symlink;

        let root = temp_dir("todex-git-metadata-symlink");
        let workspace_root = root.join("workspace-root");
        let repository = workspace_root.join("repository");
        let outside_objects = root.join("outside-objects");
        fs::create_dir_all(&repository).expect("repository");
        fs::create_dir_all(&outside_objects).expect("outside objects");
        let initialized = std::process::Command::new("git")
            .args(["-C", repository.to_str().unwrap(), "init"])
            .status()
            .expect("git init");
        assert!(initialized.success());
        fs::remove_dir_all(repository.join(".git/objects")).expect("remove objects");
        symlink(&outside_objects, repository.join(".git/objects")).expect("symlink objects");

        let configured_root = fs::canonicalize(&workspace_root).expect("workspace root");
        assert!(matches!(
            validate_mutation_repository_metadata(&configured_root, &repository).await,
            Err(AppError::InvalidRequest(_))
        ));
        let _ = fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn rejects_object_alternates_for_mutation() {
        let root = temp_dir("todex-git-alternates");
        let repository = root.join("repository");
        fs::create_dir_all(&repository).expect("repository");
        let initialized = std::process::Command::new("git")
            .args(["-C", repository.to_str().unwrap(), "init"])
            .status()
            .expect("git init");
        assert!(initialized.success());
        let info = repository.join(".git/objects/info");
        fs::write(info.join("alternates"), root.display().to_string()).expect("alternates");

        let configured_root = fs::canonicalize(&root).expect("workspace root");
        assert!(matches!(
            validate_mutation_repository_metadata(&configured_root, &repository).await,
            Err(AppError::InvalidRequest(_))
        ));
        let _ = fs::remove_dir_all(root);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn accepts_linked_worktree_metadata_inside_workspace_root() {
        let root = temp_dir("todex-git-linked-worktree");
        let repository = root.join("repository");
        let worktree = root.join("linked-worktree");
        fs::create_dir_all(&repository).expect("repository");
        let initialized = std::process::Command::new("git")
            .args(["-C", repository.to_str().unwrap(), "init"])
            .status()
            .expect("git init");
        assert!(initialized.success());
        let committed = std::process::Command::new("git")
            .args([
                "-C",
                repository.to_str().unwrap(),
                "-c",
                "user.name=TodeX Test",
                "-c",
                "user.email=todex@example.invalid",
                "commit",
                "--allow-empty",
                "-m",
                "Initial",
            ])
            .status()
            .expect("git commit");
        assert!(committed.success());
        let added = std::process::Command::new("git")
            .args([
                "-C",
                repository.to_str().unwrap(),
                "worktree",
                "add",
                "--detach",
                worktree.to_str().unwrap(),
            ])
            .status()
            .expect("git worktree add");
        assert!(added.success());

        let configured_root = fs::canonicalize(&root).expect("workspace root");
        validate_mutation_repository_metadata(&configured_root, &worktree)
            .await
            .expect("linked worktree metadata");
        let _ = fs::remove_dir_all(root);
    }
}
