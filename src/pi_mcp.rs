//! Pi's native MCP servers: the global `mcp.json` under the Pi agent
//! directory, for the "MCP" tab of the Pi settings.
//!
//! A patch replaces or removes whole server entries and may set the
//! top-level `autoEnableCodemode`; every other key of the file is kept as
//! stored. Each entry a patch writes is checked with Pi's own rules, so the
//! file never holds a server Pi would reject at startup.

use std::collections::HashMap;

use serde::Serialize;
use serde_json::{Map, Value};

use crate::{
    agent_providers::AgentDirs,
    cli_settings::{self, SettingsFile},
    error::AppError,
    pi_plugins::{self, PackageState},
};

const SERVERS_KEY: &str = "mcpServers";
const AUTO_CODEMODE_KEY: &str = "autoEnableCodemode";
const EXPOSURES: [&str; 4] = ["codemode", "deferred", "direct", "hidden"];
const EXPOSURE_ALIASES: [&str; 1] = ["codemode-deferred"];
const ADAPTER_PACKAGE: &str = "pi-mcp-adapter";

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct PiMcpDocument {
    pub path: String,
    pub exists: bool,
    pub revision: String,
    /// The parsed file; `None` when it does not parse.
    pub config: Option<Map<String, Value>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub parse_error: Option<String>,
    /// Whether `builtin:mcp` is on in `settings.json`; Pi ignores
    /// `mcp.json` while it is off.
    pub builtin_enabled: bool,
    /// Whether an enabled `pi-mcp-adapter` package takes over `/mcp`, in
    /// which case Pi does not read `mcp.json` either.
    pub adapter_installed: bool,
}

fn mcp_file(dirs: &AgentDirs) -> SettingsFile {
    SettingsFile::json(dirs.pi_dir().join("mcp.json"))
}

pub(crate) fn read(dirs: &AgentDirs) -> Result<PiMcpDocument, AppError> {
    let document = cli_settings::read(&mcp_file(dirs))?;
    let plugins = pi_plugins::list(dirs, Ok(HashMap::new()))?;
    Ok(PiMcpDocument {
        path: document.path,
        exists: document.exists,
        revision: document.revision,
        config: document.settings,
        parse_error: document.parse_error,
        builtin_enabled: plugins
            .builtins
            .iter()
            .any(|builtin| builtin.id == "mcp" && builtin.enabled),
        adapter_installed: plugins.packages.iter().any(|package| {
            package.state != PackageState::Disabled
                && (package.name.as_deref() == Some(ADAPTER_PACKAGE)
                    || package.source.contains(ADAPTER_PACKAGE))
        }),
    })
}

/// Applies `patch` if the file is still at `expected_revision`. Each entry
/// of `patch.mcpServers` replaces the stored server of that name, or
/// removes it when `null`; `autoEnableCodemode` is set, or removed when
/// `null`.
pub(crate) fn patch(
    dirs: &AgentDirs,
    expected_revision: &str,
    patch: &Map<String, Value>,
) -> Result<PiMcpDocument, AppError> {
    if let Some(key) = patch
        .keys()
        .find(|key| *key != SERVERS_KEY && *key != AUTO_CODEMODE_KEY)
    {
        return Err(AppError::InvalidRequest(format!(
            "{key} cannot be edited here"
        )));
    }
    let servers = match patch.get(SERVERS_KEY) {
        None => Map::new(),
        Some(Value::Object(servers)) => servers.clone(),
        Some(_) => {
            return Err(AppError::InvalidRequest(
                "mcpServers must be an object".to_owned(),
            ))
        }
    };
    for (name, entry) in &servers {
        if !entry.is_null() {
            validate_server(name, entry).map_err(AppError::InvalidRequest)?;
        }
    }
    let auto_codemode = patch.get(AUTO_CODEMODE_KEY).cloned();
    if auto_codemode
        .as_ref()
        .is_some_and(|value| !value.is_null() && !value.is_boolean())
    {
        return Err(AppError::InvalidRequest(
            "autoEnableCodemode must be a boolean".to_owned(),
        ));
    }
    cli_settings::modify(&mcp_file(dirs), expected_revision, |config| {
        match auto_codemode {
            Some(Value::Null) => {
                config.remove(AUTO_CODEMODE_KEY);
            }
            Some(value) => {
                config.insert(AUTO_CODEMODE_KEY.to_owned(), value);
            }
            None => {}
        }
        if servers.is_empty() {
            return Ok(());
        }
        let stored = config
            .entry(SERVERS_KEY)
            .or_insert_with(|| Value::Object(Map::new()));
        let Value::Object(stored) = stored else {
            return Err(AppError::Conflict(
                "mcp.json `mcpServers` is not an object".to_owned(),
            ));
        };
        for (name, entry) in servers {
            if entry.is_null() {
                stored.remove(&name);
            } else {
                stored.insert(name, entry);
            }
        }
        if let Some(clash) = namespace_clash(stored.keys()) {
            return Err(AppError::InvalidRequest(clash));
        }
        Ok(())
    })?;
    read(dirs)
}

