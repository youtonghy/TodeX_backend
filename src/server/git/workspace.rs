//! Fixed, shell-free Git workspace actions. All mutations share the legacy Git
//! write lock and metadata/configuration checks; failures never invoke an Agent.
use super::*;
use crate::server::protocol::{
    GitBranch, GitOperation, GitOperationResponse, GitWorkspaceResponse, GitWorktree,
};

const MAX_REFS: usize = 2_000;
const MAX_WORKTREES: usize = 128;

fn invalid(message: &str) -> AppError {
    AppError::InvalidRequest(message.to_owned())
}

async fn text_command(cwd: &Path, args: &[&str]) -> Result<String> {
    let output = run_checked(cwd, &git_args(args), "Git workspace inspection").await?;
    String::from_utf8(output.stdout).map_err(|_| invalid("Git returned non-UTF-8 data"))
}

async fn is_dirty(cwd: &Path) -> Result<bool> {
    validate_mutation_execution_config(cwd).await?;
    Ok(!text_command(
        cwd,
        &["status", "--porcelain=v1", "-z", "--untracked-files=normal"],
    )
    .await?
    .is_empty())
}

// Removing a worktree also deletes ignored files. Include them in its safety
// status (while normal repository dirty state follows ordinary Git semantics).
async fn is_removal_dirty(cwd: &Path) -> Result<bool> {
    validate_mutation_execution_config(cwd).await?;
    Ok(!text_command(
        cwd,
        &[
            "status",
            "--porcelain=v1",
            "-z",
            "--untracked-files=normal",
            "--ignored=matching",
        ],
    )
    .await?
    .is_empty())
}

pub(crate) async fn snapshot(root: &Path, workspace: &Path) -> Result<GitWorkspaceResponse> {
    let _permit = timeout(GIT_SCAN_QUEUE_TIMEOUT, GIT_SCAN_SEMAPHORE.acquire())
        .await
        .map_err(|_| AppError::Conflict("Git read capacity is busy".to_owned()))?
        .map_err(|_| AppError::Conflict("Git read capacity is closed".to_owned()))?;
    timeout(GIT_SCAN_TIMEOUT, snapshot_inner(root, workspace))
        .await
        .map_err(|_| AppError::GitCommandTimedOut("Git workspace inspection".to_owned()))?
}

async fn is_exact_repository(root: &Path, workspace: &Path) -> Result<bool> {
    match resolve_repository(root, workspace).await? {
        Some(repository) if repository != workspace => Err(invalid(
            "请选择仓库根目录后再执行 Git 操作；当前工作目录是仓库的子目录。",
        )),
        Some(_) => Ok(true),
        None => Ok(false),
    }
}

async fn snapshot_inner(root: &Path, workspace: &Path) -> Result<GitWorkspaceResponse> {
    let root = canonical_workspace_root(root)?;
    let workspace = validate_workspace_directory(&root, workspace)?;
    let mut result = GitWorkspaceResponse {
        repository_path: workspace.display().to_string(),
        initialized: false,
        current_branch: String::new(),
        branches: vec![],
        worktrees: vec![],
        dirty: false,
    };
    if !is_exact_repository(&root, &workspace).await? {
        return Ok(result);
    }
    validate_repository_metadata(&root, &workspace).await?;
    result.initialized = true;
    result.current_branch = text_command(&workspace, &["branch", "--show-current"])
        .await?
        .trim()
        .to_owned();
    result.dirty = is_dirty(&workspace).await?;
    result.worktrees = worktrees(&root, &workspace).await?;
    let refs = text_command(
        &workspace,
        &[
            "for-each-ref",
            "--format=%(refname)%00%(symref)",
            "refs/heads",
            "refs/remotes",
        ],
    )
    .await?;
    for line in refs.lines() {
        if result.branches.len() >= MAX_REFS {
            return Err(invalid(
                "Too many branches; narrow this repository before listing",
            ));
        }
        let (name, symref) = line
            .split_once('\0')
            .ok_or_else(|| invalid("Invalid Git ref record"))?;
        if !symref.is_empty() {
            continue;
        }
        let (name, remote) = if let Some(name) = name.strip_prefix("refs/heads/") {
            (name, false)
        } else if let Some(name) = name.strip_prefix("refs/remotes/") {
            (name, true)
        } else {
            continue;
        };
        result.branches.push(GitBranch {
            name: name.to_owned(),
            remote,
            current: !remote && name == result.current_branch,
            worktree_path: if remote {
                None
            } else {
                result
                    .worktrees
                    .iter()
                    .find(|tree| tree.branch == name)
                    .map(|tree| tree.path.clone())
            },
        });
    }
    Ok(result)
}

