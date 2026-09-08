//! A bounded summary of the current repository only. Unlike the Git menu's
//! workspace snapshot, a subdirectory resolves to its containing repository.
use super::*;
use crate::server::protocol::{GitStatusResponse, GitWorktreeKind};

const STATUS_TIMEOUT: Duration = Duration::from_secs(10);

pub(crate) async fn read(root: &Path, workspace: &Path) -> Result<GitStatusResponse> {
    let _permit = timeout(GIT_SCAN_QUEUE_TIMEOUT, GIT_SCAN_SEMAPHORE.acquire())
        .await
        .map_err(|_| AppError::Conflict("Git read capacity is busy".to_owned()))?
        .map_err(|_| AppError::Conflict("Git read capacity is closed".to_owned()))?;
    timeout(STATUS_TIMEOUT, read_inner(root, workspace))
        .await
        .map_err(|_| AppError::GitCommandTimedOut("Git status summary".to_owned()))?
}

async fn command(repository: &Path, args: &[&str]) -> Result<GitCommandOutput> {
    let mut arguments = git_args(&["--no-optional-locks"]);
    arguments.extend(git_args(args));
    run_checked(repository, &arguments, "Git status summary").await
}

async fn read_inner(root: &Path, workspace: &Path) -> Result<GitStatusResponse> {
    let root = canonical_workspace_root(root)?;
    let workspace = validate_workspace_directory(&root, workspace)?;
    let mut result = GitStatusResponse {
        repository_path: workspace.display().to_string(),
        initialized: false,
        branch: None,
        worktree_kind: None,
        changed_files: 0,
        additions: 0,
        deletions: 0,
        stats_truncated: false,
    };
    let Some(repository) = resolve_repository(&root, &workspace).await? else {
        return Ok(result);
    };
    // Reject external/symlinked metadata and executable local filters before
    // status or diff can cause Git to evaluate file contents.
    let metadata = repository_metadata_roots(&root, &repository).await?;
    validate_mutation_execution_config(&repository).await?;
    result.repository_path = repository.display().to_string();
    result.initialized = true;
    result.worktree_kind = Some(if metadata.len() > 1 {
        GitWorktreeKind::Linked
    } else {
        GitWorktreeKind::Main
    });
    let branch = command(&repository, &["branch", "--show-current"]).await?;
    let branch = std::str::from_utf8(&branch.stdout)
        .map_err(|_| malformed("branch name"))?
        .trim_end_matches(['\r', '\n']);
    if !branch.is_empty() {
        result.branch = Some(branch.to_owned());
    }
    let status = command(
        &repository,
        &[
            "status",
            "--porcelain=v1",
            "-z",
            "--untracked-files=all",
            "--renames",
        ],
    )
    .await?;
    let status = parse_status_z(&status.stdout)?;
    result.changed_files = status.changed_files;
    result.stats_truncated = status.truncated;
    let head = run_git_command(
        &repository,
        &git_args(&["rev-parse", "--verify", "--quiet", "HEAD^{tree}"]),
        "Git status base tree",
    )
    .await?;
    let base = if head.status.success() {
        head.stdout
    } else if head.status.code() == Some(1) {
        // Computing the empty-tree ID is read-only and respects the repository's
        // SHA format. Comparing the working tree to it handles staged files
        // edited again before the first commit without counting changes twice.
        command(&repository, &["hash-object", "-t", "tree", "--stdin"])
            .await?
            .stdout
    } else {
        return Err(malformed("HEAD tree"));
    };
    let base = std::str::from_utf8(&base)
        .map_err(|_| malformed("tree id"))?
        .trim();
    if !matches!(base.len(), 40 | 64) || !base.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return Err(malformed("tree id"));
    }
    let diff = command(
        &repository,
        &[
            "diff",
            "--no-ext-diff",
            "--no-textconv",
            "--numstat",
            "-z",
            "--find-renames",
            base,
            "--",
        ],
    )
    .await?;
    let (additions, deletions) = parse_numstat_z(&diff.stdout)?;
    let (untracked, truncated) = untracked_additions(&repository, &status.untracked_paths).await?;
    result.additions = additions.saturating_add(untracked);
    result.deletions = deletions;
    result.stats_truncated |= truncated;
    Ok(result)
}

fn malformed(field: &str) -> AppError {
    AppError::GitProcess(format!("Git returned an invalid {field}"))
}

