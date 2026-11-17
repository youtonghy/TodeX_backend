//! What earlier TodeX versions left in the Antigravity global config
//! (`~/.gemini/config`, shared by the CLI, the desktop app and the IDE),
//! and its removal.
//!
//! Before TodeX drove `agy acp` it drove agy's print mode, which has no
//! per-launch MCP servers or approvals, through static entries:
//!
//! - `mcp_config.json`: `todex_ssh` / `todex_desktop` / `todex_agents`
//!   running `todex-agentd agent-mcp-bridge --route <route>`; outside a
//!   TodeX turn they serve no tools.
//! - `hooks.json`: `todex-approval`, a `PreToolUse` hook running a script
//!   next to it that answers nothing outside a TodeX turn.
//!
//! Only those keys and the script are removed; everything else in those
//! files is kept (JSONC comments are not).

use std::path::{Path, PathBuf};

use serde_json::{json, Value};

use crate::agent_mcp::ALL_SERVERS;
use crate::agent_providers::files::{modify_json5_file, read_json5, remove_file_if_exists};
use crate::error::AppError;

const HOOK_NAME: &str = "todex-approval";
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

/// Removes TodeX's entries from `dir`. Returns whether anything was there.
/// Blocking.
pub(crate) fn remove(dir: &Path) -> Result<bool, AppError> {
    let hooks = dir.join("hooks.json");
    let mut removed = remove_key(&hooks, "Antigravity hooks", &[HOOK_NAME])?;
    // TodeX's hook was the only one: no file was there before it either.
    if removed && read_json5(&hooks, "Antigravity hooks")? == Some(json!({})) {
        remove_file_if_exists(&hooks)?;
    }
    for (name, _, _) in ALL_SERVERS {
        removed |= remove_key(
            &dir.join("mcp_config.json"),
            "Antigravity MCP config",
            &["mcpServers", name],
        )?;
    }
    removed |= remove_file_if_exists(&dir.join(HOOK_SCRIPT))?;
    Ok(removed)
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent_mcp::{AGENTS_SERVER, SSH_SERVER};

    fn temp_dir() -> PathBuf {
        let dir = std::env::temp_dir().join(format!("todex-agy-config-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn read(path: PathBuf) -> Value {
        serde_json::from_slice(&std::fs::read(path).unwrap()).unwrap()
    }

    #[test]
    fn remove_takes_todex_entries_and_keeps_the_users() {
        let dir = temp_dir();
        std::fs::write(
            dir.join("mcp_config.json"),
            json!({ "mcpServers": {
                "mine": { "command": "x" },
                SSH_SERVER: { "command": "todex-agentd", "args": ["agent-mcp-bridge", "--route", "/r"] },
                AGENTS_SERVER: { "command": "todex-agentd" },
            }, "other": 1 })
            .to_string(),
        )
        .unwrap();
        std::fs::write(
            dir.join("hooks.json"),
            json!({ "lint": { "PostToolUse": [] }, HOOK_NAME: { "PreToolUse": [] } }).to_string(),
        )
        .unwrap();
        std::fs::write(dir.join(HOOK_SCRIPT), "#!/bin/sh\n").unwrap();

        assert!(remove(&dir).unwrap());
        assert_eq!(
            read(dir.join("mcp_config.json")),
            json!({ "mcpServers": { "mine": { "command": "x" } }, "other": 1 })
        );
        assert_eq!(
            read(dir.join("hooks.json")),
            json!({ "lint": { "PostToolUse": [] } })
        );
        assert!(!dir.join(HOOK_SCRIPT).exists());
        assert!(!remove(&dir).unwrap());
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn removing_the_only_hook_removes_the_hooks_file() {
        let dir = temp_dir();
        std::fs::write(
            dir.join("hooks.json"),
            json!({ HOOK_NAME: { "PreToolUse": [] } }).to_string(),
        )
        .unwrap();
        assert!(remove(&dir).unwrap());
        assert!(!dir.join("hooks.json").exists());
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn nothing_to_remove_touches_nothing() {
        let dir = temp_dir();
        assert!(!remove(&dir).unwrap());
        assert!(!dir.join("hooks.json").exists());
        assert!(!dir.join("mcp_config.json").exists());
        let _ = std::fs::remove_dir_all(dir);
    }
}
