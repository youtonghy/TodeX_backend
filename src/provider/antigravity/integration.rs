//! TodeX's entries in the Antigravity global config (`~/.gemini/config`,
//! shared by the CLI, the desktop app and the IDE).
//!
//! `agy` has no per-launch flag for MCP servers or hooks, so TodeX keeps
//! static entries there and points them at a conversation through the
//! environment of the `agy` process it starts (see `agent_mcp`):
//!
//! - `mcp_config.json`: `todex_ssh` / `todex_desktop` run
//!   `todex-agentd agent-mcp-bridge --route <route>`; outside a TodeX turn
//!   they serve no tools.
//! - `hooks.json`: `todex-approval`, a `PreToolUse` hook for every tool,
//!   runs a small script next to it. Outside a TodeX turn the script allows
//!   without starting the daemon, which leaves agy's own permissions in
//!   charge; inside one it runs `todex-agentd agy-hook`.
//!
//! Only TodeX's own keys are written; everything else in those files is
//! kept (JSONC comments are not). Files are rewritten only when an entry
//! differs.

use std::path::{Path, PathBuf};

use serde_json::{json, Value};

use crate::agent_mcp::{
    AGY_HOOK_SUBCOMMAND, BRIDGE_SUBCOMMAND, DESKTOP_ROUTE, DESKTOP_SERVER, ENDPOINT_ENV, ROUTE_ARG,
    SSH_ROUTE, SSH_SERVER,
};
use crate::agent_providers::files::{
    atomic_write_private, modify_json5_file, read_json5, read_text, remove_file_if_exists,
};
use crate::error::AppError;

pub(crate) const HOOK_NAME: &str = "todex-approval";
/// agy kills a hook after its `timeout`; TodeX's own prompt timeout
/// decides first.
const HOOK_TIMEOUT_SECONDS: u64 = 24 * 60 * 60;
#[cfg(not(windows))]
const HOOK_SCRIPT: &str = "todex-agy-hook.sh";
#[cfg(windows)]
const HOOK_SCRIPT: &str = "todex-agy-hook.cmd";

/// `~/.gemini/config`.
pub(crate) fn config_dir() -> PathBuf {
    std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."))
        .join(".gemini")
        .join("config")
}

/// Brings TodeX's entries in `dir` up to date for `daemon`. Blocking.
pub(crate) fn ensure(dir: &Path, daemon: &Path) -> Result<(), AppError> {
    let script = dir.join(HOOK_SCRIPT);
    let content = hook_script(daemon);
    if read_text(&script, "Antigravity hook script")?.as_deref() != Some(content.as_str()) {
        atomic_write_private(&script, content.as_bytes())?;
    }
    upsert(
        &dir.join("hooks.json"),
        "Antigravity hooks",
        &[HOOK_NAME],
        hook_entry(),
    )?;
    let daemon = daemon.to_string_lossy();
    for (name, route) in [(SSH_SERVER, SSH_ROUTE), (DESKTOP_SERVER, DESKTOP_ROUTE)] {
        upsert(
            &dir.join("mcp_config.json"),
            "Antigravity MCP config",
            &["mcpServers", name],
            json!({
                "command": daemon,
                "args": [BRIDGE_SUBCOMMAND, ROUTE_ARG, route],
            }),
        )?;
    }
    Ok(())
}

/// Removes TodeX's entries from `dir`. Returns whether anything was there.
/// Blocking.
pub(crate) fn remove(dir: &Path) -> Result<bool, AppError> {
    let hooks = dir.join("hooks.json");
    let mut removed = remove_key(&hooks, "Antigravity hooks", &[HOOK_NAME])?;
    // TodeX's hook was the only one: no file was there before it either.
    if removed && read_json5(&hooks, "Antigravity hooks")? == Some(json!({})) {
        remove_file_if_exists(&hooks)?;
    }
    for name in [SSH_SERVER, DESKTOP_SERVER] {
        removed |= remove_key(
            &dir.join("mcp_config.json"),
            "Antigravity MCP config",
            &["mcpServers", name],
        )?;
    }
    removed |= remove_file_if_exists(&dir.join(HOOK_SCRIPT))?;
    Ok(removed)
}

