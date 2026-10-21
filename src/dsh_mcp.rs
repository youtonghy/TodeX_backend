//! DeepSeek Harness's own MCP servers for the capability catalog.
//!
//! dsh mounts one `@deepseek-ai/dsh-mcp-client` row per server. TodeX lists
//! and edits the rows the `acp` profile patch inserts (see
//! [`crate::dsh_config`]), presenting each as the same entry JSON the
//! catalog uses for other agents: `{type, command, args, env, cwd}` for
//! stdio, `{type: "http", url, headers}` for Streamable HTTP, `enabled`, plus
//! dsh's own client options. dsh has no SSE transport. Names starting with
//! `todex_` belong to the servers TodeX itself hands dsh over ACP; dsh
//! refuses a session whose servers share a name, so users cannot take them.

use std::collections::BTreeMap;
use std::path::Path;

use serde_json::{json, Map, Value};

use crate::dsh_config::{edit_patch_at, PatchFile};
use crate::error::AppError;

pub(crate) const MCP_CLIENT: &str = "@deepseek-ai/dsh-mcp-client";
const ROW_PREFIX: &str = "todex-mcp-";
/// dsh client options passed through unchanged.
const CLIENT_OPTIONS: [&str; 4] = [
    "toolCallTimeoutMs",
    "maxInstructionBytes",
    "failOnStartupError",
    "reconnect",
];
const ENTRY_FIELDS: [&str; 8] = [
    "type", "command", "args", "env", "cwd", "url", "headers", "enabled",
];

/// One server row as the catalog sees it.
pub(crate) struct DshMcpServer {
    pub row_id: String,
    pub name: String,
    pub entry: Value,
    pub enabled: bool,
    pub http: bool,
    pub command: Vec<String>,
    pub env: BTreeMap<String, String>,
    pub url: Option<String>,
    pub headers: BTreeMap<String, String>,
}

fn string_map(value: Option<&Value>) -> BTreeMap<String, String> {
    value
        .and_then(Value::as_object)
        .map(|map| {
            map.iter()
                .filter_map(|(key, value)| Some((key.clone(), value.as_str()?.to_owned())))
                .collect()
        })
        .unwrap_or_default()
}

/// The MCP rows the profile patch at `raw` inserts.
pub(crate) fn parse(raw: &str) -> Result<Vec<DshMcpServer>, AppError> {
    let file = PatchFile::from_text(raw);
    let rows: Vec<Map<String, Value>> = match file.layout() {
        Some(layout) => layout
            .inserted_rows()
            .into_iter()
            .map(|(row, _)| row.clone())
            .collect(),
        None => {
            return Err(AppError::InvalidRequest(format!(
                "dsh profile patch: {}",
                file.parse_error.as_deref().unwrap_or("unreadable")
            )))
        }
    };
    Ok(rows
        .into_iter()
        .filter(|row| row.get("name").and_then(Value::as_str) == Some(MCP_CLIENT))
        .filter_map(|row| {
            let row_id = row.get("id").and_then(Value::as_str)?.to_owned();
            let config = row.get("config").and_then(Value::as_object)?;
            let name = config.get("serverName").and_then(Value::as_str)?.to_owned();
            let http = config.get("transport").and_then(Value::as_str) == Some("streamable-http");
            let enabled = row.get("disabled").and_then(Value::as_bool) != Some(true);
            let mut entry = Map::new();
            entry.insert(
                "type".to_owned(),
                json!(if http { "http" } else { "stdio" }),
            );
            for key in ["command", "args", "env", "cwd", "url", "headers"]
                .into_iter()
                .chain(CLIENT_OPTIONS)
            {
                if let Some(value) = config.get(key) {
                    entry.insert(key.to_owned(), value.clone());
                }
            }
            if !enabled {
                entry.insert("enabled".to_owned(), json!(false));
            }
            let mut command = Vec::new();
            if let Some(bin) = config.get("command").and_then(Value::as_str) {
                command.push(bin.to_owned());
                command.extend(
                    config
                        .get("args")
                        .and_then(Value::as_array)
                        .into_iter()
                        .flatten()
                        .filter_map(Value::as_str)
                        .map(str::to_owned),
                );
            }
            Some(DshMcpServer {
                row_id,
                name,
                entry: Value::Object(entry),
                enabled,
                http,
                command,
                env: string_map(config.get("env")),
                url: config.get("url").and_then(Value::as_str).map(str::to_owned),
                headers: string_map(config.get("headers")),
            })
        })
        .collect())
}

