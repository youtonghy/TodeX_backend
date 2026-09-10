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
    let remote_head = gh(
        cwd,
        &api_args(
            &host,
            &format!("repos/{repository}/git/ref/heads/{encoded}"),
            "GET",
        ),
    )
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
    let existing = gh(cwd, &lookup).await?;
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
    let result = gh(cwd, &args)
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
