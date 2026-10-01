//! Paged commit history for the current branch, plus the push base shared with
//! the status summary. Like status, a subdirectory resolves to its containing
//! repository.
use super::*;
use crate::server::protocol::{GitLogCommit, GitLogResponse};

const LOG_TIMEOUT: Duration = Duration::from_secs(10);
pub(crate) const GIT_LOG_PAGE_LIMIT: u32 = 50;
pub(crate) const GIT_LOG_SKIP_LIMIT: u32 = 100_000;
/// Upper bound on unpushed commits listed to mark a page. When a branch has
/// more, commits outside the listed set are reported as unknown, not pushed.
const UNPUSHED_LIST_LIMIT: usize = 10_000;
const AUTHOR_NAME_LIMIT: usize = 256;

/// What "pushed" is measured against: the branch upstream when one is
/// configured, otherwise any remote-tracking ref. Without remotes there is
/// nothing to compare with.
pub(super) enum PushBase {
    Upstream(String),
    Remotes,
    None,
}

pub(super) async fn push_base(repository: &Path) -> Result<PushBase> {
    let upstream = run_git_command(
        repository,
        &git_args(&[
            "--no-optional-locks",
            "rev-parse",
            "--abbrev-ref",
            "--symbolic-full-name",
            "@{upstream}",
        ]),
        "Git upstream",
    )
    .await?;
    // A detached HEAD or a branch without upstream exits non-zero; that is a
    // state, not an error.
    if upstream.status.success() {
        if let Ok(name) = std::str::from_utf8(&upstream.stdout) {
            let name = name.trim_end_matches(['\r', '\n']);
            if !name.is_empty() {
                return Ok(PushBase::Upstream(name.to_owned()));
            }
        }
    }
    let remotes = run_checked(
        repository,
        &git_args(&[
            "--no-optional-locks",
            "for-each-ref",
            "--count=1",
            "--format=%(refname)",
            "refs/remotes",
        ]),
        "Git remotes",
    )
    .await?;
    Ok(if remotes.stdout.is_empty() {
        PushBase::None
    } else {
        PushBase::Remotes
    })
}

/// `rev-list` arguments excluding everything already on the push base.
fn push_base_exclusion(base: &PushBase) -> Option<&'static [&'static str]> {
    match base {
        PushBase::Upstream(_) => Some(&["--not", "@{upstream}"]),
        PushBase::Remotes => Some(&["--not", "--remotes"]),
        PushBase::None => None,
    }
}

pub(super) async fn head_is_born(repository: &Path) -> Result<bool> {
    let head = run_git_command(
        repository,
        &git_args(&["rev-parse", "--verify", "--quiet", "HEAD^{commit}"]),
        "Git HEAD",
    )
    .await?;
    match head.status.code() {
        Some(0) => Ok(true),
        Some(1) => Ok(false),
        _ => Err(AppError::GitProcess(
            "Git returned an invalid HEAD".to_owned(),
        )),
    }
}

/// Commits ahead of / behind the push base. Behind is only meaningful against a
/// single upstream; against "any remote" only the unpushed count is reported.
pub(super) async fn divergence(
    repository: &Path,
    base: &PushBase,
) -> Result<(Option<u64>, Option<u64>)> {
    let arguments: &[&str] = match base {
        PushBase::Upstream(_) => &["rev-list", "--left-right", "--count", "HEAD...@{upstream}"],
        PushBase::Remotes => &["rev-list", "--count", "HEAD", "--not", "--remotes"],
        PushBase::None => return Ok((None, None)),
    };
    let output = command(repository, arguments).await?;
    let text = std::str::from_utf8(&output.stdout).map_err(|_| malformed("commit count"))?;
    let mut counts = text.split_whitespace().map(str::parse::<u64>);
    let ahead = counts
        .next()
        .and_then(|count| count.ok())
        .ok_or_else(|| malformed("commit count"))?;
    let behind = match base {
        PushBase::Upstream(_) => Some(
            counts
                .next()
                .and_then(|count| count.ok())
                .ok_or_else(|| malformed("commit count"))?,
        ),
        _ => None,
    };
    Ok((Some(ahead), behind))
}