async fn worktrees(root: &Path, workspace: &Path) -> Result<Vec<GitWorktree>> {
    let records = text_command(workspace, &["worktree", "list", "--porcelain", "-z"]).await?;
    let mut trees = Vec::new();
    for record in records.split("\0\0").filter(|record| !record.is_empty()) {
        if trees.len() >= MAX_WORKTREES {
            return Err(invalid("Too many worktrees"));
        }
        let fields: Vec<_> = record.split('\0').collect();
        let path = fields
            .iter()
            .find_map(|field| field.strip_prefix("worktree "))
            .ok_or_else(|| invalid("Invalid Git worktree record"))?;
        let branch = fields
            .iter()
            .find_map(|field| field.strip_prefix("branch refs/heads/"))
            .unwrap_or("");
        let accessible_path = validate_workspace_directory(root, Path::new(path)).ok();
        let accessible = if let Some(path) = accessible_path.as_deref() {
            validate_repository_metadata(root, path).await.is_ok()
        } else {
            false
        };
        // A missing/out-of-bounds worktree is never a safe deletion candidate.
        let dirty = if accessible {
            is_removal_dirty(accessible_path.as_deref().unwrap())
                .await
                .unwrap_or(true)
        } else {
            true
        };
        trees.push(GitWorktree {
            path: path.to_owned(),
            branch: branch.to_owned(),
            current: accessible_path.as_deref() == Some(workspace),
            main: trees.is_empty(),
            locked: fields
                .iter()
                .any(|field| *field == "locked" || field.starts_with("locked ")),
            dirty,
            accessible,
        });
    }
    Ok(trees)
}

async fn validate_branch(workspace: &Path, name: &str) -> Result<()> {
    if name.is_empty()
        || name.len() > 240
        || name.starts_with('-')
        || name.contains('@')
        || name.chars().any(char::is_control)
    {
        return Err(invalid("Invalid branch name"));
    }
    text_command(workspace, &["check-ref-format", "--branch", name]).await?;
    Ok(())
}

async fn start_commit(workspace: &Path, value: Option<&str>) -> Result<String> {
    let value = value.unwrap_or("HEAD");
    if value.is_empty()
        || value.len() > 240
        || value.starts_with('-')
        || value.chars().any(char::is_control)
    {
        return Err(invalid("Invalid branch starting point"));
    }
    // Resolve revision syntax to an immutable object id before a mutating command.
    let oid = text_command(
        workspace,
        &[
            "rev-parse",
            "--verify",
            "--end-of-options",
            &format!("{value}^{{commit}}"),
        ],
    )
    .await?;
    let oid = oid.trim();
    if !(oid.len() == 40 || oid.len() == 64) || !oid.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Err(invalid("Invalid commit id"));
    }
    Ok(oid.to_owned())
}

fn new_worktree_path(root: &Path, value: &str) -> Result<PathBuf> {
    let path = Path::new(value);
    if !path.is_absolute()
        || path.components().any(|part| {
            matches!(
                part,
                std::path::Component::ParentDir | std::path::Component::CurDir
            )
        })
    {
        return Err(invalid(
            "Worktree path must be absolute without relative components",
        ));
    }
    let parent = path
        .parent()
        .ok_or_else(|| invalid("Worktree needs a parent directory"))?;
    let parent = validate_workspace_directory(root, parent)?;
    let name = path
        .file_name()
        .ok_or_else(|| invalid("Worktree needs a directory name"))?;
    let destination = parent.join(name);
    match std::fs::symlink_metadata(&destination) {
        Ok(_) => Err(invalid("Worktree destination must not already exist")),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(destination),
        Err(error) => Err(AppError::Io(error)),
    }
}

