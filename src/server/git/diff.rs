//! Bounded unified diffs: one changed file (workbench change cards) or the
//! whole working tree. Reads only; the same repository resolution and
//! metadata checks as the status endpoint apply before Git may evaluate file
//! contents.
use super::*;
use crate::server::protocol::{GitDiffBaseRef, GitDiffResponse};

const DIFF_TIMEOUT: Duration = Duration::from_secs(10);
/// One tracked diff plus a `diff --no-index` per untracked file.
const REPOSITORY_DIFF_TIMEOUT: Duration = Duration::from_secs(30);
const DIFF_PATH_LIMIT: usize = 4096;

pub(crate) async fn file(
    roots: &[PathBuf],
    workspace: &Path,
    path: &str,
) -> Result<GitDiffResponse> {
    let _permit = timeout(GIT_SCAN_QUEUE_TIMEOUT, GIT_SCAN_SEMAPHORE.acquire())
        .await
        .map_err(|_| AppError::Conflict("Git read capacity is busy".to_owned()))?
        .map_err(|_| AppError::Conflict("Git read capacity is closed".to_owned()))?;
    timeout(DIFF_TIMEOUT, file_inner(roots, workspace, path))
        .await
        .map_err(|_| AppError::GitCommandTimedOut("Git file diff".to_owned()))?
}

async fn file_inner(roots: &[PathBuf], workspace: &Path, path: &str) -> Result<GitDiffResponse> {
    let roots = canonical_workspace_roots(roots);
    let workspace = validate_workspace_directory(&roots, workspace)?;
    let relative = checked_relative_path(path)?;
    let repository = resolve_repository(&roots, &workspace)
        .await?
        .ok_or_else(|| AppError::InvalidRequest("工作区不是 Git 仓库".to_owned()))?;
    repository_metadata_roots(&roots, &repository).await?;
    validate_read_execution_config(&repository).await?;

    // Match the status summary: an unborn HEAD diffs against the empty tree so
    // staged files still appear before the first commit.
    let head = run_git_command(
        &repository,
        &git_args(&["rev-parse", "--verify", "--quiet", "HEAD^{tree}"]),
        "Git diff base tree",
    )
    .await?;
    let base = if head.status.success() {
        head.stdout
    } else if head.status.code() == Some(1) {
        run_checked(
            &repository,
            &git_args(&["hash-object", "-t", "tree", "--stdin"]),
            "Git diff empty tree",
        )
        .await?
        .stdout
    } else {
        return Err(AppError::GitProcess(
            "Git returned an invalid HEAD tree".to_owned(),
        ));
    };
    let base = std::str::from_utf8(&base)
        .map_err(|_| AppError::GitProcess("Git returned an invalid tree id".to_owned()))?
        .trim();
    if !matches!(base.len(), 40 | 64) || !base.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return Err(AppError::GitProcess(
            "Git returned an invalid tree id".to_owned(),
        ));
    }

    let diff = run_checked(
        &repository,
        &git_args(&[
            "diff",
            "--no-ext-diff",
            "--no-textconv",
            "--find-renames",
            base,
            "--",
            path,
        ]),
        "Git file diff",
    )
    .await?;

    // Untracked files have no index/HEAD entry, so diff -- <path> is empty.
    // A clean tracked file is also empty, so confirm the file is absent from
    // the index before comparing the working file against /dev/null.
    let (stdout, untracked) = if diff.stdout.is_empty() {
        let tracked = run_git_command(
            &repository,
            &git_args(&["ls-files", "--error-unmatch", "--", path]),
            "Git tracked file check",
        )
        .await?;
        if tracked.status.success() {
            (diff.stdout, false)
        } else {
            match untracked_file_diff(&repository, &relative).await? {
                Some(output) => (output, true),
                None => (diff.stdout, false),
            }
        }
    } else {
        (diff.stdout, false)
    };
    let truncated = stdout.len() > GIT_RESPONSE_OUTPUT_LIMIT;
    Ok(GitDiffResponse {
        repository_path: repository.display().to_string(),
        path: path.to_owned(),
        diff: bounded_text(&stdout, GIT_RESPONSE_OUTPUT_LIMIT),
        truncated,
        untracked,
        base: None,
        base_ref: None,
    })
}