fn validate_name(name: &str) -> Result<(), AppError> {
    let valid = !name.is_empty()
        && name.len() <= 32
        && name
            .chars()
            .all(|char| char.is_ascii_alphanumeric() || matches!(char, '_' | '-'));
    if !valid {
        return Err(AppError::InvalidRequest(
            "a DeepSeek Harness MCP server name is 1-32 letters, digits, '_' or '-'".to_owned(),
        ));
    }
    if name.to_ascii_lowercase().starts_with("todex_") {
        return Err(AppError::InvalidRequest(
            "names starting with todex_ are reserved for TodeX's own tools".to_owned(),
        ));
    }
    Ok(())
}

/// The dsh row for catalog entry `entry` named `name`.
fn row(row_id: &str, name: &str, entry: &Value) -> Result<Map<String, Value>, AppError> {
    validate_name(name)?;
    let entry = entry
        .as_object()
        .ok_or_else(|| AppError::InvalidRequest("MCP config must be a JSON object".to_owned()))?;
    if let Some(key) = entry.keys().find(|key| {
        !ENTRY_FIELDS.contains(&key.as_str()) && !CLIENT_OPTIONS.contains(&key.as_str())
    }) {
        return Err(AppError::InvalidRequest(format!(
            "`{key}` is not a DeepSeek Harness MCP option"
        )));
    }
    let kind = entry.get("type").and_then(Value::as_str);
    let http = match kind {
        Some("http") => true,
        Some("stdio") => false,
        Some("sse") => {
            return Err(AppError::InvalidRequest(
                "DeepSeek Harness has no SSE transport; use Streamable HTTP".to_owned(),
            ))
        }
        Some(other) => {
            return Err(AppError::InvalidRequest(format!(
                "unknown MCP transport {other}"
            )))
        }
        None => entry.get("url").is_some() && entry.get("command").is_none(),
    };
    let mut config = Map::new();
    config.insert("serverName".to_owned(), json!(name));
    if http {
        let url = entry.get("url").and_then(Value::as_str).unwrap_or_default();
        if !(url.starts_with("https://") || url.starts_with("http://")) {
            return Err(AppError::InvalidRequest(
                "a Streamable HTTP MCP server needs an http(s) url".to_owned(),
            ));
        }
        config.insert("transport".to_owned(), json!("streamable-http"));
        for key in ["url", "headers"] {
            if let Some(value) = entry.get(key) {
                config.insert(key.to_owned(), value.clone());
            }
        }
    } else {
        if !entry
            .get("command")
            .and_then(Value::as_str)
            .is_some_and(|command| !command.trim().is_empty())
        {
            return Err(AppError::InvalidRequest(
                "a stdio MCP server needs a command".to_owned(),
            ));
        }
        config.insert("transport".to_owned(), json!("stdio"));
        for key in ["command", "args", "env", "cwd"] {
            if let Some(value) = entry.get(key) {
                config.insert(key.to_owned(), value.clone());
            }
        }
    }
    for key in CLIENT_OPTIONS {
        if let Some(value) = entry.get(key) {
            config.insert(key.to_owned(), value.clone());
        }
    }
    let mut row = Map::new();
    row.insert("id".to_owned(), json!(row_id));
    row.insert("name".to_owned(), json!(MCP_CLIENT));
    if entry.get("enabled").and_then(Value::as_bool) == Some(false) {
        row.insert("disabled".to_owned(), json!(true));
    }
    row.insert("config".to_owned(), Value::Object(config));
    Ok(row)
}

fn new_row_id(name: &str) -> String {
    format!(
        "{ROW_PREFIX}{}",
        name.to_ascii_lowercase().replace('_', "-")
    )
}

fn ensure_unique(path: &Path, name: &str, except_row: Option<&str>) -> Result<(), AppError> {
    let raw = std::fs::read_to_string(path).unwrap_or_default();
    let taken = parse(&raw)?.into_iter().any(|server| {
        server.name.eq_ignore_ascii_case(name) && Some(server.row_id.as_str()) != except_row
    });
    if taken {
        return Err(AppError::Conflict(format!(
            "mcp server {name} already exists in {}",
            path.display()
        )));
    }
    Ok(())
}

pub(crate) fn insert(path: &Path, name: &str, entry: &Value) -> Result<(), AppError> {
    ensure_unique(path, name, None)?;
    let row_id = new_row_id(name);
    let row = row(&row_id, name, entry)?;
    edit_patch_at(path, None, |layout| {
        if layout
            .inserted_rows()
            .iter()
            .any(|(row, _)| row.get("id").and_then(Value::as_str) == Some(&row_id))
        {
            return Err(AppError::Conflict(format!(
                "dsh row {row_id} already exists"
            )));
        }
        layout.set_inserted_row(&row_id, Some(row))
    })
    .map(|_| ())
}

