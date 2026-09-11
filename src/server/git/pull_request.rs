//! Direct PR operations over the GitHub API (`gh api`). Reasoning-heavy PR work
//! (explaining changes, fixing comments or checks) still belongs to the Agent.
use super::*;
use crate::server::protocol::{
    GitOperation, GitPullRequest, GitPullRequestChecks, GitPullRequestResponse,
    GitPullRequestReviews,
};
use serde_json::Value;
use std::collections::HashMap;

fn invalid(message: &str) -> AppError {
    AppError::InvalidRequest(message.to_owned())
}

fn repository_parts(value: &str) -> Result<(String, String)> {
    let parts: Vec<_> = value.split('/').collect();
    let (host, owner, name) = match parts.as_slice() {
        [owner, name] => ("github.com", *owner, *name),
        [host, owner, name] => (*host, *owner, *name),
        _ => return Err(invalid("repository must be [host/]owner/repo")),
    };
    if [host, owner, name].iter().any(|part| {
        part.is_empty()
            || part.starts_with(['-', '.'])
            || part.ends_with('.')
            || !part
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b"-_.".contains(&b))
    }) {
        return Err(invalid("Invalid repository identifier"));
    }
    Ok((host.to_ascii_lowercase(), format!("{owner}/{name}")))
}

fn remote_repository(value: &str) -> Result<(String, String)> {
    let value = value.trim().trim_end_matches('/').trim_end_matches(".git");
    let value = if let Some(value) = value.strip_prefix("https://") {
        value.to_owned()
    } else if let Some(value) = value.strip_prefix("ssh://git@") {
        value.to_owned()
    } else if let Some(value) = value.strip_prefix("git@") {
        value.replacen(':', "/", 1)
    } else {
        return Err(invalid(
            "PR creation requires a GitHub HTTPS or SSH upstream",
        ));
    };
    repository_parts(&value)
}

async fn git_text(cwd: &Path, args: &[&str]) -> Result<String> {
    let output = run_checked(cwd, &git_args(args), "Inspect PR branch").await?;
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_owned())
}

async fn gh(cwd: &Path, args: &[String]) -> Result<Value> {
    let mut command = Command::new("gh");
    command.env_clear();
    // Only carry credentials and runtime settings intended for GitHub CLI.
    for key in [
        "PATH",
        "HOME",
        "USERPROFILE",
        "APPDATA",
        "SYSTEMROOT",
        "TMPDIR",
        "LANG",
        "XDG_CONFIG_HOME",
        "GH_CONFIG_DIR",
        "GH_TOKEN",
        "GITHUB_TOKEN",
        "GH_ENTERPRISE_TOKEN",
        "GITHUB_ENTERPRISE_TOKEN",
        "HTTPS_PROXY",
        "HTTP_PROXY",
        "NO_PROXY",
        "SSL_CERT_FILE",
        "SSL_CERT_DIR",
    ] {
        if let Some(value) = std::env::var_os(key) {
            command.env(key, value);
        }
    }
    command
        .args(args)
        .current_dir(cwd)
        .env("GH_PROMPT_DISABLED", "1")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        command.as_std_mut().process_group(0);
    }
    let result = run_external_command(command, "GitHub PR request").await?;
    if !result.status.success() {
        return Err(AppError::GitProcess(format!(
            "GitHub CLI failed: {}",
            bounded_text(&result.stderr, GIT_ERROR_DETAIL_LIMIT)
        )));
    }
    serde_json::from_slice(&result.stdout).map_err(|_| invalid("GitHub returned invalid PR data"))
}

fn api_args(host: &str, endpoint: &str, method: &str) -> Vec<String> {
    git_args(&["api", "--hostname", host, "--method", method, endpoint])
}

fn pr_url(value: &Value, host: &str, repository: &str) -> Result<String> {
    let url = value
        .get("html_url")
        .and_then(Value::as_str)
        .unwrap_or_default();
    let prefix = format!("https://{host}/{repository}/pull/");
    if !url
        .to_ascii_lowercase()
        .starts_with(&prefix.to_ascii_lowercase())
        || !url[prefix.len()..].bytes().all(|b| b.is_ascii_digit())
        || url.len() == prefix.len()
    {
        return Err(invalid("GitHub returned an invalid PR URL"));
    }
    Ok(url.to_owned())
}

pub(super) async fn create(
    cwd: &Path,
    title: &str,
    body: &str,
    base: &str,
    draft: bool,
    repository: &str,
) -> Result<String> {
    create_with_gh(
        cwd,
        title,
        body,
        base,
        draft,
        repository,
        |args| async move { gh(cwd, &args).await },
    )
    .await
}

