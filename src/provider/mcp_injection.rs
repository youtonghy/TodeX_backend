//! Renders TodeX's agent MCP servers ([`AgentMcpLaunch`]) in each provider's
//! own config format; which one a provider uses is its profile's
//! [`McpInjection`](super::profile::McpInjection).

use serde_json::{json, Value};

use crate::agent_mcp::{AgentMcpLaunch, AgentMcpServer};
use crate::error::AppError;
use crate::secure_fs;

fn env_map(server: &AgentMcpServer) -> serde_json::Map<String, Value> {
    server
        .env
        .iter()
        .map(|(name, value)| (name.clone(), Value::String(value.clone())))
        .collect()
}

/// Codex `config` override (dotted key, so the user's own `mcp_servers`
/// table is merged rather than replaced). Tools are pre-approved: TodeX
/// grants access itself (per SSH host, or through its own permission
/// prompts).
pub(crate) fn codex_config(server: &AgentMcpServer) -> (String, Value) {
    (
        format!("mcp_servers.{}", server.name),
        json!({
            "command": server.command,
            "args": server.args(),
            "env": env_map(server),
            "default_tools_approval_mode": "approve",
            "tool_timeout_sec": server.tool_timeout_seconds,
        }),
    )
}

/// Codex `config` overrides, one dotted key per server.
pub(crate) fn codex_configs(launch: &AgentMcpLaunch) -> Vec<(String, Value)> {
    launch.servers.iter().map(codex_config).collect()
}

/// ACP `mcpServers` (`McpServerStdio` entries) for `session/new|load|resume`.
pub(crate) fn acp_servers(launch: &AgentMcpLaunch) -> Value {
    Value::Array(
        launch
            .servers
            .iter()
            .map(|server| {
                json!({
                    "name": server.name,
                    "command": server.command,
                    "args": server.args(),
                    "env": server
                        .env
                        .iter()
                        .map(|(name, value)| json!({ "name": name, "value": value }))
                        .collect::<Vec<_>>(),
                })
            })
            .collect(),
    )
}

/// Claude Code arguments. The config goes to an owner-only file rather than
/// the command line, where other local users could read the token.
pub(crate) async fn claude_args(launch: &AgentMcpLaunch) -> Result<Vec<String>, AppError> {
    let servers: serde_json::Map<String, Value> = launch
        .servers
        .iter()
        .map(|server| {
            (
                server.name.to_owned(),
                json!({
                    "type": "stdio",
                    "command": server.command,
                    "args": server.args(),
                    "env": env_map(server),
                    "timeout": server.tool_timeout_seconds * 1000,
                }),
            )
        })
        .collect();
    let config = serde_json::to_vec(&json!({ "mcpServers": servers }))?;
    let path = launch.config_file.clone();
    let written = path.clone();
    tokio::task::spawn_blocking(move || secure_fs::write_owner_only_atomic(&written, &config))
        .await
        .map_err(|error| AppError::Anyhow(error.into()))??;
    let allowed = launch
        .servers
        .iter()
        .map(|server| format!("mcp__{}", server.name))
        .collect::<Vec<_>>()
        .join(",");
    // `=` form: both options are variadic and would otherwise swallow any
    // argument that follows them.
    Ok(vec![
        format!("--mcp-config={}", path.display()),
        // Server-level permission rules: these tools run without a Claude
        // prompt.
        format!("--allowedTools={allowed}"),
    ])
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::*;
    use crate::agent_mcp::{TOKEN_ENV, URL_ENV};

    fn server(name: &'static str, url: &str) -> AgentMcpServer {
        AgentMcpServer {
            name,
            route: "/internal/agent-mcp/test",
            command: PathBuf::from("/opt/todex/todex-agentd"),
            env: vec![
                (URL_ENV.to_owned(), url.to_owned()),
                (TOKEN_ENV.to_owned(), "t".to_owned()),
            ],
            tool_timeout_seconds: 660,
        }
    }

    #[tokio::test]
    async fn launch_formats_match_each_provider_schema() {
        let root = std::env::temp_dir().join(format!("todex-agent-mcp-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&root).unwrap();
        let launch = AgentMcpLaunch {
            servers: vec![
                server("todex_ssh", "http://127.0.0.1:1/ssh"),
                server("todex_other", "http://127.0.0.1:1/other"),
            ],
            config_file: root.join("claude.json"),
            global: None,
        };

        let codex = codex_configs(&launch);
        assert_eq!(codex.len(), 2);
        assert_eq!(codex[0].0, "mcp_servers.todex_ssh");
        assert_eq!(codex[1].0, "mcp_servers.todex_other");
        assert_eq!(codex[0].1["args"], json!(["agent-mcp-bridge"]));
        assert_eq!(codex[0].1["env"][TOKEN_ENV], "t");
        assert_eq!(codex[1].1["env"][URL_ENV], "http://127.0.0.1:1/other");
        assert_eq!(codex[0].1["default_tools_approval_mode"], "approve");
        assert_eq!(codex[0].1["tool_timeout_sec"], 660);

        let acp = acp_servers(&launch);
        assert_eq!(acp[0]["name"], "todex_ssh");
        assert_eq!(acp[1]["name"], "todex_other");
        assert_eq!(acp[0]["env"][1], json!({ "name": TOKEN_ENV, "value": "t" }));
        // The typed ACP schema accepts every entry as a stdio server.
        for entry in acp.as_array().unwrap() {
            let parsed: agent_client_protocol::schema::v1::McpServer =
                serde_json::from_value(entry.clone()).unwrap();
            assert!(matches!(
                parsed,
                agent_client_protocol::schema::v1::McpServer::Stdio(_)
            ));
        }

        let args = claude_args(&launch).await.unwrap();
        assert_eq!(args[1], "--allowedTools=mcp__todex_ssh,mcp__todex_other");
        let config: Value = serde_json::from_slice(
            &std::fs::read(args[0].strip_prefix("--mcp-config=").unwrap()).unwrap(),
        )
        .unwrap();
        assert_eq!(config["mcpServers"]["todex_ssh"]["type"], "stdio");
        assert_eq!(config["mcpServers"]["todex_ssh"]["timeout"], 660_000);
        assert_eq!(
            config["mcpServers"]["todex_other"]["env"][URL_ENV],
            "http://127.0.0.1:1/other"
        );
        let _ = std::fs::remove_dir_all(root);
    }
}
