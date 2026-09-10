//! Direct PR creation only; follow-up PR work belongs to the conversation Agent.
use super::*;
use serde_json::Value;

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
    let branch = git_text(cwd, &["symbolic-ref", "--quiet", "HEAD"]).await?;
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
    let remote_repo = remote_repository(&url)?;
    if remote_repo.0 != host || !remote_repo.1.eq_ignore_ascii_case(&repository) {
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
}
