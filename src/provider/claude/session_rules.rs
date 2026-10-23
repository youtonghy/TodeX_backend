//! "Allow for session" on Claude's `can_use_tool`: a permission rule TodeX
//! builds itself and adds with `destination: "session"`, which lives only in
//! the running Claude process and never touches a settings file.
//!
//! Claude's own `permission_suggestions` are not echoed back: Bash
//! suggestions are saved to `.claude/settings.local.json` and file
//! suggestions switch the whole session to `acceptEdits`.
//!
//! Each rule is as narrow as the request it answers. Verified against
//! Claude Code 2.1.306 by repeating the same call in a second turn:
//! - Bash: an exact-command rule matches a single command, quotes and
//!   parentheses included. A compound command (`&&`, `;`, `|`), a
//!   redirection or a substitution never matches, because Claude checks
//!   each part on its own, so none is built for those.
//! - Files: `Edit(//abs/path)` covers Edit and Write of that one file.
//! - WebFetch: `domain:<host>` covers every URL on the host.
//! - MCP tools: a rule without content covers the whole tool.

use std::path::{Component, Path};

use serde_json::{json, Value};

/// The session rule for allowing this request again, or `None` when no rule
/// would both match a repeat and stay as narrow as the request.
pub(super) fn session_rule(request: &Value, cwd: &Path) -> Option<Value> {
    let tool = request.get("tool_name").and_then(Value::as_str)?;
    let input = request.get("input");
    let field = |name: &str| {
        input
            .and_then(|input| input.get(name))
            .and_then(Value::as_str)
            .filter(|value| !value.trim().is_empty())
    };
    match tool {
        "Bash" => {
            let command = field("command")?;
            simple_command(command).then(|| rule("Bash", Some(command.to_owned())))
        }
        "Edit" | "Write" | "MultiEdit" => {
            path_rule(field("file_path")?, cwd).map(|path| rule("Edit", Some(path)))
        }
        "NotebookEdit" => {
            path_rule(field("notebook_path")?, cwd).map(|path| rule("NotebookEdit", Some(path)))
        }
        "WebFetch" => {
            let url = reqwest::Url::parse(field("url")?).ok()?;
            if !matches!(url.scheme(), "http" | "https") {
                return None;
            }
            let host = url.host_str()?;
            Some(rule("WebFetch", Some(format!("domain:{host}"))))
        }
        // Parenthesised content on MCP rules is dropped as invalid.
        tool if tool.starts_with("mcp__") => Some(rule(tool, None)),
        _ => None,
    }
}

/// The `updatedPermissions` entry adding `rule` for this session only.
pub(super) fn session_permission_update(rule: Value) -> Value {
    json!({
        "type": "addRules",
        "rules": [rule],
        "behavior": "allow",
        "destination": "session",
    })
}

fn rule(tool: &str, content: Option<String>) -> Value {
    match content {
        Some(content) => json!({ "toolName": tool, "ruleContent": content }),
        None => json!({ "toolName": tool }),
    }
}

/// A single command without operators, redirections, substitutions or
/// expansions. Checked on the raw text, quoted characters included, so
/// some harmless commands are refused rather than any compound one let in.
/// `*` is refused too: in a `Bash(...)` rule it is a wildcard (and a
/// trailing `:*` a prefix match), so the rule would allow other commands.
fn simple_command(command: &str) -> bool {
    !command.chars().any(|c| {
        matches!(
            c,
            ';' | '&' | '|' | '<' | '>' | '`' | '$' | '*' | '\n' | '\r'
        )
    })
}