struct StatusPaths {
    changed_files: u64,
    untracked_paths: Vec<PathBuf>,
    truncated: bool,
}

fn parse_status_z(bytes: &[u8]) -> Result<StatusPaths> {
    let mut changed_paths = HashSet::new();
    let mut result = StatusPaths {
        changed_files: 0,
        untracked_paths: vec![],
        truncated: false,
    };
    let mut records = bytes.split(|byte| *byte == 0);
    while let Some(record) = records.next() {
        if record.is_empty() {
            continue;
        }
        if record.len() < 4 || record[2] != b' ' {
            return Err(malformed("status record"));
        }
        let status = &record[..2];
        let path = &record[3..];
        // Porcelain -z lists destination then the old path as a separate field.
        // A rename/copy is still one changed file.
        if status.iter().any(|byte| matches!(byte, b'R' | b'C')) {
            if records.next().is_none_or(|path| path.is_empty()) {
                return Err(malformed("rename record"));
            }
        }
        changed_paths.insert(path.to_vec());
        if status == b"??" {
            if result.untracked_paths.len() >= GIT_STATUS_FILE_LIMIT {
                result.truncated = true;
            } else if let Some(path) = path_from_bytes(path) {
                result.untracked_paths.push(path);
            } else {
                result.truncated = true;
            }
        }
    }
    result.changed_files = changed_paths.len() as u64;
    Ok(result)
}

fn path_from_bytes(bytes: &[u8]) -> Option<PathBuf> {
    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStringExt;
        Some(PathBuf::from(std::ffi::OsString::from_vec(bytes.to_vec())))
    }
    #[cfg(not(unix))]
    {
        std::str::from_utf8(bytes).ok().map(PathBuf::from)
    }
}

fn parse_numstat_z(bytes: &[u8]) -> Result<(u64, u64)> {
    let mut additions = 0_u64;
    let mut deletions = 0_u64;
    let mut records = bytes.split(|byte| *byte == 0);
    while let Some(record) = records.next() {
        if record.is_empty() {
            continue;
        }
        let mut columns = record.splitn(3, |byte| *byte == b'\t');
        let added = columns.next().ok_or_else(|| malformed("numstat"))?;
        let deleted = columns.next().ok_or_else(|| malformed("numstat"))?;
        let path = columns.next().ok_or_else(|| malformed("numstat"))?;
        if path.is_empty() {
            for _ in 0..2 {
                if records.next().is_none_or(|path| path.is_empty()) {
                    return Err(malformed("numstat rename"));
                }
            }
        }
        if added == b"-" && deleted == b"-" {
            continue;
        }
        let parse = |value: &[u8]| {
            std::str::from_utf8(value)
                .ok()
                .and_then(|value| value.parse::<u64>().ok())
                .ok_or_else(|| malformed("numstat count"))
        };
        additions = additions.saturating_add(parse(added)?);
        deletions = deletions.saturating_add(parse(deleted)?);
    }
    Ok((additions, deletions))
}