pub(crate) fn replace(
    path: &Path,
    row_id: &str,
    name: &str,
    entry: &Value,
) -> Result<(), AppError> {
    ensure_unique(path, name, Some(row_id))?;
    let row = row(row_id, name, entry)?;
    edit_patch_at(path, None, |layout| {
        if !layout
            .inserted_rows()
            .iter()
            .any(|(row, _)| row.get("id").and_then(Value::as_str) == Some(row_id))
        {
            return Err(AppError::NotFound(format!("mcp server row {row_id}")));
        }
        layout.set_inserted_row(row_id, Some(row))
    })
    .map(|_| ())
}

pub(crate) fn remove(path: &Path, row_id: &str) -> Result<(), AppError> {
    edit_patch_at(path, None, |layout| {
        if !layout
            .inserted_rows()
            .iter()
            .any(|(row, _)| row.get("id").and_then(Value::as_str) == Some(row_id))
        {
            return Err(AppError::NotFound(format!("mcp server row {row_id}")));
        }
        layout.set_inserted_row(row_id, None)
    })
    .map(|_| ())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn patch_path() -> (std::path::PathBuf, std::path::PathBuf) {
        let root = std::env::temp_dir().join(format!("todex-dsh-mcp-{}", uuid::Uuid::new_v4()));
        let path = root.join("profiles/acp/cordis.patch.yml");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        (root, path)
    }

    #[test]
    fn entries_round_trip_through_mcp_client_rows() {
        let (root, path) = patch_path();
        std::fs::write(
            &path,
            "# keep\n- id: web\n  disabled: true\n- insert:\n    - id: memory-engram\n      name: '@deepseek-ai/dsh-mcp-client'\n      config:\n        serverName: engram\n        transport: stdio\n        command: engram\n        cwd: !!js process.cwd()\n",
        )
        .unwrap();
        insert(
            &path,
            "fs_tools",
            &json!({"type": "stdio", "command": "/usr/bin/mcp-fs", "args": ["--root", "/w"],
                    "env": {"LEVEL": "1"}, "toolCallTimeoutMs": 90000}),
        )
        .unwrap();
        insert(
            &path,
            "remote",
            &json!({"type": "http", "url": "https://mcp.example/mcp", "headers": {"X-A": "b"}}),
        )
        .unwrap();
        let text = std::fs::read_to_string(&path).unwrap();
        // The hand-written rows stay as they were, expression included.
        assert!(text.starts_with(
            "# keep\n- id: web\n  disabled: true\n- insert:\n    - id: memory-engram\n"
        ));
        assert!(text.contains("cwd: !!js process.cwd()"));
        let servers = parse(&text).unwrap();
        let names: Vec<_> = servers.iter().map(|server| server.name.as_str()).collect();
        assert_eq!(names, ["engram", "fs_tools", "remote"]);
        let fs_tools = &servers[1];
        assert_eq!(fs_tools.row_id, "todex-mcp-fs-tools");
        assert_eq!(fs_tools.command, ["/usr/bin/mcp-fs", "--root", "/w"]);
        assert_eq!(
            fs_tools.entry,
            json!({"type": "stdio", "command": "/usr/bin/mcp-fs", "args": ["--root", "/w"],
                   "env": {"LEVEL": "1"}, "toolCallTimeoutMs": 90000})
        );
        assert!(servers[2].http);
        assert_eq!(servers[2].url.as_deref(), Some("https://mcp.example/mcp"));

        replace(
            &path,
            "todex-mcp-fs-tools",
            "fs_tools",
            &json!({"command": "/usr/bin/mcp-fs", "enabled": false}),
        )
        .unwrap();
        let servers = parse(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert!(!servers[1].enabled);
        assert_eq!(servers[1].entry["enabled"], false);
        // A row with an expression cannot be rewritten.
        assert!(matches!(
            replace(
                &path,
                "memory-engram",
                "engram",
                &json!({"command": "engram"})
            ),
            Err(AppError::Conflict(_))
        ));
        remove(&path, "todex-mcp-remote").unwrap();
        assert_eq!(
            parse(&std::fs::read_to_string(&path).unwrap())
                .unwrap()
                .len(),
            2
        );
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn invalid_entries_are_refused() {
        let (root, path) = patch_path();
        for (name, entry) in [
            ("todex_desktop", json!({"command": "/bin/x"})),
            ("has space", json!({"command": "/bin/x"})),
            ("events", json!({"type": "sse", "url": "https://x"})),
            ("nothing", json!({"type": "stdio"})),
            ("extra", json!({"command": "/bin/x", "bogus": 1})),
        ] {
            assert!(insert(&path, name, &entry).is_err(), "{name}");
        }
        insert(&path, "dup", &json!({"command": "/bin/x"})).unwrap();
        assert!(matches!(
            insert(&path, "DUP", &json!({"command": "/bin/y"})),
            Err(AppError::Conflict(_))
        ));
        std::fs::remove_dir_all(root).unwrap();
    }
}