/// Names that differ only in `-` and `_` share Pi's tool namespace.
fn namespace_clash<'a>(names: impl Iterator<Item = &'a String>) -> Option<String> {
    let mut seen: HashMap<String, &String> = HashMap::new();
    for name in names {
        if let Some(other) = seen.insert(name.replace('-', "_"), name) {
            return Some(format!("server \"{name}\" conflicts with \"{other}\""));
        }
    }
    None
}

fn is_exposure(value: &Value) -> bool {
    value
        .as_str()
        .is_some_and(|value| EXPOSURES.contains(&value) || EXPOSURE_ALIASES.contains(&value))
}

fn is_string_map(value: &Value) -> bool {
    value
        .as_object()
        .is_some_and(|map| map.values().all(Value::is_string))
}

fn is_http_url(value: &str) -> bool {
    ["http://", "https://"].iter().any(|scheme| {
        value
            .strip_prefix(scheme)
            .is_some_and(|rest| !rest.is_empty() && !rest.starts_with('/'))
    })
}

/// A server entry of the global file, by the rules of Pi's
/// `validateMcpServerConfig`. OAuth settings are left to Pi.
pub(crate) fn validate_server(name: &str, entry: &Value) -> Result<(), String> {
    if name.is_empty()
        || !name
            .chars()
            .all(|ch| ch.is_ascii_alphanumeric() || ch == '_' || ch == '-')
    {
        return Err(format!(
            "invalid server name \"{name}\" (use letters, digits, \"_\" and \"-\")"
        ));
    }
    let Some(server) = entry.as_object() else {
        return Err(format!("server \"{name}\" must be an object"));
    };
    let field = |key: &str| server.get(key);
    let fail = |message: &str| Err(format!("server \"{name}\": {message}"));
    if field("exposure").is_some_and(|value| !is_exposure(value)) {
        return fail("exposure must be one of codemode, deferred, direct, hidden");
    }
    if let Some(tools) = field("toolExposure") {
        if !tools
            .as_object()
            .is_some_and(|tools| tools.values().all(is_exposure))
        {
            return fail("toolExposure must map tool names to exposures");
        }
    }
    if field("enabled").is_some_and(|value| !value.is_boolean()) {
        return fail("enabled must be a boolean");
    }
    if field("description").is_some_and(|value| !value.is_string()) {
        return fail("description must be a string");
    }
    if field("timeout").is_some_and(|value| !value.as_f64().is_some_and(|seconds| seconds > 0.0)) {
        return fail("timeout must be a positive number of seconds");
    }
    if field("oauth").is_some_and(|value| !value.is_object()) {
        return fail("oauth must be an object");
    }
    let kind = field("type").and_then(Value::as_str);
    if kind == Some("sse") {
        return fail("legacy SSE transport is not supported; use the streamable HTTP URL");
    }
    if let Some(url) = field("url").and_then(Value::as_str) {
        if matches!(kind, None | Some("http") | Some("streamable-http")) {
            if !is_http_url(url) {
                return fail("url must be an http or https URL");
            }
            if field("headers").is_some_and(|value| !is_string_map(value)) {
                return fail("headers must map names to strings");
            }
            if field("auth").is_some_and(|auth| {
                !auth
                    .get("provider")
                    .and_then(Value::as_str)
                    .is_some_and(|provider| !provider.is_empty())
            }) {
                return fail("auth.provider must be a provider name");
            }
            return Ok(());
        }
    }
    if let Some(command) = field("command").and_then(Value::as_str) {
        if matches!(kind, None | Some("stdio")) {
            if command.trim().is_empty() {
                return fail("command must not be empty");
            }
            if field("args").is_some_and(|args| {
                !args
                    .as_array()
                    .is_some_and(|args| args.iter().all(Value::is_string))
            }) {
                return fail("args must be an array of strings");
            }
            if field("env").is_some_and(|value| !is_string_map(value)) {
                return fail("env must map names to strings");
            }
            if field("cwd").is_some_and(|value| !value.is_string()) {
                return fail("cwd must be a string");
            }
            return Ok(());
        }
    }
    fail("needs either \"command\" (stdio) or \"url\" (streamable HTTP)")
}