async fn create_with_gh<F, Fut>(
    cwd: &Path,
    title: &str,
    body: &str,
    base: &str,
    draft: bool,
    repository: &str,
    mut execute: F,
) -> Result<String>
where
    F: FnMut(Vec<String>) -> Fut,
    Fut: std::future::Future<Output = Result<Value>>,
{
    if title.trim().is_empty()
        || title.len() > 256
        || body.len() > 60_000
        || title.chars().any(char::is_control)
        || body
            .chars()
            .any(|c| c.is_control() && !matches!(c, '\n' | '\r' | '\t'))
    {
        return Err(invalid("PR title or description is invalid or too long"));
    }
    let (host, repository) = repository_parts(repository)?;
    if base.is_empty() || base.starts_with('-') || base.len() > 1024 {
        return Err(invalid("A valid base branch is required"));
    }
    git_text(cwd, &["check-ref-format", &format!("refs/heads/{base}")]).await?;
    let (remote_host, remote_repo, head) = upstream_repository(cwd).await?;
    if remote_host != host || !remote_repo.eq_ignore_ascii_case(&repository) {
        return Err(invalid("Requested repository must match the current branch upstream; ask the Agent to handle fork workflows"));
    }
    if head == base {
        return Err(invalid("PR head and base must be different branches"));
    }
    let oid = git_text(cwd, &["rev-parse", "--verify", "HEAD"]).await?;
    let encoded: String = head
        .bytes()
        .map(|b| {
            if b.is_ascii_alphanumeric() || b"-_.~".contains(&b) {
                (b as char).to_string()
            } else {
                format!("%{b:02X}")
            }
        })
        .collect();
    let remote_head = execute(api_args(
        &host,
        &format!("repos/{repository}/git/ref/heads/{encoded}"),
        "GET",
    ))
    .await?;
    if remote_head.pointer("/object/sha").and_then(Value::as_str) != Some(oid.as_str()) {
        return Err(invalid(
            "Remote branch does not match HEAD; push the current commit before creating a PR",
        ));
    }
    let owner = repository.split('/').next().unwrap();
    let mut lookup = api_args(&host, &format!("repos/{repository}/pulls"), "GET");
    lookup.extend(git_args(&[
        "--raw-field",
        "state=open",
        "--raw-field",
        &format!("head={owner}:{head}"),
        "--raw-field",
        &format!("base={base}"),
    ]));
    let existing = execute(lookup).await?;
    let existing = existing
        .as_array()
        .ok_or_else(|| invalid("GitHub returned invalid PR list"))?;
    if let Some(pr) = existing.first() {
        return pr_url(pr, &host, &repository);
    }
    let mut args = api_args(&host, &format!("repos/{repository}/pulls"), "POST");
    args.extend(git_args(&[
        "--raw-field",
        &format!("title={title}"),
        "--raw-field",
        &format!("body={body}"),
        "--raw-field",
        &format!("head={head}"),
        "--raw-field",
        &format!("base={base}"),
        "--field",
        if draft { "draft=true" } else { "draft=false" },
    ]));
    // Never retry a write: the server may have accepted it before a timeout.
    let result = execute(args)
        .await
        .and_then(|pr| pr_url(&pr, &host, &repository));
    result.map_err(|error| AppError::GitPartialSuccess {
        repository_path: cwd.display().to_string(),
        operation: "create-pr".to_owned(),
        detail: format!(
            "PR creation outcome is unknown; inspect GitHub before retrying. {}",
            truncate_text(&error.to_string(), GIT_ERROR_DETAIL_LIMIT)
        ),
    })
}

/// Resolve the current branch's pushed upstream to its GitHub repository.
/// Returns (host, owner/repo, upstream head branch name).
async fn upstream_repository(cwd: &Path) -> Result<(String, String, String)> {
    let branch = git_text(cwd, &["symbolic-ref", "--quiet", "HEAD"]).await?;
    if branch.is_empty() {
        return Err(invalid("Current checkout is not on a branch"));
    }
    let upstream = git_text(
        cwd,
        &[
            "for-each-ref",
            "--format=%(upstream:remotename)%00%(upstream:remoteref)",
            &branch,
        ],
    )
    .await?;
    let (remote, reference) = upstream
        .split_once('\0')
        .ok_or_else(|| invalid("Current branch needs a pushed upstream"))?;
    if remote.is_empty() || remote.starts_with('-') {
        return Err(invalid("Current branch needs a pushed upstream"));
    }
    let head = reference
        .strip_prefix("refs/heads/")
        .filter(|s| !s.is_empty())
        .ok_or_else(|| invalid("Upstream must be a branch"))?;
    let url = git_text(cwd, &["remote", "get-url", "--", remote]).await?;
    let (host, repository) = remote_repository(&url)?;
    Ok((host, repository, head.to_owned()))
}