async fn untracked_additions(repository: &Path, paths: &[PathBuf]) -> Result<(u64, bool)> {
    let mut additions = 0_u64;
    let mut total_bytes = 0_u64;
    let mut truncated = false;
    for relative in paths {
        if relative.is_absolute()
            || relative
                .components()
                .any(|part| matches!(part, std::path::Component::ParentDir))
        {
            truncated = true;
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
                truncated = true;
                continue;
            }
            Err(error) => return Err(AppError::Io(error)),
        };
        if !metadata.is_file() || metadata.len() > GIT_UNTRACKED_FILE_LIMIT {
            truncated = true;
            continue;
        }
        if total_bytes.saturating_add(metadata.len()) > GIT_UNTRACKED_TOTAL_LIMIT {
            truncated = true;
            break;
        }
        let canonical = match std::fs::canonicalize(&path) {
            Ok(path) if path.starts_with(repository) => path,
            _ => {
                truncated = true;
                continue;
            }
        };
        let file = match tokio::fs::File::open(canonical).await {
            Ok(file) => file,
            Err(error)
                if matches!(
                    error.kind(),
                    io::ErrorKind::PermissionDenied | io::ErrorKind::NotFound
                ) =>
            {
                truncated = true;
                continue;
            }
            Err(error) => return Err(AppError::Io(error)),
        };
        let mut bytes = Vec::new();
        file.take(GIT_UNTRACKED_FILE_LIMIT + 1)
            .read_to_end(&mut bytes)
            .await?;
        total_bytes = total_bytes.saturating_add(bytes.len() as u64);
        if bytes.len() as u64 > GIT_UNTRACKED_FILE_LIMIT || total_bytes > GIT_UNTRACKED_TOTAL_LIMIT
        {
            truncated = true;
            continue;
        }
        if bytes.contains(&0) {
            continue;
        }
        if !bytes.is_empty() {
            additions = additions.saturating_add(
                bytes.iter().filter(|byte| **byte == b'\n').count() as u64
                    + u64::from(!bytes.ends_with(b"\n")),
            );
        }
    }
    Ok((additions, truncated))
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
        async fn new() -> Self {
            let root =
                std::env::temp_dir().join(format!("todex-git-status-{}", uuid::Uuid::new_v4()));
            fs::create_dir_all(root.join("repo")).unwrap();
            let root = fs::canonicalize(root).unwrap();
            let fixture = Self {
                repo: root.join("repo"),
                root,
            };
            fixture.git(&["init", "--template="]).await;
            fixture.git(&["config", "user.name", "Test"]).await;
            fixture
                .git(&["config", "user.email", "test@example.invalid"])
                .await;
            fixture
        }
        async fn git(&self, args: &[&str]) {
            command(&self.repo, args).await.unwrap();
        }
        async fn commit(&self) {
            self.git(&["add", "--all"]).await;
            self.git(&[
                "-c",
                "commit.gpgSign=false",
                "-c",
                "core.hooksPath=/dev/null",
                "commit",
                "--allow-empty",
                "-m",
                "Initial",
            ])
            .await;
        }
        async fn status(&self) -> GitStatusResponse {
            read(&self.root, &self.repo).await.unwrap()
        }
    }
    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.root);
        }
    }

    #[tokio::test]
    async fn status_resolves_main_linked_and_subdirectories_without_scanning_children() {
        let fixture = Fixture::new().await;
        fixture.commit().await;
        let not_repository = read(&fixture.root, &fixture.root).await.unwrap();
        assert!(
            !not_repository.initialized,
            "a child repository must not be scanned"
        );
        let payload = serde_json::to_value(not_repository).unwrap();
        assert_eq!(payload["branch"], serde_json::Value::Null);
        assert_eq!(payload["worktreeKind"], serde_json::Value::Null);
        assert_eq!(payload["changedFiles"], 0);
        assert_eq!(payload["additions"], 0);
        assert_eq!(payload["deletions"], 0);
        let main = fixture.status().await;
        assert_eq!(main.worktree_kind, Some(GitWorktreeKind::Main));
        assert!(main.branch.is_some());
        assert_eq!(main.changed_files, 0);
        let nested = fixture.repo.join("nested");
        fs::create_dir(&nested).unwrap();
        let nested_status = read(&fixture.root, &nested).await.unwrap();
        assert_eq!(nested_status.repository_path, main.repository_path);
        assert_eq!(nested_status.branch, main.branch);
        let linked = fixture.root.join("linked");
        fixture
            .git(&[
                "-c",
                "core.hooksPath=/dev/null",
                "worktree",
                "add",
                "-b",
                "linked",
                linked.to_str().unwrap(),
            ])
            .await;
        let linked_status = read(&fixture.root, &linked).await.unwrap();
        assert_eq!(linked_status.worktree_kind, Some(GitWorktreeKind::Linked));
        assert_eq!(linked_status.branch.as_deref(), Some("linked"));
        assert_eq!(linked_status.changed_files, 0);
        fixture
            .git(&["-c", "core.hooksPath=/dev/null", "switch", "--detach"])
            .await;
        let detached = fixture.status().await;
        assert_eq!(detached.worktree_kind, Some(GitWorktreeKind::Main));
        assert_eq!(detached.branch, None);
    }

    #[tokio::test]
    async fn status_counts_staged_and_unstaged_net_changes_plus_untracked() {
        let fixture = Fixture::new().await;
        fs::write(fixture.repo.join("tracked"), "old\nkeep\n").unwrap();
        fixture.commit().await;
        fs::write(fixture.repo.join("tracked"), "old\nstaged\n").unwrap();
        fixture.git(&["add", "tracked"]).await;
        fs::write(fixture.repo.join("tracked"), "new\nstaged\nextra\n").unwrap();
        fs::write(
            fixture.repo.join("untracked with\ttab\nnewline"),
            "one\ntwo",
        )
        .unwrap();
        let status = fixture.status().await;
        assert_eq!(status.changed_files, 2);
        assert_eq!((status.additions, status.deletions), (5, 2));
        assert!(!status.stats_truncated);
    }

    #[tokio::test]
    async fn status_counts_unborn_staged_files_from_empty_tree_once() {
        let fixture = Fixture::new().await;
        fs::write(fixture.repo.join("staged"), "first\n").unwrap();
        fs::write(fixture.repo.join("removed"), "will be removed\n").unwrap();
        fixture.git(&["add", "--all"]).await;
        fs::write(fixture.repo.join("staged"), "updated\nsecond\n").unwrap();
        fs::remove_file(fixture.repo.join("removed")).unwrap();
        let index_before = fs::read(fixture.repo.join(".git/index")).unwrap();
        let status = fixture.status().await;
        assert!(status.initialized);
        assert_eq!(status.changed_files, 2);
        assert_eq!((status.additions, status.deletions), (2, 0));
        assert!(!status.stats_truncated);
        // The status read must not write an empty-tree object or create a commit.
        assert!(command(&fixture.repo, &["rev-parse", "--verify", "HEAD"])
            .await
            .is_err());
        assert_eq!(
            fs::read(fixture.repo.join(".git/index")).unwrap(),
            index_before
        );
        let empty_tree = command(&fixture.repo, &["hash-object", "-t", "tree", "--stdin"])
            .await
            .unwrap();
        let empty_tree = std::str::from_utf8(&empty_tree.stdout).unwrap().trim();
        assert!(!fixture
            .repo
            .join(".git/objects")
            .join(&empty_tree[..2])
            .join(&empty_tree[2..])
            .exists());
    }

    #[tokio::test]
    async fn status_counts_renames_once_and_does_not_invent_binary_lines() {
        let fixture = Fixture::new().await;
        let old = "old name\twith\nline";
        let new = "new name\twith\nline";
        fs::write(fixture.repo.join(old), "a\nb\nc\nd\ne\nf\ng\nh\ni\nj\n").unwrap();
        fs::write(fixture.repo.join("binary"), b"\0old\n").unwrap();
        fixture.commit().await;
        fixture.git(&["mv", "--", old, new]).await;
        fs::write(
            fixture.repo.join(new),
            "a\nb\nc\nd\ne\nf\ng\nh\ni\nj\nnew\n",
        )
        .unwrap();
        fs::write(fixture.repo.join("binary"), b"\0new\nextra\n").unwrap();
        fs::write(fixture.repo.join("untracked-binary"), b"\0untracked\n").unwrap();
        let status = fixture.status().await;
        assert_eq!(status.changed_files, 3);
        assert_eq!((status.additions, status.deletions), (1, 0));
        assert!(!status.stats_truncated);
    }

    #[tokio::test]
    async fn status_flags_untracked_limits_and_never_reads_external_symlinks() {
        let fixture = Fixture::new().await;
        fixture.commit().await;
        fs::write(fixture.repo.join("small"), "one\n").unwrap();
        fs::File::create(fixture.repo.join("large"))
            .unwrap()
            .set_len(GIT_UNTRACKED_FILE_LIMIT + 1)
            .unwrap();
        fs::write(fixture.root.join("external"), "do not count this\n").unwrap();
        std::os::unix::fs::symlink(fixture.root.join("external"), fixture.repo.join("link"))
            .unwrap();
        let status = fixture.status().await;
        assert_eq!(status.changed_files, 3);
        assert_eq!((status.additions, status.deletions), (1, 0));
        assert!(status.stats_truncated);
        let records = (0..=GIT_STATUS_FILE_LIMIT)
            .map(|index| format!("?? file-{index}\0"))
            .collect::<String>();
        let parsed = parse_status_z(records.as_bytes()).unwrap();
        assert!(parsed.truncated);
        assert_eq!(parsed.changed_files, GIT_STATUS_FILE_LIMIT as u64 + 1);
        assert_eq!(parsed.untracked_paths.len(), GIT_STATUS_FILE_LIMIT);
    }
}
