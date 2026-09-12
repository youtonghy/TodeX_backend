//! A bounded unified diff for one changed file, used by workbench change
//! cards. Reads only; the same repository resolution and metadata checks as
//! the status endpoint apply before Git may evaluate file contents.
use super::*;
use crate::server::protocol::GitDiffResponse;

const DIFF_TIMEOUT: Duration = Duration::from_secs(10);
const DIFF_PATH_LIMIT: usize = 4096;

pub(crate) async fn file(root: &Path, workspace: &Path, path: &str) -> Result<GitDiffResponse> {
    let _permit = timeout(GIT_SCAN_QUEUE_TIMEOUT, GIT_SCAN_SEMAPHORE.acquire())
        .await
        .map_err(|_| AppError::Conflict("Git read capacity is busy".to_owned()))?
        .map_err(|_| AppError::Conflict("Git read capacity is closed".to_owned()))?;
    timeout(DIFF_TIMEOUT, file_inner(root, workspace, path))
        .await
        .map_err(|_| AppError::GitCommandTimedOut("Git file diff".to_owned()))?
}

async fn file_inner(root: &Path, workspace: &Path, path: &str) -> Result<GitDiffResponse> {
    let root = canonical_workspace_root(root)?;
    let workspace = validate_workspace_directory(&root, workspace)?;
    let relative = checked_relative_path(path)?;
    let repository = resolve_repository(&root, &workspace)
        .await?
        .ok_or_else(|| AppError::InvalidRequest("工作区不是 Git 仓库".to_owned()))?;
    repository_metadata_roots(&root, &repository).await?;
    validate_mutation_execution_config(&repository).await?;

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
    })
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
            file(&self.root, &self.repo, path).await
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
    async fn rejects_escaping_and_non_repository_paths() {
        let fixture = Fixture::new();
        fixture.commit();
        for path in ["../outside.txt", "/abs.txt", "", "dir/../../x"] {
            assert!(fixture.diff(path).await.is_err(), "path {path:?} accepted");
        }
        let result = file(&fixture.root, &fixture.root, "a.txt").await;
        assert!(matches!(result, Err(AppError::InvalidRequest(_))));
    }
}