struct PrLookup {
    host: String,
    repository: String,
    number: u64,
}

/// Locate the PR for the current branch's upstream head. Prefers an open PR,
/// otherwise the most recently created one. `None` means none exists.
async fn find<F, Fut>(cwd: &Path, execute: &mut F) -> Result<Option<PrLookup>>
where
    F: FnMut(Vec<String>) -> Fut,
    Fut: std::future::Future<Output = Result<Value>>,
{
    let (host, repository, head) = upstream_repository(cwd).await?;
    let owner = repository.split('/').next().unwrap_or_default();
    let mut args = api_args(&host, &format!("repos/{repository}/pulls"), "GET");
    args.extend(git_args(&[
        "--raw-field",
        "state=all",
        "--raw-field",
        &format!("head={owner}:{head}"),
        "--raw-field",
        "per_page=30",
    ]));
    let pulls = execute(args).await?;
    let pulls = pulls
        .as_array()
        .ok_or_else(|| invalid("GitHub returned invalid PR list"))?;
    let Some(pr) = pulls
        .iter()
        .find(|pr| pr.get("state").and_then(Value::as_str) == Some("open"))
        .or_else(|| pulls.first())
    else {
        return Ok(None);
    };
    let number = pr
        .get("number")
        .and_then(Value::as_u64)
        .ok_or_else(|| invalid("GitHub returned invalid PR data"))?;
    Ok(Some(PrLookup {
        host,
        repository,
        number,
    }))
}

async fn fetch_pr<F, Fut>(execute: &mut F, lookup: &PrLookup) -> Result<Value>
where
    F: FnMut(Vec<String>) -> Fut,
    Fut: std::future::Future<Output = Result<Value>>,
{
    execute(api_args(
        &lookup.host,
        &format!("repos/{}/pulls/{}", lookup.repository, lookup.number),
        "GET",
    ))
    .await
}

fn text_field<'a>(value: &'a Value, pointer: &str) -> &'a str {
    value.pointer(pointer).and_then(Value::as_str).unwrap_or("")
}

async fn detail<F, Fut>(execute: &mut F, lookup: &PrLookup) -> Result<GitPullRequest>
where
    F: FnMut(Vec<String>) -> Fut,
    Fut: std::future::Future<Output = Result<Value>>,
{
    let pr = fetch_pr(execute, lookup).await?;
    let url = pr_url(&pr, &lookup.host, &lookup.repository)?;
    let merged = pr.get("merged").and_then(Value::as_bool).unwrap_or(false);
    let head_sha = text_field(&pr, "/head/sha").to_owned();
    let mut review_args = api_args(
        &lookup.host,
        &format!(
            "repos/{}/pulls/{}/reviews",
            lookup.repository, lookup.number
        ),
        "GET",
    );
    review_args.extend(git_args(&["--raw-field", "per_page=100", "--paginate"]));
    let reviews = execute(review_args).await?;
    let mut checks = GitPullRequestChecks {
        passing: 0,
        failing: 0,
        pending: 0,
    };
    if !head_sha.is_empty() {
        let mut run_args = api_args(
            &lookup.host,
            &format!("repos/{}/commits/{head_sha}/check-runs", lookup.repository),
            "GET",
        );
        run_args.extend(git_args(&["--raw-field", "per_page=100", "--paginate"]));
        let runs = execute(run_args).await?;
        if let Some(list) = runs.get("check_runs").and_then(Value::as_array) {
            for run in list {
                if run.get("status").and_then(Value::as_str) != Some("completed") {
                    checks.pending += 1;
                    continue;
                }
                match run.get("conclusion").and_then(Value::as_str) {
                    Some("success") | Some("neutral") | Some("skipped") | Some("stale") => {
                        checks.passing += 1
                    }
                    Some(_) => checks.failing += 1,
                    None => {}
                }
            }
        }
        let status = execute(api_args(
            &lookup.host,
            &format!("repos/{}/commits/{head_sha}/status", lookup.repository),
            "GET",
        ))
        .await?;
        if let Some(list) = status.get("statuses").and_then(Value::as_array) {
            for entry in list {
                match entry.get("state").and_then(Value::as_str) {
                    Some("success") => checks.passing += 1,
                    Some("pending") => checks.pending += 1,
                    Some(_) => checks.failing += 1,
                    None => {}
                }
            }
        }
    }
    // Only each reviewer's latest submitted review counts toward the summary.
    let mut latest: HashMap<&str, &str> = HashMap::new();
    if let Some(list) = reviews.as_array() {
        for review in list {
            let state = review.get("state").and_then(Value::as_str).unwrap_or("");
            let user = text_field(review, "/user/login");
            if user.is_empty() || state == "PENDING" {
                continue;
            }
            latest.insert(user, state);
        }
    }
    let mut review_counts = GitPullRequestReviews {
        approved: 0,
        changes_requested: 0,
        commented: 0,
    };
    for state in latest.values() {
        match *state {
            "APPROVED" => review_counts.approved += 1,
            "CHANGES_REQUESTED" => review_counts.changes_requested += 1,
            "COMMENTED" => review_counts.commented += 1,
            _ => {}
        }
    }
    Ok(GitPullRequest {
        number: lookup.number,
        title: truncate_text(text_field(&pr, "/title"), 512),
        url,
        state: if merged {
            "merged".to_owned()
        } else {
            text_field(&pr, "/state").to_owned()
        },
        draft: pr.get("draft").and_then(Value::as_bool).unwrap_or(false),
        head_ref: text_field(&pr, "/head/ref").to_owned(),
        base_ref: text_field(&pr, "/base/ref").to_owned(),
        head_sha,
        mergeable: match pr.get("mergeable").and_then(Value::as_bool) {
            Some(true) => "mergeable".to_owned(),
            Some(false) => "unmergeable".to_owned(),
            None => "unknown".to_owned(),
        },
        merge_state: text_field(&pr, "/mergeable_state").to_owned(),
        auto_merge_method: pr
            .pointer("/auto_merge/merge_method")
            .and_then(Value::as_str)
            .map(str::to_owned),
        reviews: review_counts,
        checks,
    })
}