pub(crate) async fn read(
    roots: &[PathBuf],
    workspace: &Path,
    skip: u32,
    limit: u32,
) -> Result<GitLogResponse> {
    if limit == 0 || limit > GIT_LOG_PAGE_LIMIT {
        return Err(AppError::InvalidRequest(format!(
            "limit must be between 1 and {GIT_LOG_PAGE_LIMIT}"
        )));
    }
    if skip > GIT_LOG_SKIP_LIMIT {
        return Err(AppError::InvalidRequest(format!(
            "skip must not exceed {GIT_LOG_SKIP_LIMIT}"
        )));
    }
    let _permit = timeout(GIT_SCAN_QUEUE_TIMEOUT, GIT_SCAN_SEMAPHORE.acquire())
        .await
        .map_err(|_| AppError::Conflict("Git read capacity is busy".to_owned()))?
        .map_err(|_| AppError::Conflict("Git read capacity is closed".to_owned()))?;
    timeout(LOG_TIMEOUT, read_inner(roots, workspace, skip, limit))
        .await
        .map_err(|_| AppError::GitCommandTimedOut("Git log".to_owned()))?
}

async fn command(repository: &Path, args: &[&str]) -> Result<GitCommandOutput> {
    let mut arguments = git_args(&["--no-optional-locks"]);
    arguments.extend(git_args(args));
    run_checked(repository, &arguments, "Git log").await
}

async fn read_inner(
    roots: &[PathBuf],
    workspace: &Path,
    skip: u32,
    limit: u32,
) -> Result<GitLogResponse> {
    let roots = canonical_workspace_roots(roots);
    let workspace = validate_workspace_directory(&roots, workspace)?;
    let mut result = GitLogResponse {
        repository_path: workspace.display().to_string(),
        initialized: false,
        commits: Vec::new(),
        has_more: false,
    };
    let Some(repository) = resolve_repository(&roots, &workspace).await? else {
        return Ok(result);
    };
    // Same metadata and config checks as the status summary before Git reads
    // anything from this repository.
    repository_metadata_roots(&roots, &repository).await?;
    validate_mutation_execution_config(&repository).await?;
    result.repository_path = repository.display().to_string();
    result.initialized = true;
    if !head_is_born(&repository).await? {
        return Ok(result);
    }
    let skip_arg = format!("--skip={skip}");
    // One extra record tells whether another page exists.
    let count_arg = format!("--max-count={}", limit + 1);
    let output = command(
        &repository,
        &[
            "-c",
            "log.showSignature=false",
            "log",
            "--no-color",
            "--no-decorate",
            "-z",
            "--format=%H%x00%an%x00%at%x00%s",
            &skip_arg,
            &count_arg,
            "HEAD",
            "--",
        ],
    )
    .await?;
    let mut commits = parse_log_z(&output.stdout)?;
    result.has_more = commits.len() > limit as usize;
    commits.truncate(limit as usize);
    mark_pushed(&repository, &mut commits).await?;
    result.commits = commits;
    Ok(result)
}

async fn mark_pushed(repository: &Path, commits: &mut [GitLogCommit]) -> Result<()> {
    if commits.is_empty() {
        return Ok(());
    }
    let base = push_base(repository).await?;
    let Some(exclusion) = push_base_exclusion(&base) else {
        return Ok(());
    };
    let count_arg = format!("--max-count={UNPUSHED_LIST_LIMIT}");
    let mut arguments = vec!["rev-list", count_arg.as_str(), "HEAD"];
    arguments.extend_from_slice(exclusion);
    arguments.push("--");
    let output = command(repository, &arguments).await?;
    let text = std::str::from_utf8(&output.stdout).map_err(|_| malformed("commit list"))?;
    let unpushed: HashSet<&str> = text.lines().filter(|line| !line.is_empty()).collect();
    let complete = unpushed.len() < UNPUSHED_LIST_LIMIT;
    for commit in commits {
        commit.pushed = if unpushed.contains(commit.sha.as_str()) {
            Some(false)
        } else if complete {
            Some(true)
        } else {
            None
        };
    }
    Ok(())
}

fn parse_log_z(bytes: &[u8]) -> Result<Vec<GitLogCommit>> {
    let bytes = bytes.strip_suffix(b"\0").unwrap_or(bytes);
    if bytes.is_empty() {
        return Ok(Vec::new());
    }
    // `-z` terminates each record with NUL and the format separates fields
    // with NUL too, so records are consecutive groups of four fields.
    let fields: Vec<&[u8]> = bytes.split(|byte| *byte == 0).collect();
    if !fields.len().is_multiple_of(4) {
        return Err(malformed("log record"));
    }
    fields
        .chunks_exact(4)
        .map(|record| {
            let sha = std::str::from_utf8(record[0]).map_err(|_| malformed("commit id"))?;
            if !matches!(sha.len(), 40 | 64) || !sha.bytes().all(|byte| byte.is_ascii_hexdigit()) {
                return Err(malformed("commit id"));
            }
            let authored_at = std::str::from_utf8(record[2])
                .ok()
                .and_then(|value| value.parse::<i64>().ok())
                .ok_or_else(|| malformed("commit time"))?;
            Ok(GitLogCommit {
                sha: sha.to_owned(),
                subject: bounded_text(record[3], GIT_COMMIT_MESSAGE_LIMIT),
                author_name: bounded_text(record[1], AUTHOR_NAME_LIMIT),
                authored_at,
                pushed: None,
            })
        })
        .collect()
}