fn hook_entry() -> Value {
    #[cfg(not(windows))]
    let command = format!("sh {HOOK_SCRIPT}");
    #[cfg(windows)]
    let command = HOOK_SCRIPT.to_owned();
    // Hooks run in the directory holding hooks.json, which also holds the
    // script, so the command needs no path quoting on any shell.
    json!({
        "PreToolUse": [{
            "matcher": "*",
            "hooks": [{ "type": "command", "command": command, "timeout": HOOK_TIMEOUT_SECONDS }],
        }],
    })
}

#[cfg(not(windows))]
fn hook_script(daemon: &Path) -> String {
    let daemon = daemon.to_string_lossy().replace('\'', r"'\''");
    format!(
        "#!/bin/sh\n\
         # Installed by TodeX (todex-agentd) for the Antigravity CLI; remove it from TodeX's CLI settings.\n\
         # Outside a TodeX turn it allows, leaving agy's own permissions in charge.\n\
         if [ -z \"${ENDPOINT_ENV}\" ]; then\n  cat >/dev/null\n  echo '{{\"decision\":\"allow\"}}'\n  exit 0\nfi\n\
         exec '{daemon}' {AGY_HOOK_SUBCOMMAND}\n"
    )
}

#[cfg(windows)]
fn hook_script(daemon: &Path) -> String {
    let daemon = daemon.to_string_lossy();
    format!(
        "@echo off\r\n\
         rem Installed by TodeX (todex-agentd) for the Antigravity CLI; remove it from TodeX's CLI settings.\r\n\
         rem Outside a TodeX turn it allows, leaving agy's own permissions in charge.\r\n\
         if not defined {ENDPOINT_ENV} (\r\n  echo {{\"decision\":\"allow\"}}\r\n  exit /b 0\r\n)\r\n\
         \"{daemon}\" {AGY_HOOK_SUBCOMMAND}\r\n"
    )
}

/// Sets `path` (a key path) in the JSON document at `file` to `value`
/// unless it already holds it.
fn upsert(file: &Path, label: &str, path: &[&str], value: Value) -> Result<(), AppError> {
    let current = read_json5(file, label)?;
    if current.as_ref().and_then(|document| lookup(document, path)) == Some(&value) {
        return Ok(());
    }
    modify_json5_file(file, label, |document| {
        let mut node = document;
        for key in &path[..path.len() - 1] {
            let object = node
                .as_object_mut()
                .ok_or_else(|| not_object(file, label))?;
            node = object
                .entry((*key).to_owned())
                .or_insert_with(|| Value::Object(Default::default()));
        }
        node.as_object_mut()
            .ok_or_else(|| not_object(file, label))?
            .insert(path[path.len() - 1].to_owned(), value);
        Ok(())
    })
}

fn remove_key(file: &Path, label: &str, path: &[&str]) -> Result<bool, AppError> {
    let present = read_json5(file, label)?
        .as_ref()
        .and_then(|document| lookup(document, path))
        .is_some();
    if present {
        modify_json5_file(file, label, |document| {
            let parent = path[..path.len() - 1]
                .iter()
                .try_fold(document, |node, key| node.get_mut(*key));
            if let Some(object) = parent.and_then(Value::as_object_mut) {
                object.remove(path[path.len() - 1]);
            }
            Ok(())
        })?;
    }
    Ok(present)
}

fn lookup<'a>(document: &'a Value, path: &[&str]) -> Option<&'a Value> {
    path.iter().try_fold(document, |node, key| node.get(*key))
}