pub(crate) async fn summary(root: &Path, workspace: &Path) -> Result<GitPullRequestResponse> {
    let _permit = timeout(GIT_SCAN_QUEUE_TIMEOUT, GIT_SCAN_SEMAPHORE.acquire())
        .await
        .map_err(|_| AppError::Conflict("Git read capacity is busy".to_owned()))?
        .map_err(|_| AppError::Conflict("Git read capacity is closed".to_owned()))?;
    timeout(GIT_SCAN_TIMEOUT, summary_inner(root, workspace))
        .await
        .map_err(|_| AppError::GitCommandTimedOut("Git pull request inspection".to_owned()))?
}

async fn summary_inner(root: &Path, workspace: &Path) -> Result<GitPullRequestResponse> {
    let root = canonical_workspace_root(root)?;
    let workspace = validate_workspace_directory(&root, workspace)?;
    let mut result = GitPullRequestResponse {
        repository_path: workspace.display().to_string(),
        initialized: false,
        branch: String::new(),
        pull_request: None,
    };
    if !workspace::is_exact_repository(&root, &workspace).await? {
        return Ok(result);
    }
    validate_repository_metadata(&root, &workspace).await?;
    result.initialized = true;
    result.branch = git_text(&workspace, &["branch", "--show-current"])
        .await?
        .trim()
        .to_owned();
    if result.branch.is_empty() {
        return Ok(result);
    }
    let workspace_path: &Path = &workspace;
    result.pull_request = summarize(workspace_path, |args| async move {
        gh(workspace_path, &args).await
    })
    .await?;
    Ok(result)
}

async fn summarize<F, Fut>(cwd: &Path, mut execute: F) -> Result<Option<GitPullRequest>>
where
    F: FnMut(Vec<String>) -> Fut,
    Fut: std::future::Future<Output = Result<Value>>,
{
    let Some(lookup) = find(cwd, &mut execute).await? else {
        return Ok(None);
    };
    detail(&mut execute, &lookup).await.map(Some)
}

fn graphql(host: &str, query: &str, variables: &[(&str, String)]) -> Vec<String> {
    let mut args = git_args(&[
        "api",
        "--hostname",
        host,
        "graphql",
        "--raw-field",
        &format!("query={query}"),
    ]);
    for (key, value) in variables {
        args.extend(git_args(&["--raw-field", &format!("{key}={value}")]));
    }
    args
}

fn check_graphql(value: &Value) -> Result<()> {
    if let Some(errors) = value.get("errors").and_then(Value::as_array) {
        if let Some(error) = errors.first() {
            let message = error
                .get("message")
                .and_then(Value::as_str)
                .unwrap_or("GitHub GraphQL request failed");
            return Err(AppError::GitProcess(
                truncate_text(message, GIT_ERROR_DETAIL_LIMIT).to_owned(),
            ));
        }
    }
    Ok(())
}

fn merge_method(value: &str) -> Result<()> {
    match value {
        "merge" | "squash" | "rebase" => Ok(()),
        _ => Err(invalid("merge method must be merge, squash, or rebase")),
    }
}