/// The whole working tree (tracked changes, staged or not, plus untracked
/// files) against the upstream merge base, `HEAD`, or the empty tree.
pub(crate) async fn repository(roots: &[PathBuf], workspace: &Path) -> Result<GitDiffResponse> {
    let _permit = timeout(GIT_SCAN_QUEUE_TIMEOUT, GIT_SCAN_SEMAPHORE.acquire())
        .await
        .map_err(|_| AppError::Conflict("Git read capacity is busy".to_owned()))?
        .map_err(|_| AppError::Conflict("Git read capacity is closed".to_owned()))?;
    timeout(REPOSITORY_DIFF_TIMEOUT, repository_inner(roots, workspace))
        .await
        .map_err(|_| AppError::GitCommandTimedOut("Git repository diff".to_owned()))?
}

async fn repository_inner(roots: &[PathBuf], workspace: &Path) -> Result<GitDiffResponse> {
    let roots = canonical_workspace_roots(roots);
    let workspace = validate_workspace_directory(&roots, workspace)?;
    let repository = resolve_repository(&roots, &workspace)
        .await?
        .ok_or_else(|| AppError::InvalidRequest("工作区不是 Git 仓库".to_owned()))?;
    repository_metadata_roots(&roots, &repository).await?;
    validate_read_execution_config(&repository).await?;
    let (base, base_ref) = repository_base(&repository).await?;

    let limit = GIT_RESPONSE_OUTPUT_LIMIT;
    let tracked = run_git_command_truncated(
        &repository,
        &git_args(&[
            "diff",
            "--no-ext-diff",
            "--no-textconv",
            "--no-color",
            "--find-renames",
            &base,
            "--",
        ]),
        "Git repository diff",
        limit,
    )
    .await?;
    if !tracked.status.success() {
        return Err(command_failure(
            "Git repository diff",
            &GitCommandOutput {
                status: tracked.status,
                stdout: Vec::new(),
                stderr: tracked.stderr,
            },
        ));
    }
    let mut output = tracked.stdout;
    let mut truncated = tracked.truncated;

    if !truncated {
        let untracked = run_checked(
            &repository,
            &git_args(&["ls-files", "-z", "--others", "--exclude-standard"]),
            "Git untracked files",
        )
        .await?;
        for (index, raw) in untracked
            .stdout
            .split(|byte| *byte == 0)
            .filter(|raw| !raw.is_empty())
            .enumerate()
        {
            if index >= GIT_STATUS_FILE_LIMIT || output.len() > limit {
                truncated = true;
                break;
            }
            let Ok(path) = std::str::from_utf8(raw) else {
                continue;
            };
            let Ok(relative) = checked_relative_path(path) else {
                continue;
            };
            if let Some(diff) = new_file_diff(&repository, &relative).await? {
                output.extend_from_slice(&diff);
            }
        }
    }
    truncated |= output.len() > limit;
    Ok(GitDiffResponse {
        repository_path: repository.display().to_string(),
        path: String::new(),
        diff: bounded_text(&output, limit),
        truncated,
        untracked: false,
        base: Some(base),
        base_ref: Some(base_ref),
    })
}

/// The merge base with the upstream when the branch has one (so unpushed
/// commits are part of the diff), else `HEAD`, else the empty tree.
async fn repository_base(repository: &Path) -> Result<(String, GitDiffBaseRef)> {
    let head = run_git_command(
        repository,
        &git_args(&["rev-parse", "--verify", "--quiet", "HEAD^{commit}"]),
        "Git diff base",
    )
    .await?;
    if !head.status.success() {
        if head.status.code() != Some(1) {
            return Err(AppError::GitProcess(
                "Git returned an invalid HEAD".to_owned(),
            ));
        }
        let empty = run_checked(
            repository,
            &git_args(&["hash-object", "-t", "tree", "--stdin"]),
            "Git diff empty tree",
        )
        .await?;
        return Ok((object_id(&empty.stdout)?, GitDiffBaseRef::Empty));
    }
    // Fails without an upstream (or on a detached HEAD): fall back to HEAD.
    let merge_base = run_git_command(
        repository,
        &git_args(&["merge-base", "HEAD", "@{upstream}"]),
        "Git diff upstream base",
    )
    .await?;
    if merge_base.status.success() {
        if let Ok(base) = object_id(&merge_base.stdout) {
            return Ok((base, GitDiffBaseRef::Upstream));
        }
    }
    Ok((object_id(&head.stdout)?, GitDiffBaseRef::Head))
}