fn malformed(field: &str) -> AppError {
    AppError::GitProcess(format!("Git returned an invalid {field}"))
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
            let root = std::env::temp_dir().join(format!("todex-git-log-{}", uuid::Uuid::new_v4()));
            fs::create_dir_all(root.join("repo")).unwrap();
            let root = fs::canonicalize(root).unwrap();
            let fixture = Self {
                repo: root.join("repo"),
                root,
            };
            fixture
                .git(&["init", "--template=", "--initial-branch=main"])
                .await;
            fixture.git(&["config", "user.name", "Test"]).await;
            fixture
                .git(&["config", "user.email", "test@example.invalid"])
                .await;
            fixture
        }
        async fn git(&self, args: &[&str]) {
            command(&self.repo, args).await.unwrap();
        }
        async fn commit(&self, subject: &str) {
            self.git(&[
                "-c",
                "commit.gpgSign=false",
                "-c",
                "core.hooksPath=/dev/null",
                "commit",
                "--allow-empty",
                "-m",
                subject,
            ])
            .await;
        }
        async fn log(&self, skip: u32, limit: u32) -> GitLogResponse {
            read(std::slice::from_ref(&self.root), &self.repo, skip, limit)
                .await
                .unwrap()
        }
        async fn status(&self) -> crate::server::protocol::GitStatusResponse {
            super::super::status::read(std::slice::from_ref(&self.root), &self.repo)
                .await
                .unwrap()
        }
        /// A bare repository inside the workspace root, pushed as `origin`.
        async fn add_origin(&self) {
            let remote = self.root.join("origin.git");
            command(
                &self.root,
                &["init", "--bare", "--template=", remote.to_str().unwrap()],
            )
            .await
            .unwrap();
            self.git(&["remote", "add", "origin", remote.to_str().unwrap()])
                .await;
        }
        /// Pushes through the local bare repository, then points `origin` at a
        /// network URL: reads reject local-path remotes, and the remote-tracking
        /// refs that ahead/behind compare against stay in place.
        async fn push(&self) {
            let remote = self.root.join("origin.git");
            self.git(&["remote", "set-url", "origin", remote.to_str().unwrap()])
                .await;
            self.git(&[
                "-c",
                "core.hooksPath=/dev/null",
                "push",
                "--quiet",
                "-u",
                "origin",
                "main",
            ])
            .await;
            self.git(&[
                "remote",
                "set-url",
                "origin",
                "https://example.invalid/origin.git",
            ])
            .await;
        }
        fn subjects(log: &GitLogResponse) -> Vec<&str> {
            log.commits
                .iter()
                .map(|commit| commit.subject.as_str())
                .collect()
        }
        fn pushed(log: &GitLogResponse) -> Vec<Option<bool>> {
            log.commits.iter().map(|commit| commit.pushed).collect()
        }
    }
    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.root);
        }
    }

    #[tokio::test]
    async fn log_reports_non_repositories_and_unborn_branches_as_empty() {
        let fixture = Fixture::new().await;
        let outside = read(std::slice::from_ref(&fixture.root), &fixture.root, 0, 5)
            .await
            .unwrap();
        assert!(
            !outside.initialized,
            "a child repository must not be scanned"
        );
        assert!(outside.commits.is_empty());
        let unborn = fixture.log(0, 5).await;
        assert!(unborn.initialized);
        assert!(unborn.commits.is_empty());
        assert!(!unborn.has_more);
        let status = fixture.status().await;
        assert_eq!(
            (status.upstream, status.ahead, status.behind),
            (None, None, None)
        );
    }

    #[tokio::test]
    async fn log_pages_newest_first_and_rejects_out_of_range_parameters() {
        let fixture = Fixture::new().await;
        for index in 1..=7 {
            fixture.commit(&format!("c{index}")).await;
        }
        let first = fixture.log(0, 5).await;
        assert_eq!(Fixture::subjects(&first), ["c7", "c6", "c5", "c4", "c3"]);
        assert!(first.has_more);
        let commit = &first.commits[0];
        assert!(matches!(commit.sha.len(), 40 | 64));
        assert_eq!(commit.author_name, "Test");
        assert!(commit.authored_at > 0);
        // No remotes: nothing to compare against.
        assert!(Fixture::pushed(&first).iter().all(Option::is_none));
        let second = fixture.log(5, 5).await;
        assert_eq!(Fixture::subjects(&second), ["c2", "c1"]);
        assert!(!second.has_more);
        let exact = fixture.log(2, 5).await;
        assert_eq!(exact.commits.len(), 5);
        assert!(!exact.has_more, "exactly the last page");
        let payload = serde_json::to_value(&first).unwrap();
        assert_eq!(payload["hasMore"], true);
        assert_eq!(payload["commits"][0]["authorName"], "Test");
        assert!(payload["commits"][0]["authoredAt"].is_i64());
        assert_eq!(payload["commits"][0]["pushed"], serde_json::Value::Null);
        for (skip, limit) in [
            (0, 0),
            (0, GIT_LOG_PAGE_LIMIT + 1),
            (GIT_LOG_SKIP_LIMIT + 1, 5),
        ] {
            let error = read(
                std::slice::from_ref(&fixture.root),
                &fixture.repo,
                skip,
                limit,
            )
            .await
            .unwrap_err();
            assert!(
                matches!(error, AppError::InvalidRequest(_)),
                "{skip}/{limit}"
            );
        }
    }

    #[tokio::test]
    async fn log_and_status_compare_against_the_upstream() {
        let fixture = Fixture::new().await;
        fixture.commit("base1").await;
        fixture.commit("base2").await;
        fixture.add_origin().await;
        fixture.push().await;
        let synced = fixture.status().await;
        assert_eq!(synced.upstream.as_deref(), Some("origin/main"));
        assert_eq!((synced.ahead, synced.behind), (Some(0), Some(0)));
        fixture.commit("local1").await;
        fixture.commit("local2").await;
        let ahead = fixture.status().await;
        assert_eq!((ahead.ahead, ahead.behind), (Some(2), Some(0)));
        let log = fixture.log(0, 5).await;
        assert_eq!(
            Fixture::subjects(&log),
            ["local2", "local1", "base2", "base1"]
        );
        assert_eq!(
            Fixture::pushed(&log),
            [Some(false), Some(false), Some(true), Some(true)]
        );
        // Push both, drop one locally and add another: one ahead, one behind.
        fixture.push().await;
        fixture
            .git(&[
                "-c",
                "core.hooksPath=/dev/null",
                "reset",
                "--quiet",
                "--hard",
                "HEAD~1",
            ])
            .await;
        fixture.commit("diverged").await;
        let diverged = fixture.status().await;
        assert_eq!((diverged.ahead, diverged.behind), (Some(1), Some(1)));
        assert_eq!(
            Fixture::pushed(&fixture.log(0, 2).await),
            [Some(false), Some(true)]
        );
    }

    #[tokio::test]
    async fn branch_without_upstream_counts_commits_missing_from_every_remote() {
        let fixture = Fixture::new().await;
        fixture.commit("shared").await;
        fixture.add_origin().await;
        fixture.push().await;
        fixture
            .git(&[
                "-c",
                "core.hooksPath=/dev/null",
                "switch",
                "--quiet",
                "-c",
                "feature",
            ])
            .await;
        fixture.commit("feature1").await;
        let status = fixture.status().await;
        assert_eq!(status.branch.as_deref(), Some("feature"));
        assert_eq!(status.upstream, None);
        assert_eq!((status.ahead, status.behind), (Some(1), None));
        assert_eq!(
            Fixture::pushed(&fixture.log(0, 5).await),
            [Some(false), Some(true)]
        );
        fixture
            .git(&[
                "-c",
                "core.hooksPath=/dev/null",
                "switch",
                "--quiet",
                "--detach",
            ])
            .await;
        let detached = fixture.status().await;
        assert_eq!(detached.upstream, None);
        assert_eq!(detached.ahead, Some(1));
    }

    #[test]
    fn parse_log_z_keeps_empty_fields_and_rejects_partial_records() {
        let sha = "a".repeat(40);
        let record = format!("{sha}\0\x001700000000\0\0");
        let commits = parse_log_z(record.as_bytes()).unwrap();
        assert_eq!(commits.len(), 1);
        assert_eq!(commits[0].author_name, "");
        assert_eq!(commits[0].subject, "");
        assert_eq!(commits[0].authored_at, 1_700_000_000);
        assert!(parse_log_z(format!("{sha}\0name\0").as_bytes()).is_err());
        assert!(parse_log_z(b"nothex\0n\x001\0s\0").is_err());
        assert!(parse_log_z(b"").unwrap().is_empty());
    }
}