fn mutation_failed(cwd: &Path, operation: &GitOperation, error: AppError) -> AppError {
    AppError::GitPartialSuccess {
        repository_path: cwd.display().to_string(),
        operation: operation.action().to_owned(),
        detail: format!(
            "PR update outcome is unknown; inspect GitHub before retrying. {}",
            truncate_text(&error.to_string(), GIT_ERROR_DETAIL_LIMIT)
        ),
    }
}

pub(super) async fn mutate(cwd: &Path, operation: &GitOperation) -> Result<String> {
    mutate_with_gh(cwd, operation, |args| async move { gh(cwd, &args).await }).await
}

async fn mutate_with_gh<F, Fut>(
    cwd: &Path,
    operation: &GitOperation,
    mut execute: F,
) -> Result<String>
where
    F: FnMut(Vec<String>) -> Fut,
    Fut: std::future::Future<Output = Result<Value>>,
{
    let lookup = find(cwd, &mut execute)
        .await?
        .ok_or_else(|| invalid("No pull request exists for the current branch"))?;
    let pr = fetch_pr(&mut execute, &lookup).await?;
    let url = pr_url(&pr, &lookup.host, &lookup.repository)?;
    let number = lookup.number;
    let merged = pr.get("merged").and_then(Value::as_bool).unwrap_or(false);
    let open = pr.get("state").and_then(Value::as_str) == Some("open") && !merged;
    let draft = pr.get("draft").and_then(Value::as_bool).unwrap_or(false);
    let auto_merge = pr.get("auto_merge").is_some_and(|value| !value.is_null());
    let node_id = pr
        .get("node_id")
        .and_then(Value::as_str)
        .filter(|id| !id.is_empty())
        .ok_or_else(|| invalid("GitHub returned invalid PR data"))?
        .to_owned();
    let endpoint = format!("repos/{}/pulls/{number}", lookup.repository);
    // Each write re-checks the freshly fetched PR state before issuing its call.
    let (args, verify, done) = match operation {
        GitOperation::ClosePr {} => {
            if !open {
                return Err(invalid("PR is not open"));
            }
            let mut args = api_args(&lookup.host, &endpoint, "PATCH");
            args.extend(git_args(&["--raw-field", "state=closed"]));
            (args, Some("closed"), format!("Closed PR #{number}\n{url}"))
        }
        GitOperation::ReopenPr {} => {
            if merged {
                return Err(invalid("A merged PR cannot be reopened"));
            }
            if open {
                return Err(invalid("PR is already open"));
            }
            let mut args = api_args(&lookup.host, &endpoint, "PATCH");
            args.extend(git_args(&["--raw-field", "state=open"]));
            (args, Some("open"), format!("Reopened PR #{number}\n{url}"))
        }
        GitOperation::DraftPr {} => {
            if !open {
                return Err(invalid("PR is not open"));
            }
            if draft {
                return Err(invalid("PR is already a draft"));
            }
            (
                graphql(
                    &lookup.host,
                    "mutation($id:ID!){convertPullRequestToDraft(input:{pullRequestId:$id}){pullRequest{number}}}",
                    &[("id", node_id)],
                ),
                None,
                format!("Converted PR #{number} to draft\n{url}"),
            )
        }
        GitOperation::ReadyPr {} => {
            if !open {
                return Err(invalid("PR is not open"));
            }
            if !draft {
                return Err(invalid("PR is not a draft"));
            }
            (
                graphql(
                    &lookup.host,
                    "mutation($id:ID!){markPullRequestReadyForReview(input:{pullRequestId:$id}){pullRequest{number}}}",
                    &[("id", node_id)],
                ),
                None,
                format!("Marked PR #{number} ready for review\n{url}"),
            )
        }
        GitOperation::MergePr { method, head_sha } => {
            if !open {
                return Err(invalid("PR is not open"));
            }
            merge_method(method)?;
            let head = text_field(&pr, "/head/sha");
            if !head.eq_ignore_ascii_case(head_sha) {
                return Err(invalid("PR head changed; refresh before merging"));
            }
            let mut args = api_args(&lookup.host, &format!("{endpoint}/merge"), "PUT");
            args.extend(git_args(&[
                "--raw-field",
                &format!("merge_method={method}"),
                "--raw-field",
                &format!("sha={head}"),
            ]));
            (args, Some("merged"), format!("Merged PR #{number}\n{url}"))
        }
        GitOperation::EnablePrAutoMerge { method } => {
            if !open {
                return Err(invalid("PR is not open"));
            }
            if auto_merge {
                return Err(invalid("Auto-merge is already enabled"));
            }
            merge_method(method)?;
            (
                graphql(
                    &lookup.host,
                    "mutation($id:ID!,$method:PullRequestMergeMethod!){enablePullRequestAutoMerge(input:{pullRequestId:$id,mergeMethod:$method}){pullRequest{number}}}",
                    &[("id", node_id), ("method", method.to_uppercase())],
                ),
                None,
                format!("Enabled {method} auto-merge for PR #{number}\n{url}"),
            )
        }
        GitOperation::DisablePrAutoMerge {} => {
            if !auto_merge {
                return Err(invalid("Auto-merge is not enabled on this PR"));
            }
            (
                graphql(
                    &lookup.host,
                    "mutation($id:ID!){disablePullRequestAutoMerge(input:{pullRequestId:$id}){pullRequest{number}}}",
                    &[("id", node_id)],
                ),
                None,
                format!("Disabled auto-merge for PR #{number}\n{url}"),
            )
        }
        _ => return Err(invalid("Unsupported pull request operation")),
    };
    // Never retry a write: the server may have accepted it before a timeout.
    let result = execute(args)
        .await
        .map_err(|error| mutation_failed(cwd, operation, error))?;
    // A GraphQL top-level error means the mutation was rejected, not applied.
    check_graphql(&result)?;
    let confirmed = match verify {
        Some("merged") => result.get("merged").and_then(Value::as_bool) == Some(true),
        Some(state) => result.get("state").and_then(Value::as_str) == Some(state),
        None => true,
    };
    if !confirmed {
        return Err(mutation_failed(
            cwd,
            operation,
            AppError::GitProcess("PR update response did not confirm the new state".to_owned()),
        ));
    }
    Ok(done)
}