fn not_object(file: &Path, label: &str) -> AppError {
    AppError::InvalidRequest(format!(
        "{label} is not a JSON object where TodeX keeps its entry: {}",
        file.display()
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir() -> PathBuf {
        let dir = std::env::temp_dir().join(format!("todex-agy-config-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn ensure_adds_todex_entries_and_keeps_the_users() {
        let dir = temp_dir();
        std::fs::write(
            dir.join("mcp_config.json"),
            r#"{ "mcpServers": { "mine": { "command": "x" } }, "other": 1 }"#,
        )
        .unwrap();
        std::fs::write(
            dir.join("hooks.json"),
            r#"{ "lint": { "PostToolUse": [] } }"#,
        )
        .unwrap();
        let daemon = Path::new("/opt/todex/todex-agentd");
        ensure(&dir, daemon).unwrap();

        let mcp: Value =
            serde_json::from_slice(&std::fs::read(dir.join("mcp_config.json")).unwrap()).unwrap();
        assert_eq!(mcp["other"], 1);
        assert_eq!(mcp["mcpServers"]["mine"]["command"], "x");
        assert_eq!(
            mcp["mcpServers"][SSH_SERVER]["args"],
            json!([BRIDGE_SUBCOMMAND, ROUTE_ARG, SSH_ROUTE])
        );
        assert_eq!(
            mcp["mcpServers"][DESKTOP_SERVER]["command"],
            "/opt/todex/todex-agentd"
        );
        let hooks: Value =
            serde_json::from_slice(&std::fs::read(dir.join("hooks.json")).unwrap()).unwrap();
        assert!(hooks["lint"].is_object());
        assert_eq!(hooks[HOOK_NAME]["PreToolUse"][0]["matcher"], "*");
        let script = std::fs::read_to_string(dir.join(HOOK_SCRIPT)).unwrap();
        assert!(script.contains("todex-agentd"));
        assert!(script.contains(ENDPOINT_ENV));

        // Up to date: nothing is rewritten.
        let before = std::fs::metadata(dir.join("hooks.json"))
            .unwrap()
            .modified()
            .unwrap();
        ensure(&dir, daemon).unwrap();
        assert_eq!(
            std::fs::metadata(dir.join("hooks.json"))
                .unwrap()
                .modified()
                .unwrap(),
            before
        );

        assert!(remove(&dir).unwrap());
        let mcp: Value =
            serde_json::from_slice(&std::fs::read(dir.join("mcp_config.json")).unwrap()).unwrap();
        assert!(mcp["mcpServers"].get(SSH_SERVER).is_none());
        assert_eq!(mcp["mcpServers"]["mine"]["command"], "x");
        let hooks: Value =
            serde_json::from_slice(&std::fs::read(dir.join("hooks.json")).unwrap()).unwrap();
        assert!(hooks.get(HOOK_NAME).is_none());
        assert!(hooks["lint"].is_object());
        assert!(!dir.join(HOOK_SCRIPT).exists());
        assert!(!remove(&dir).unwrap());
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn removing_the_only_hook_removes_the_hooks_file() {
        let dir = temp_dir();
        ensure(&dir, Path::new("/opt/todex/todex-agentd")).unwrap();
        assert!(remove(&dir).unwrap());
        assert!(!dir.join("hooks.json").exists());
        let _ = std::fs::remove_dir_all(dir);
    }

    #[cfg(unix)]
    #[test]
    fn the_hook_script_allows_outside_todex_without_the_daemon() {
        let dir = temp_dir();
        ensure(&dir, Path::new("/nonexistent/todex-agentd")).unwrap();
        let output = std::process::Command::new("sh")
            .arg(HOOK_SCRIPT)
            .current_dir(&dir)
            .env_remove(ENDPOINT_ENV)
            .stdin(std::process::Stdio::null())
            .output()
            .unwrap();
        assert!(output.status.success());
        let decision: Value = serde_json::from_slice(&output.stdout).unwrap();
        assert_eq!(decision["decision"], "allow");
        // Inside a turn the daemon decides; a missing daemon fails, which agy
        // reads as a denial.
        let output = std::process::Command::new("sh")
            .arg(HOOK_SCRIPT)
            .current_dir(&dir)
            .env(ENDPOINT_ENV, "http://127.0.0.1:1")
            .stdin(std::process::Stdio::null())
            .output()
            .unwrap();
        assert!(!output.status.success());
        let _ = std::fs::remove_dir_all(dir);
    }
}