fn object_id(output: &[u8]) -> Result<String> {
    let id = std::str::from_utf8(output)
        .map_err(|_| AppError::GitProcess("Git returned an invalid object id".to_owned()))?
        .trim();
    if !matches!(id.len(), 40 | 64) || !id.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return Err(AppError::GitProcess(
            "Git returned an invalid object id".to_owned(),
        ));
    }
    Ok(id.to_owned())
}

/// A new-file diff for one untracked file, with repository-relative names.
/// Files above the untracked size limit are shown like binary files instead
/// of being read.
async fn new_file_diff(repository: &Path, relative: &Path) -> Result<Option<Vec<u8>>> {
    let path = repository.join(relative);
    let metadata = match tokio::fs::symlink_metadata(&path).await {
        Ok(metadata) => metadata,
        Err(error)
            if matches!(
                error.kind(),
                io::ErrorKind::PermissionDenied | io::ErrorKind::NotFound
            ) =>
        {
            return Ok(None);
        }
        Err(error) => return Err(AppError::Io(error)),
    };
    if !metadata.is_file() {
        return Ok(None);
    }
    if !std::fs::canonicalize(&path)?.starts_with(repository) {
        return Ok(None);
    }
    let name = relative.to_string_lossy();
    if metadata.len() > GIT_UNTRACKED_FILE_LIMIT {
        return Ok(Some(
            format!(
                "diff --git a/{name} b/{name}\nnew file mode 100644\nBinary files /dev/null and b/{name} differ\n"
            )
            .into_bytes(),
        ));
    }
    let mut args = git_args(&[
        "diff",
        "--no-index",
        "--no-ext-diff",
        "--no-textconv",
        "--no-color",
        "--",
        "/dev/null",
    ]);
    args.push(name.into_owned());
    let output = run_git_command(repository, &args, "Git untracked file diff").await?;
    // --no-index exits 1 when the two sides differ; only real failures abort.
    if output.status.success() || output.status.code() == Some(1) {
        return Ok(Some(output.stdout));
    }
    Err(AppError::GitProcess(
        "git diff --no-index failed".to_owned(),
    ))
}

/// Reject paths that could escape the repository when the working file must be
/// read directly for an untracked diff.
fn checked_relative_path(path: &str) -> Result<PathBuf> {
    let relative = PathBuf::from(path);
    if path.is_empty()
        || path.len() > DIFF_PATH_LIMIT
        || path.contains('\0')
        || relative.is_absolute()
        || relative
            .components()
            .any(|part| matches!(part, std::path::Component::ParentDir))
    {
        return Err(AppError::InvalidRequest("无效的文件路径".to_owned()));
    }
    Ok(relative)
}