/// `//abs/path` for a file, relative paths resolved against Claude's
/// working directory. `None` for a path that is not plain: `.`/`..`
/// segments (Claude matches the normalised path) or glob characters
/// (the rule is a gitignore-style pattern).
fn path_rule(path: &str, cwd: &Path) -> Option<String> {
    if path
        .chars()
        .any(|c| matches!(c, '*' | '?' | '[' | ']' | '{' | '}' | '\\' | '!' | '\n'))
        || path.split('/').any(|part| matches!(part, "." | ".."))
    {
        return None;
    }
    let path = Path::new(path);
    let absolute = if path.is_absolute() {
        path.to_path_buf()
    } else {
        cwd.join(path)
    };
    if !absolute.is_absolute()
        || absolute
            .components()
            .any(|part| matches!(part, Component::ParentDir))
    {
        return None;
    }
    let text = absolute.to_str()?;
    Some(format!("//{}", text.trim_start_matches('/')))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn request(tool: &str, input: Value) -> Value {
        json!({ "subtype": "can_use_tool", "tool_name": tool, "input": input })
    }

    #[test]
    fn bash_rules_match_the_exact_single_command_only() {
        let cwd = Path::new("/work");
        assert_eq!(
            session_rule(
                &request("Bash", json!({ "command": "touch \"y (2) 'q'.txt\"" })),
                cwd
            ),
            Some(json!({ "toolName": "Bash", "ruleContent": "touch \"y (2) 'q'.txt\"" }))
        );
        for command in [
            "mkdir x && touch x/a",
            "echo hi | tee out",
            "python3 -c 'print(1)' > out.txt 2>&1",
            "ls; rm -rf x",
            "echo $(whoami)",
            "echo `id`",
            "echo $HOME",
            "sleep 5 &",
            "rm -f build/*.log",
            "ls *",
            "git status:*",
            "a\nb",
            "   ",
        ] {
            assert_eq!(
                session_rule(&request("Bash", json!({ "command": command })), cwd),
                None,
                "{command}"
            );
        }
    }

    // POSIX paths: on Windows `/tmp/...` has no drive and is not absolute,
    // and `\` paths are refused, so no file rule is built there.
    #[cfg(unix)]
    #[test]
    fn file_rules_name_one_absolute_path() {
        let cwd = Path::new("/work/repo");
        let edit = |tool: &str, key: &str, path: &str| {
            session_rule(&request(tool, json!({ key: path })), cwd)
        };
        assert_eq!(
            edit("Write", "file_path", "/tmp/out/note.txt"),
            Some(json!({ "toolName": "Edit", "ruleContent": "//tmp/out/note.txt" }))
        );
        assert_eq!(
            edit("Edit", "file_path", "sub/note (1).txt"),
            Some(json!({ "toolName": "Edit", "ruleContent": "//work/repo/sub/note (1).txt" }))
        );
        assert_eq!(
            edit("MultiEdit", "file_path", "/a/b.rs"),
            Some(json!({ "toolName": "Edit", "ruleContent": "//a/b.rs" }))
        );
        assert_eq!(
            edit("NotebookEdit", "notebook_path", "nb.ipynb"),
            Some(json!({ "toolName": "NotebookEdit", "ruleContent": "//work/repo/nb.ipynb" }))
        );
        for path in ["../x.txt", "/a/./b", "/a/*.rs", "/a/[b].rs", ""] {
            assert_eq!(edit("Write", "file_path", path), None, "{path}");
        }
        assert_eq!(edit("NotebookEdit", "file_path", "/a/nb.ipynb"), None);
    }

    #[test]
    fn web_fetch_rules_name_the_host() {
        let cwd = Path::new("/work");
        assert_eq!(
            session_rule(
                &request(
                    "WebFetch",
                    json!({ "url": "https://docs.example.com:8443/a?b" })
                ),
                cwd
            ),
            Some(json!({ "toolName": "WebFetch", "ruleContent": "domain:docs.example.com" }))
        );
        for url in ["file:///etc/passwd", "not a url"] {
            assert_eq!(
                session_rule(&request("WebFetch", json!({ "url": url })), cwd),
                None
            );
        }
    }

    #[test]
    fn mcp_tools_get_a_tool_rule_and_other_tools_none() {
        let cwd = Path::new("/work");
        assert_eq!(
            session_rule(&request("mcp__mini__ping", json!({ "text": "a" })), cwd),
            Some(json!({ "toolName": "mcp__mini__ping" }))
        );
        for tool in ["Read", "Task", "ExitPlanMode", "AskUserQuestion"] {
            assert_eq!(session_rule(&request(tool, json!({})), cwd), None, "{tool}");
        }
    }

    #[test]
    fn the_update_stays_in_the_session() {
        assert_eq!(
            session_permission_update(json!({ "toolName": "Bash", "ruleContent": "ls" })),
            json!({
                "type": "addRules",
                "rules": [{ "toolName": "Bash", "ruleContent": "ls" }],
                "behavior": "allow",
                "destination": "session",
            })
        );
    }
}