#[cfg(test)]
mod tests {
    use std::fs;

    use serde_json::json;

    use super::*;

    fn temp_dirs() -> AgentDirs {
        let root = std::env::temp_dir().join(format!("pi-mcp-{}", uuid::Uuid::new_v4().simple()));
        fs::create_dir_all(&root).unwrap();
        AgentDirs::for_tests_with_pi_dir(root)
    }

    fn object(value: Value) -> Map<String, Value> {
        value.as_object().unwrap().clone()
    }

    fn stored(dirs: &AgentDirs) -> Value {
        serde_json::from_slice(&fs::read(dirs.pi_dir().join("mcp.json")).unwrap()).unwrap()
    }

    #[test]
    fn creates_replaces_renames_and_removes_servers() {
        let dirs = temp_dirs();
        let first = read(&dirs).unwrap();
        assert!(!first.exists);
        assert!(first.builtin_enabled);
        assert!(!first.adapter_installed);

        let created = patch(
            &dirs,
            &first.revision,
            &object(json!({
                "mcpServers": {"docs": {"command": "npx", "args": ["-y", "docs"], "toolExposure": {"x*": "direct"}}},
                "autoEnableCodemode": false,
            })),
        )
        .unwrap();
        assert!(created.exists);
        assert_eq!(stored(&dirs)["autoEnableCodemode"], json!(false));

        // A replaced entry drops the keys it no longer sends.
        let replaced = patch(
            &dirs,
            &created.revision,
            &object(json!({"mcpServers": {"docs": null, "web-docs": {"url": "https://x.dev/mcp", "enabled": false}}})),
        )
        .unwrap();
        assert_eq!(
            stored(&dirs)["mcpServers"],
            json!({"web-docs": {"url": "https://x.dev/mcp", "enabled": false}})
        );

        assert!(matches!(
            patch(
                &dirs,
                &created.revision,
                &object(json!({"autoEnableCodemode": null}))
            ),
            Err(AppError::Conflict(_))
        ));
        patch(
            &dirs,
            &replaced.revision,
            &object(json!({"autoEnableCodemode": null})),
        )
        .unwrap();
        assert_eq!(stored(&dirs).get("autoEnableCodemode"), None);
    }

    #[test]
    fn keeps_other_keys_and_rejects_invalid_entries() {
        let dirs = temp_dirs();
        fs::write(
            dirs.pi_dir().join("mcp.json"),
            r#"{"mcpServers":{"a_b":{"command":"x"}},"future":1}"#,
        )
        .unwrap();
        let document = read(&dirs).unwrap();
        let rejected = |value: Value| {
            matches!(
                patch(&dirs, &document.revision, &object(value)),
                Err(AppError::InvalidRequest(_))
            )
        };
        assert!(rejected(json!({"future": 2})));
        assert!(rejected(json!({"mcpServers": {"a-b": {"command": "y"}}})));
        assert!(rejected(
            json!({"mcpServers": {"bad name": {"command": "y"}}})
        ));
        assert!(rejected(
            json!({"mcpServers": {"s": {"type": "sse", "url": "https://x"}}})
        ));
        assert!(rejected(json!({"mcpServers": {"s": {"url": "ftp://x"}}})));
        assert!(rejected(
            json!({"mcpServers": {"s": {"command": "x", "exposure": "all"}}})
        ));
        assert!(rejected(
            json!({"mcpServers": {"s": {"command": "x", "env": {"K": 1}}}})
        ));
        assert!(rejected(json!({"mcpServers": {"s": {"enabled": false}}})));
        assert!(rejected(json!({"autoEnableCodemode": "yes"})));

        patch(
            &dirs,
            &document.revision,
            &object(
                json!({"mcpServers": {"c": {"command": "z", "exposure": "codemode-deferred"}}}),
            ),
        )
        .unwrap();
        let saved = stored(&dirs);
        assert_eq!(saved["future"], json!(1));
        assert_eq!(saved["mcpServers"]["a_b"], json!({"command": "x"}));
    }

    #[test]
    fn reports_builtin_and_adapter_state() {
        let dirs = temp_dirs();
        fs::write(
            dirs.pi_dir().join("settings.json"),
            r#"{"extensions":["-builtin:mcp"],"packages":["npm:pi-mcp-adapter"]}"#,
        )
        .unwrap();
        let document = read(&dirs).unwrap();
        assert!(!document.builtin_enabled);
        assert!(document.adapter_installed);
    }
}