pub(crate) async fn operate(
    root: &Path,
    data_dir: &Path,
    workspace: &Path,
    operation: &GitOperation,
) -> Result<GitOperationResponse> {
    #[cfg(not(unix))]
    {
        let _ = (root, data_dir, workspace, operation);
        return Err(AppError::Unsupported(
            "Fixed Git mutations require Unix process isolation".to_owned(),
        ));
    }
    #[cfg(unix)]
    {
        let root = canonical_workspace_root(root)?;
        let workspace = validate_workspace_directory(&root, workspace)?;
        let _guard = timeout(GIT_WRITE_QUEUE_TIMEOUT, GIT_WRITE_LOCK.lock())
            .await
            .map_err(|_| AppError::Conflict("Git write capacity is busy".to_owned()))?;
        timeout(
            GIT_MUTATION_TIMEOUT,
            operate_locked(&root, data_dir, &workspace, operation),
        )
        .await
        .map_err(|_| AppError::GitPartialSuccess {
            repository_path: workspace.display().to_string(),
            operation: operation.action().to_owned(),
            detail: "Operation timed out; refresh repository state before retrying".to_owned(),
        })?
    }
}

async fn operate_locked(
    root: &Path,
    data_dir: &Path,
    workspace: &Path,
    operation: &GitOperation,
) -> Result<GitOperationResponse> {
    let initialized = is_exact_repository(root, workspace).await?;
    if !initialized && !matches!(operation, GitOperation::Init {}) {
        return Err(AppError::GitRepositoryNotFound);
    }
    if initialized {
        validate_mutation_repository_metadata(root, workspace).await?;
        validate_mutation_execution_config(workspace).await?;
    } else if std::fs::symlink_metadata(workspace.join(".git")).is_ok() {
        return Err(invalid(
            "Existing Git metadata is invalid; ask the Agent to inspect it",
        ));
    }
    let hooks = prepare_disabled_hooks_directory(data_dir).await?;
    let args = match operation {
        GitOperation::CreatePr {
            title,
            body,
            base_branch,
            draft,
            repository,
        } => {
            let output = super::pull_request::create(
                workspace,
                title,
                body,
                base_branch,
                *draft,
                repository,
            )
            .await?;
            return Ok(GitOperationResponse {
                repository_path: workspace.display().to_string(),
                action: operation.action().to_owned(),
                output,
            });
        }
        GitOperation::Init {} => {
            if initialized {
                return Err(invalid("Repository is already initialized"));
            }
            git_args(&["init", "--template="])
        }
        GitOperation::CreateBranch {
            branch_name,
            start_point,
        } => {
            validate_branch(workspace, branch_name).await?;
            let oid = start_commit(workspace, start_point.as_deref()).await?;
            git_args(&["branch", "--", branch_name, &oid])
        }
        GitOperation::SwitchBranch { branch_name } => {
            validate_branch(workspace, branch_name).await?;
            if is_dirty(workspace).await? {
                return Err(invalid(
                    "Working tree has changes; commit or resolve them before switching",
                ));
            }
            let trees = worktrees(root, workspace).await?;
            if trees
                .iter()
                .any(|tree| tree.branch == *branch_name && !tree.current)
            {
                return Err(invalid("Branch is already checked out in another worktree"));
            }
            // --no-guess prevents an implicit remote-tracking branch creation.
            git_args(&["switch", "--no-guess", "--", branch_name])
        }
        GitOperation::CreateWorktree {
            path,
            branch_name,
            start_point,
        } => {
            validate_branch(workspace, branch_name).await?;
            let path = new_worktree_path(root, path)?;
            let oid = start_commit(workspace, start_point.as_deref()).await?;
            vec![
                "worktree".to_owned(),
                "add".to_owned(),
                "-b".to_owned(),
                branch_name.clone(),
                "--".to_owned(),
                path.to_string_lossy().into_owned(),
                oid,
            ]
        }
        GitOperation::RemoveWorktree { path } => {
            let target = validate_workspace_directory(root, Path::new(path))?;
            let trees = worktrees(root, workspace).await?;
            let tree = trees
                .iter()
                .find(|tree| Path::new(&tree.path) == target)
                .ok_or_else(|| invalid("Worktree is not registered in this repository"))?;
            if tree.main || tree.current || tree.locked || tree.dirty || !tree.accessible {
                return Err(invalid("Only a clean, unlocked, accessible, non-current linked worktree can be removed"));
            }
            validate_mutation_repository_metadata(root, &target).await?;
            // Never delete a registered path whose .git pointer has been replaced.
            if repository_metadata_roots(root, &target).await?.last()
                != repository_metadata_roots(root, workspace).await?.last()
            {
                return Err(invalid(
                    "Worktree metadata does not belong to this repository",
                ));
            }
            vec![
                "worktree".to_owned(),
                "remove".to_owned(),
                "--".to_owned(),
                target.to_string_lossy().into_owned(),
            ]
        }
        GitOperation::Push {} => {
            let branch = text_command(workspace, &["symbolic-ref", "--quiet", "HEAD"]).await?;
            let upstream = text_command(
                workspace,
                &[
                    "for-each-ref",
                    "--format=%(upstream:remotename)%00%(upstream:remoteref)",
                    branch.trim(),
                ],
            )
            .await?;
            let (remote, reference) = upstream
                .trim()
                .split_once('\0')
                .ok_or_else(|| invalid("Current branch has no upstream"))?;
            if remote.is_empty() || remote.starts_with('-') || !reference.starts_with("refs/heads/")
            {
                return Err(invalid(
                    "Current branch needs a branch upstream before pushing",
                ));
            }
            // Explicit refspec bypasses push.default and remote.*.push (including
            // force refspecs). No force, tags, mirror, upstream setup, or hooks.
            git_args(&[
                "-c",
                &format!("remote.{remote}.mirror=false"),
                "push",
                "--no-force",
                "--no-follow-tags",
                "--",
                remote,
                &format!("HEAD:{reference}"),
            ])
        }
    };
    let mut output = String::new();
    run_step(
        workspace,
        mutation_git_args_owned(
            &hooks,
            [git_args(&["-c", "submodule.recurse=false"]), args].concat(),
        ),
        operation.action(),
        &mut output,
    )
    .await
    .map_err(|error| AppError::GitPartialSuccess {
        repository_path: workspace.display().to_string(),
        operation: operation.action().to_owned(),
        detail: format!(
            "Git may have changed repository or remote state; refresh before retrying. {}",
            truncate_text(&error.to_string(), GIT_ERROR_DETAIL_LIMIT)
        ),
    })?;
    if output.is_empty() {
        output = "Operation completed".to_owned();
    }
    Ok(GitOperationResponse {
        repository_path: workspace.display().to_string(),
        action: operation.action().to_owned(),
        output,
    })
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::fs;

    #[tokio::test]
    async fn create_pr_requires_pushed_upstream_without_mutating_checkout() {
        let fixture = Fixture::new();
        fixture.seed().await;
        let head = text_command(&fixture.repo, &["rev-parse", "HEAD"])
            .await
            .unwrap();
        let result = fixture
            .action(GitOperation::CreatePr {
                title: "Example".to_owned(),
                body: "First line\nSecond line".to_owned(),
                base_branch: "main".to_owned(),
                draft: true,
                repository: "owner/repo".to_owned(),
            })
            .await;
        assert!(result.unwrap_err().to_string().contains("upstream"));
        assert_eq!(
            head,
            text_command(&fixture.repo, &["rev-parse", "HEAD"])
                .await
                .unwrap()
        );
        assert!(!is_dirty(&fixture.repo).await.unwrap());
    }

    struct Fixture {
        root: PathBuf,
        repo: PathBuf,
        data: PathBuf,
    }
    impl Fixture {
        fn new() -> Self {
            let root =
                std::env::temp_dir().join(format!("todex-git-workspace-{}", uuid::Uuid::new_v4()));
            fs::create_dir_all(root.join("repo")).unwrap();
            let root = fs::canonicalize(root).unwrap();
            Self {
                repo: root.join("repo"),
                data: root.join("data"),
                root,
            }
        }
        async fn action(&self, operation: GitOperation) -> Result<GitOperationResponse> {
            operate(&self.root, &self.data, &self.repo, &operation).await
        }
        async fn seed(&self) {
            self.action(GitOperation::Init {}).await.unwrap();
            text_command(&self.repo, &["config", "user.name", "Test"])
                .await
                .unwrap();
            text_command(
                &self.repo,
                &["config", "user.email", "test@example.invalid"],
            )
            .await
            .unwrap();
            fs::write(self.repo.join("README.md"), "initial\n").unwrap();
            text_command(&self.repo, &["add", "README.md"])
                .await
                .unwrap();
            text_command(
                &self.repo,
                &[
                    "-c",
                    "commit.gpgSign=false",
                    "-c",
                    "core.hooksPath=/dev/null",
                    "commit",
                    "-m",
                    "Initial",
                ],
            )
            .await
            .unwrap();
        }
    }
    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.root);
        }
    }

    #[tokio::test]
    async fn init_branch_and_worktree_lifecycle_preserves_commits_and_branches() {
        let fixture = Fixture::new();
        let before = snapshot(&fixture.root, &fixture.repo).await.unwrap();
        assert!(!before.initialized);
        fixture.action(GitOperation::Init {}).await.unwrap();
        let initialized = snapshot(&fixture.root, &fixture.repo).await.unwrap();
        assert!(initialized.initialized);
        assert!(initialized.branches.is_empty()); // init deliberately creates no commit
        assert!(start_commit(&fixture.repo, None).await.is_err());
        text_command(&fixture.repo, &["config", "user.name", "Test"])
            .await
            .unwrap();
        text_command(
            &fixture.repo,
            &["config", "user.email", "test@example.invalid"],
        )
        .await
        .unwrap();
        text_command(
            &fixture.repo,
            &[
                "-c",
                "commit.gpgSign=false",
                "-c",
                "core.hooksPath=/dev/null",
                "commit",
                "--allow-empty",
                "-m",
                "Initial",
            ],
        )
        .await
        .unwrap();
        let before_branch = snapshot(&fixture.root, &fixture.repo)
            .await
            .unwrap()
            .current_branch;
        fixture
            .action(GitOperation::CreateBranch {
                branch_name: "feature".into(),
                start_point: None,
            })
            .await
            .unwrap();
        assert_eq!(
            snapshot(&fixture.root, &fixture.repo)
                .await
                .unwrap()
                .current_branch,
            before_branch
        );
        fixture
            .action(GitOperation::SwitchBranch {
                branch_name: "feature".into(),
            })
            .await
            .unwrap();
        let tree = fixture.root.join("linked tree");
        fixture
            .action(GitOperation::CreateWorktree {
                path: tree.display().to_string(),
                branch_name: "linked".into(),
                start_point: Some("feature".into()),
            })
            .await
            .unwrap();
        let state = snapshot(&fixture.root, &fixture.repo).await.unwrap();
        assert_eq!(state.current_branch, "feature");
        assert_eq!(state.worktrees.len(), 2);
        assert!(state.worktrees[0].main && state.worktrees[0].current);
        assert!(state.worktrees[1].accessible && !state.worktrees[1].dirty);
        assert_eq!(
            state
                .branches
                .iter()
                .find(|branch| branch.name == "linked")
                .unwrap()
                .worktree_path
                .as_deref(),
            tree.to_str()
        );
        assert!(fixture
            .action(GitOperation::SwitchBranch {
                branch_name: "linked".into()
            })
            .await
            .is_err());
        fixture
            .action(GitOperation::RemoveWorktree {
                path: tree.display().to_string(),
            })
            .await
            .unwrap();
        assert!(!tree.exists());
        assert!(snapshot(&fixture.root, &fixture.repo)
            .await
            .unwrap()
            .branches
            .iter()
            .any(|branch| branch.name == "linked"));
        let duplicate_branch = fixture
            .action(GitOperation::CreateWorktree {
                path: tree.display().to_string(),
                branch_name: "linked".into(),
                start_point: None,
            })
            .await;
        assert!(
            matches!(duplicate_branch, Err(AppError::GitPartialSuccess { .. })),
            "Executed mutation failures must require refresh before another attempt"
        );
        assert!(!tree.exists());
    }

    #[tokio::test]
    async fn nested_workspace_never_lists_or_initializes_a_parent_repository() {
        let fixture = Fixture::new();
        fixture.seed().await;
        let nested = fixture.repo.join("nested");
        fs::create_dir(&nested).unwrap();
        let before = start_commit(&fixture.repo, None).await.unwrap();
        let read_error = snapshot(&fixture.root, &nested).await.unwrap_err();
        assert!(
            matches!(read_error, AppError::InvalidRequest(ref message) if message.contains("请选择仓库根目录"))
        );
        let init_error = operate(
            &fixture.root,
            &fixture.data,
            &nested,
            &GitOperation::Init {},
        )
        .await
        .unwrap_err();
        assert!(
            matches!(init_error, AppError::InvalidRequest(ref message) if message.contains("请选择仓库根目录"))
        );
        assert!(!nested.join(".git").exists());
        assert_eq!(start_commit(&fixture.repo, None).await.unwrap(), before);
    }

    #[tokio::test]
    async fn unsafe_refs_dirty_switches_and_worktree_deletions_are_rejected() {
        let fixture = Fixture::new();
        fixture.seed().await;
        for name in [
            "-f",
            "@{-1}",
            "a..b",
            "feature\nnext",
            "refs/heads/../../escape",
        ] {
            assert!(
                fixture
                    .action(GitOperation::CreateBranch {
                        branch_name: name.into(),
                        start_point: None
                    })
                    .await
                    .is_err(),
                "{name}"
            );
        }
        fixture
            .action(GitOperation::CreateBranch {
                branch_name: "feature".into(),
                start_point: None,
            })
            .await
            .unwrap();
        fs::write(fixture.repo.join("untracked.txt"), "keep").unwrap();
        assert!(fixture
            .action(GitOperation::SwitchBranch {
                branch_name: "feature".into()
            })
            .await
            .is_err());
        let tree = fixture.root.join("linked");
        fixture
            .action(GitOperation::CreateWorktree {
                path: tree.display().to_string(),
                branch_name: "linked".into(),
                start_point: None,
            })
            .await
            .unwrap();
        let remove = || GitOperation::RemoveWorktree {
            path: tree.display().to_string(),
        };
        fs::write(tree.join("secret.env"), "must keep").unwrap();
        text_command(&tree, &["config", "core.excludesFile", "/dev/null"])
            .await
            .unwrap();
        fs::create_dir_all(fixture.repo.join(".git/info")).unwrap();
        fs::write(fixture.repo.join(".git/info/exclude"), "secret.env\n").unwrap();
        assert!(fixture.action(remove()).await.is_err());
        assert!(tree.join("secret.env").exists());
        fs::remove_file(tree.join("secret.env")).unwrap();
        text_command(&fixture.repo, &["worktree", "lock", tree.to_str().unwrap()])
            .await
            .unwrap();
        assert!(fixture.action(remove()).await.is_err());
        assert!(fixture
            .action(GitOperation::RemoveWorktree {
                path: fixture.repo.display().to_string()
            })
            .await
            .is_err());
        text_command(
            &fixture.repo,
            &["worktree", "unlock", tree.to_str().unwrap()],
        )
        .await
        .unwrap();
        fs::remove_file(tree.join(".git")).unwrap();
        assert!(fixture.action(remove()).await.is_err());
        assert!(tree.join("README.md").exists());
    }

    #[tokio::test]
    async fn rejects_path_escapes_symlink_metadata_and_checkout_filters() {
        use std::os::unix::fs::symlink;
        let fixture = Fixture::new();
        fixture.seed().await;
        let outside = Fixture::new();
        symlink(&outside.root, fixture.root.join("escape")).unwrap();
        for path in [
            outside.root.join("outside"),
            fixture.root.join("escape/new"),
            fixture.root.join("../new"),
            fixture.repo.clone(),
        ] {
            assert!(fixture
                .action(GitOperation::CreateWorktree {
                    path: path.display().to_string(),
                    branch_name: "new".into(),
                    start_point: None
                })
                .await
                .is_err());
        }
        text_command(
            &fixture.repo,
            &["config", "filter.bad.smudge", "touch should-not-run"],
        )
        .await
        .unwrap();
        assert!(fixture
            .action(GitOperation::SwitchBranch {
                branch_name: "feature".into()
            })
            .await
            .is_err());
        assert!(!fixture.repo.join("should-not-run").exists());
        text_command(&fixture.repo, &["config", "--unset", "filter.bad.smudge"])
            .await
            .unwrap();
        fs::rename(fixture.repo.join(".git"), fixture.root.join("metadata")).unwrap();
        symlink(fixture.root.join("metadata"), fixture.repo.join(".git")).unwrap();
        assert!(fixture
            .action(GitOperation::CreateBranch {
                branch_name: "feature".into(),
                start_point: None
            })
            .await
            .is_err());
    }

    #[tokio::test]
    async fn push_uses_only_upstream_and_never_forces_or_follows_tags() {
        struct Daemon(std::process::Child);
        impl Drop for Daemon {
            fn drop(&mut self) {
                let _ = self.0.kill();
                let _ = self.0.wait();
            }
        }
        let fixture = Fixture::new();
        fixture.seed().await;
        assert!(fixture.action(GitOperation::Push {}).await.is_err());
        let initial = start_commit(&fixture.repo, None).await.unwrap();
        let bare = fixture.root.join("remote.git");
        fs::create_dir(&bare).unwrap();
        text_command(&bare, &["init", "--bare", "--template="])
            .await
            .unwrap();
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        drop(listener);
        let _daemon = Daemon(
            std::process::Command::new("git")
                .args([
                    "daemon",
                    "--listen=127.0.0.1",
                    &format!("--port={port}"),
                    "--reuseaddr",
                    "--export-all",
                    "--enable=receive-pack",
                    &format!("--base-path={}", fixture.root.display()),
                ])
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .spawn()
                .unwrap(),
        );
        let mut ready = false;
        for _ in 0..50 {
            if std::net::TcpStream::connect(("127.0.0.1", port)).is_ok() {
                ready = true;
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        assert!(ready, "loopback Git daemon did not start");
        text_command(
            &fixture.repo,
            &[
                "remote",
                "add",
                "origin",
                &format!("git://127.0.0.1:{port}/remote.git"),
            ],
        )
        .await
        .unwrap();
        text_command(
            &fixture.repo,
            &[
                "-c",
                "core.hooksPath=/dev/null",
                "push",
                "origin",
                "HEAD:refs/heads/upstream",
            ],
        )
        .await
        .unwrap();
        let branch = text_command(&fixture.repo, &["branch", "--show-current"])
            .await
            .unwrap();
        for (key, value) in [
            (format!("branch.{}.remote", branch.trim()), "origin"),
            (
                format!("branch.{}.merge", branch.trim()),
                "refs/heads/upstream",
            ),
            ("remote.origin.push".into(), "+HEAD:refs/heads/unrequested"),
            ("remote.origin.mirror".into(), "true"),
            ("push.followTags".into(), "true"),
        ] {
            text_command(&fixture.repo, &["config", &key, value])
                .await
                .unwrap();
        }
        text_command(
            &fixture.repo,
            &[
                "-c",
                "tag.gpgSign=false",
                "tag",
                "-a",
                "unrequested-tag",
                "-m",
                "Tag",
            ],
        )
        .await
        .unwrap();
        text_command(
            &fixture.repo,
            &[
                "-c",
                "commit.gpgSign=false",
                "-c",
                "core.hooksPath=/dev/null",
                "commit",
                "--allow-empty",
                "-m",
                "Forward",
            ],
        )
        .await
        .unwrap();
        fixture.action(GitOperation::Push {}).await.unwrap();
        let remote_head = text_command(&bare, &["rev-parse", "refs/heads/upstream"])
            .await
            .unwrap();
        assert_eq!(
            remote_head.trim(),
            start_commit(&fixture.repo, None).await.unwrap()
        );
        assert_eq!(
            text_command(&bare, &["for-each-ref", "--format=%(refname)"])
                .await
                .unwrap()
                .trim(),
            "refs/heads/upstream"
        );
        text_command(&fixture.repo, &["reset", "--hard", &initial])
            .await
            .unwrap();
        text_command(
            &fixture.repo,
            &[
                "-c",
                "commit.gpgSign=false",
                "-c",
                "core.hooksPath=/dev/null",
                "commit",
                "--allow-empty",
                "-m",
                "Diverged",
            ],
        )
        .await
        .unwrap();
        assert!(fixture.action(GitOperation::Push {}).await.is_err());
        assert_eq!(
            text_command(&bare, &["rev-parse", "refs/heads/upstream"])
                .await
                .unwrap(),
            remote_head
        );
    }

    #[test]
    fn operation_schema_rejects_arbitrary_fields_and_commands() {
        use crate::server::protocol::GitOperationRequest;
        for operation in [
            serde_json::json!({"action":"shell", "command":"rm -rf /"}),
            serde_json::json!({"action":"init", "args":["--bare"]}),
            serde_json::json!({"action":"create-branch", "branchName":"safe", "force":true}),
        ] {
            assert!(serde_json::from_value::<GitOperationRequest>(
                serde_json::json!({"workspacePath":"/workspace", "operation":operation})
            )
            .is_err());
        }
    }
}