#[cfg(test)]
mod tests {
    use super::*;
    struct Fixture(PathBuf);
    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }
    async fn fixture() -> (Fixture, String) {
        let path = std::env::temp_dir().join(format!("todex-pr-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&path).unwrap();
        git_text(&path, &["init", "--initial-branch=feature", "--template="])
            .await
            .unwrap();
        git_text(
            &path,
            &[
                "-c",
                "user.name=Test",
                "-c",
                "user.email=test@example.invalid",
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
        let oid = git_text(&path, &["rev-parse", "HEAD"]).await.unwrap();
        git_text(
            &path,
            &[
                "remote",
                "add",
                "origin",
                "https://github.com/owner/repo.git",
            ],
        )
        .await
        .unwrap();
        git_text(&path, &["update-ref", "refs/remotes/origin/feature", &oid])
            .await
            .unwrap();
        git_text(
            &path,
            &["branch", "--set-upstream-to=origin/feature", "feature"],
        )
        .await
        .unwrap();
        (Fixture(path), oid)
    }

    async fn mock_create(
        path: &Path,
        replies: Vec<Result<Value>>,
    ) -> (Result<String>, Vec<Vec<String>>) {
        let mut replies = VecDeque::from(replies);
        let mut calls = Vec::new();
        let result = create_with_gh(
            path,
            "Title $(literal)",
            "First line\nSecond `literal` line",
            "main",
            true,
            "owner/repo",
            |args| {
                calls.push(args);
                std::future::ready(replies.pop_front().expect("Unexpected GitHub request"))
            },
        )
        .await;
        assert!(
            replies.is_empty(),
            "Not all expected GitHub requests occurred"
        );
        (result, calls)
    }

    #[tokio::test]
    async fn creates_pr_with_literal_multiline_body_and_draft() {
        let (fixture, oid) = fixture().await;
        let (result, calls) = mock_create(
            &fixture.0,
            vec![
                Ok(serde_json::json!({"object":{"sha":oid}})),
                Ok(serde_json::json!([])),
                Ok(serde_json::json!({"html_url":"https://github.com/owner/repo/pull/42"})),
            ],
        )
        .await;
        assert_eq!(result.unwrap(), "https://github.com/owner/repo/pull/42");
        assert_eq!(calls.len(), 3);
        assert!(calls[2].windows(2).any(|pair| pair == ["--method", "POST"]));
        assert!(calls[2]
            .windows(2)
            .any(|pair| pair == ["--raw-field", "body=First line\nSecond `literal` line"]));
        assert!(calls[2]
            .windows(2)
            .any(|pair| pair == ["--raw-field", "title=Title $(literal)"]));
        assert!(calls[2]
            .windows(2)
            .any(|pair| pair == ["--field", "draft=true"]));
        assert!(calls[2].contains(&"head=feature".to_owned()));
        assert!(calls[2].contains(&"base=main".to_owned()));
    }

    #[tokio::test]
    async fn existing_pr_returns_url_without_post() {
        let (fixture, oid) = fixture().await;
        let (result, calls) = mock_create(
            &fixture.0,
            vec![
                Ok(serde_json::json!({"object":{"sha":oid}})),
                Ok(serde_json::json!([{"html_url":"https://github.com/owner/repo/pull/42"}])),
            ],
        )
        .await;
        assert_eq!(result.unwrap(), "https://github.com/owner/repo/pull/42");
        assert_eq!(calls.len(), 2);
        assert!(calls.iter().all(|args| !args.contains(&"POST".to_owned())));
    }

    #[tokio::test]
    async fn remote_sha_mismatch_prevents_post() {
        let (fixture, _) = fixture().await;
        let (result, calls) = mock_create(
            &fixture.0,
            vec![Ok(serde_json::json!({"object":{"sha":"different"}}))],
        )
        .await;
        assert!(result
            .unwrap_err()
            .to_string()
            .contains("does not match HEAD"));
        assert_eq!(calls.len(), 1);
        assert!(!calls[0].contains(&"POST".to_owned()));
    }

    #[tokio::test]
    async fn write_timeout_is_unknown_and_never_retried() {
        let (fixture, oid) = fixture().await;
        let (result, calls) = mock_create(
            &fixture.0,
            vec![
                Ok(serde_json::json!({"object":{"sha":oid}})),
                Ok(serde_json::json!([])),
                Err(AppError::GitCommandTimedOut("GitHub PR request".to_owned())),
            ],
        )
        .await;
        match result.unwrap_err() {
            AppError::GitPartialSuccess { detail, .. } => {
                assert!(detail.contains("outcome is unknown"))
            }
            other => panic!("Expected unknown write outcome, got {other}"),
        }
        assert_eq!(calls.len(), 3);
    }

    #[test]
    fn validates_repository_and_remote_identity() {
        assert_eq!(
            repository_parts("owner/repo").unwrap(),
            remote_repository("git@github.com:owner/repo.git").unwrap()
        );
        assert_eq!(
            repository_parts("enterprise.example/owner/repo").unwrap(),
            remote_repository("https://enterprise.example/owner/repo.git").unwrap()
        );
        for bad in [
            "--repo",
            "https://github.com/a/b",
            "a/../b",
            "host/a/b?x",
            "a/b\n",
        ] {
            assert!(repository_parts(bad).is_err());
        }
        assert!(remote_repository("https://user:secret@github.com/a/b").is_err());
    }
    #[test]
    fn rejects_unexpected_urls() {
        assert!(pr_url(
            &serde_json::json!({"html_url":"https://github.com/a/b/pull/12"}),
            "github.com",
            "a/b"
        )
        .is_ok());
        for url in [
            "javascript:alert(1)",
            "https://github.com/a/b/pull/",
            "https://evil.com/a/b/pull/1",
            "https://github.com/a/b/pull/1/evil",
        ] {
            assert!(pr_url(&serde_json::json!({"html_url":url}), "github.com", "a/b").is_err());
        }
    }

    fn pr_json(state: &str, merged: bool, draft: bool) -> Value {
        serde_json::json!({
            "number": 42,
            "state": state,
            "merged": merged,
            "draft": draft,
            "node_id": "PR_node",
            "head": {"sha": "abc123", "ref": "feature"},
            "base": {"ref": "main"},
            "html_url": "https://github.com/owner/repo/pull/42",
        })
    }

    async fn mock_mutate(
        path: &Path,
        operation: GitOperation,
        replies: Vec<Result<Value>>,
    ) -> (Result<String>, Vec<Vec<String>>) {
        let mut replies = VecDeque::from(replies);
        let mut calls = Vec::new();
        let result = mutate_with_gh(path, &operation, |args| {
            calls.push(args);
            std::future::ready(replies.pop_front().expect("Unexpected GitHub request"))
        })
        .await;
        assert!(
            replies.is_empty(),
            "Not all expected GitHub requests occurred"
        );
        (result, calls)
    }

    #[tokio::test]
    async fn close_pr_patches_state_after_preflight() {
        let (fixture, _) = fixture().await;
        let (result, calls) = mock_mutate(
            &fixture.0,
            GitOperation::ClosePr {},
            vec![
                Ok(serde_json::json!([{"number": 42, "state": "open"}])),
                Ok(pr_json("open", false, false)),
                Ok(serde_json::json!({"state": "closed"})),
            ],
        )
        .await;
        assert_eq!(
            result.unwrap(),
            "Closed PR #42\nhttps://github.com/owner/repo/pull/42"
        );
        assert_eq!(calls.len(), 3);
        assert!(calls[2]
            .windows(2)
            .any(|pair| pair == ["--method", "PATCH"]));
        assert!(calls[2].contains(&"state=closed".to_owned()));
    }

    #[tokio::test]
    async fn reopen_pr_rejects_merged_without_write() {
        let (fixture, _) = fixture().await;
        let (result, calls) = mock_mutate(
            &fixture.0,
            GitOperation::ReopenPr {},
            vec![
                Ok(serde_json::json!([{"number": 42, "state": "closed"}])),
                Ok(pr_json("closed", true, false)),
            ],
        )
        .await;
        assert!(result.unwrap_err().to_string().contains("reopened"));
        assert_eq!(calls.len(), 2);
        assert!(calls.iter().all(|args| !args.contains(&"PATCH".to_owned())));
    }

    #[tokio::test]
    async fn ready_pr_uses_graphql_mutation() {
        let (fixture, _) = fixture().await;
        let (result, calls) = mock_mutate(
            &fixture.0,
            GitOperation::ReadyPr {},
            vec![
                Ok(serde_json::json!([{"number": 42, "state": "open"}])),
                Ok(pr_json("open", false, true)),
                Ok(serde_json::json!({"data":{"markPullRequestReadyForReview":{"pullRequest":{"number":42}}}})),
            ],
        )
        .await;
        assert!(result.unwrap().contains("ready for review"));
        assert_eq!(calls.len(), 3);
        assert!(calls[2].contains(&"graphql".to_owned()));
        assert!(calls[2]
            .iter()
            .any(|arg| arg.contains("markPullRequestReadyForReview")));
        assert!(calls[2].contains(&"id=PR_node".to_owned()));
    }

    #[tokio::test]
    async fn merge_pr_binds_expected_head_sha() {
        let (fixture, _) = fixture().await;
        let (result, calls) = mock_mutate(
            &fixture.0,
            GitOperation::MergePr {
                method: "squash".to_owned(),
                head_sha: "different".to_owned(),
            },
            vec![
                Ok(serde_json::json!([{"number": 42, "state": "open"}])),
                Ok(pr_json("open", false, false)),
            ],
        )
        .await;
        assert!(result.unwrap_err().to_string().contains("head changed"));
        assert_eq!(calls.len(), 2);
    }

    #[tokio::test]
    async fn merge_pr_puts_method_and_sha() {
        let (fixture, _) = fixture().await;
        let (result, calls) = mock_mutate(
            &fixture.0,
            GitOperation::MergePr {
                method: "squash".to_owned(),
                head_sha: "ABC123".to_owned(),
            },
            vec![
                Ok(serde_json::json!([{"number": 42, "state": "open"}])),
                Ok(pr_json("open", false, false)),
                Ok(serde_json::json!({"merged": true, "sha": "def456"})),
            ],
        )
        .await;
        assert!(result.unwrap().contains("Merged PR #42"));
        assert!(calls[2].windows(2).any(|pair| pair == ["--method", "PUT"]));
        assert!(calls[2].contains(&"merge_method=squash".to_owned()));
        assert!(calls[2].contains(&"sha=abc123".to_owned()));
    }

    #[tokio::test]
    async fn mutation_failure_is_unknown_and_never_retried() {
        let (fixture, _) = fixture().await;
        let (result, calls) = mock_mutate(
            &fixture.0,
            GitOperation::ClosePr {},
            vec![
                Ok(serde_json::json!([{"number": 42, "state": "open"}])),
                Ok(pr_json("open", false, false)),
                Err(AppError::GitCommandTimedOut("GitHub PR request".to_owned())),
            ],
        )
        .await;
        match result.unwrap_err() {
            AppError::GitPartialSuccess { detail, .. } => {
                assert!(detail.contains("outcome is unknown"))
            }
            other => panic!("Expected unknown write outcome, got {other}"),
        }
        assert_eq!(calls.len(), 3);
    }

    #[tokio::test]
    async fn graphql_errors_fail_without_marking_unknown() {
        let (fixture, _) = fixture().await;
        let (result, _) = mock_mutate(
            &fixture.0,
            GitOperation::EnablePrAutoMerge {
                method: "merge".to_owned(),
            },
            vec![
                Ok(serde_json::json!([{"number": 42, "state": "open"}])),
                Ok(pr_json("open", false, false)),
                Ok(serde_json::json!({"errors":[{"message":"Auto-merge is not allowed"}]})),
            ],
        )
        .await;
        match result.unwrap_err() {
            AppError::GitProcess(detail) => assert!(detail.contains("Auto-merge is not allowed")),
            other => panic!("Expected definite failure, got {other}"),
        }
    }
}