async fn untracked_file_diff(repository: &Path, relative: &Path) -> Result<Option<Vec<u8>>> {
    let path = repository.join(relative);
    let metadata = match tokio::fs::symlink_metadata(&path).await {
        Ok(metadata) => metadata,
        Err(error)
            if matches!(
                error.kind(),
                io::ErrorKind::PermissionDenied | io::ErrorKind::NotFound
            ) =>
        {
            return Ok(None);
        }
        Err(error) => return Err(AppError::Io(error)),
    };
    // Directories and unreadable entries simply have no tracked diff either.
    if !metadata.is_file() {
        return Ok(None);
    }
    let canonical = std::fs::canonicalize(&path)?;
    if !canonical.starts_with(repository) {
        return Err(AppError::WorkspacePathOutsideRoot);
    }
    let mut args = git_args(&[
        "diff",
        "--no-index",
        "--no-ext-diff",
        "--no-textconv",
        "--",
        "/dev/null",
    ]);
    args.push(canonical.display().to_string());
    let output = run_git_command(repository, &args, "Git untracked file diff").await?;
    // --no-index exits 1 when the two sides differ; only real failures abort.
    if output.status.success() || output.status.code() == Some(1) {
        return Ok(Some(output.stdout));
    }
    Err(AppError::GitProcess(
        "git diff --no-index failed".to_owned(),
    ))
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::fs;

    struct Fixture {
        root: PathBuf,
        repo: PathBuf,
    }
    impl Fixture {
        fn new() -> Self {
            let root =
                std::env::temp_dir().join(format!("todex-git-diff-{}", uuid::Uuid::new_v4()));
            fs::create_dir_all(root.join("repo")).unwrap();
            let root = fs::canonicalize(root).unwrap();
            let fixture = Self {
                repo: root.join("repo"),
                root,
            };
            fixture.git(&["init", "--template="]);
            fixture.git(&["config", "user.name", "Test"]);
            fixture.git(&["config", "user.email", "test@example.invalid"]);
            fixture
        }
        fn git(&self, args: &[&str]) {
            let status = std::process::Command::new("git")
                .args(args)
                .current_dir(&self.repo)
                .stdin(std::process::Stdio::null())
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .status()
                .unwrap();
            assert!(status.success(), "git {args:?} failed");
        }
        fn commit(&self) {
            self.git(&["add", "--all"]);
            self.git(&[
                "-c",
                "commit.gpgSign=false",
                "-c",
                "core.hooksPath=/dev/null",
                "commit",
                "--allow-empty",
                "-m",
                "Initial",
            ]);
        }
        async fn diff(&self, path: &str) -> Result<GitDiffResponse> {
            file(std::slice::from_ref(&self.root), &self.repo, path).await
        }
        async fn whole(&self) -> GitDiffResponse {
            repository(std::slice::from_ref(&self.root), &self.repo)
                .await
                .unwrap()
        }
    }
    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.root);
        }
    }

    #[tokio::test]
    async fn tracked_modification_returns_unified_diff() {
        let fixture = Fixture::new();
        fs::write(fixture.repo.join("a.txt"), "old line\n").unwrap();
        fixture.commit();
        fs::write(fixture.repo.join("a.txt"), "old line\nnew line\n").unwrap();
        let result = fixture.diff("a.txt").await.unwrap();
        assert!(result.diff.contains("+new line"), "{}", result.diff);
        assert!(!result.truncated);
        assert!(!result.untracked);
        let deleted = fixture.repo.join("a.txt");
        fs::remove_file(&deleted).unwrap();
        let result = fixture.diff("a.txt").await.unwrap();
        assert!(result.diff.contains("-old line"), "{}", result.diff);
    }

    #[tokio::test]
    async fn untracked_file_diffs_against_empty() {
        let fixture = Fixture::new();
        fs::write(fixture.repo.join("a.txt"), "same\n").unwrap();
        fixture.commit();
        // A tracked file with no changes produces an empty, non-untracked diff.
        let result = fixture.diff("a.txt").await.unwrap();
        assert!(!result.untracked);
        assert!(result.diff.is_empty(), "{}", result.diff);
        fs::write(fixture.repo.join("new.txt"), "fresh\ncontent\n").unwrap();
        let result = fixture.diff("new.txt").await.unwrap();
        assert!(result.untracked);
        assert!(result.diff.contains("+fresh"), "{}", result.diff);
    }

    #[tokio::test]
    async fn whole_repository_diff_covers_staged_unstaged_and_untracked_files() {
        let fixture = Fixture::new();
        fs::write(fixture.repo.join("a.txt"), "old\n").unwrap();
        fs::write(fixture.repo.join("b.txt"), "keep\n").unwrap();
        fixture.commit();
        fs::write(fixture.repo.join("a.txt"), "old\nunstaged\n").unwrap();
        fs::write(fixture.repo.join("b.txt"), "keep\nstaged\n").unwrap();
        fixture.git(&["add", "b.txt"]);
        fs::create_dir(fixture.repo.join("dir")).unwrap();
        fs::write(fixture.repo.join("dir/new file.txt"), "fresh\n").unwrap();
        fs::write(fixture.repo.join(".gitignore"), "ignored.txt\n").unwrap();
        fs::write(fixture.repo.join("ignored.txt"), "secret\n").unwrap();
        let head = std::process::Command::new("git")
            .args(["rev-parse", "HEAD"])
            .current_dir(&fixture.repo)
            .output()
            .unwrap();
        let result = fixture.whole().await;
        assert_eq!(result.base_ref, Some(GitDiffBaseRef::Head));
        assert_eq!(
            result.base.as_deref(),
            Some(String::from_utf8(head.stdout).unwrap().trim())
        );
        assert_eq!(result.path, "");
        assert!(!result.untracked && !result.truncated);
        for expected in [
            "+unstaged",
            "+staged",
            "+fresh",
            "b/dir/new file.txt",
            "+ignored.txt",
        ] {
            assert!(
                result.diff.contains(expected),
                "{expected}: {}",
                result.diff
            );
        }
        assert!(!result.diff.contains("secret"), "{}", result.diff);
        assert!(
            !result.diff.contains(&fixture.repo.display().to_string()),
            "untracked names are repository-relative: {}",
            result.diff
        );
        let json = serde_json::to_value(&result).unwrap();
        assert_eq!(json["baseRef"], "head");
        assert!(json["base"].is_string());
    }

    #[tokio::test]
    async fn whole_repository_diff_includes_unpushed_commits_and_unborn_repositories() {
        let fixture = Fixture::new();
        fs::write(fixture.repo.join("staged.txt"), "first\n").unwrap();
        fixture.git(&["add", "staged.txt"]);
        fs::write(fixture.repo.join("loose.txt"), "loose\n").unwrap();
        let unborn = fixture.whole().await;
        assert_eq!(unborn.base_ref, Some(GitDiffBaseRef::Empty));
        assert!(unborn.diff.contains("+first") && unborn.diff.contains("+loose"));

        fixture.commit();
        let remote = fixture.root.join("origin.git");
        fixture.git(&["init", "--bare", "--template=", remote.to_str().unwrap()]);
        fixture.git(&["remote", "add", "origin", remote.to_str().unwrap()]);
        fixture.git(&[
            "-c",
            "core.hooksPath=/dev/null",
            "push",
            "-u",
            "origin",
            "HEAD",
        ]);
        let pushed = std::process::Command::new("git")
            .args(["rev-parse", "HEAD"])
            .current_dir(&fixture.repo)
            .output()
            .unwrap();
        fs::write(fixture.repo.join("staged.txt"), "first\nunpushed\n").unwrap();
        fixture.commit();
        let result = fixture.whole().await;
        assert_eq!(result.base_ref, Some(GitDiffBaseRef::Upstream));
        assert_eq!(
            result.base.as_deref(),
            Some(String::from_utf8(pushed.stdout).unwrap().trim())
        );
        assert!(result.diff.contains("+unpushed"), "{}", result.diff);
    }

    #[tokio::test]
    async fn whole_repository_diff_truncates_large_output() {
        let fixture = Fixture::new();
        fixture.commit();
        let line = "x".repeat(1023) + "\n";
        for index in 0..3 {
            fs::write(
                fixture.repo.join(format!("big-{index}.txt")),
                line.repeat(512),
            )
            .unwrap();
        }
        let result = fixture.whole().await;
        assert!(result.truncated);
        assert!(result.diff.len() <= GIT_RESPONSE_OUTPUT_LIMIT);
    }

    #[tokio::test]
    async fn rejects_escaping_and_non_repository_paths() {
        let fixture = Fixture::new();
        fixture.commit();
        for path in ["../outside.txt", "/abs.txt", "", "dir/../../x"] {
            assert!(fixture.diff(path).await.is_err(), "path {path:?} accepted");
        }
        let result = file(&[fixture.root.clone()], &fixture.root, "a.txt").await;
        assert!(matches!(result, Err(AppError::InvalidRequest(_))));
    }
}
