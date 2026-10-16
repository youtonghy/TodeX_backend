//! TodeX approvals for the Antigravity CLI.
//!
//! Headless `agy` denies every tool that needs approval, so TodeX runs it
//! with `--dangerously-skip-permissions` and gates tools itself: a global
//! `PreToolUse` hook (installed by `provider::antigravity`) runs
//! `todex-agentd agy-hook`, which posts the hook payload to
//! [`AGY_HOOK_ROUTE`] with the conversation's token and prints the answer.
//! The decision follows the turn's permission mode and, where it asks,
//! raises the same `permission.requested` prompt as other provider tools.
//!
//! An `auto` turn runs agy's own Auto review (`--mode auto`, without
//! `--dangerously-skip-permissions`) when the account offers it; the hook
//! then answers nothing for what that review decides. It does so only when
//! the `agy` process says so through [`AGY_NATIVE_AUTO_ENV`]: an `auto` turn
//! launched without it (Auto unavailable) runs with skipped permissions,
//! so the hook asks as in `ask` mode.
//!
//! In ask mode, read-class tools are allowed without a prompt only when
//! every path they name resolves inside the conversation's workspace (or
//! agy's own bundled docs and this conversation's artifacts); other reads
//! ask like an edit.
//!
//! Otherwise an empty hook answer leaves agy's own permissions in charge,
//! which `--dangerously-skip-permissions` turns off, so every path that
//! cannot reach a decision answers with an explicit denial.

use std::{
    path::{Component, Path, PathBuf},
    time::Duration,
};

use axum::{
    extract::{Request, State},
    response::{IntoResponse, Response},
    routing::post,
    Json, Router,
};
use serde_json::{json, Value};
use uuid::Uuid;

use super::{
    authorizer::{action_key, Answerer, CancelSignal, Prompt},
    server, AGY_HOOK_ROUTE, AGY_NATIVE_AUTO_ENV, AGY_NATIVE_AUTO_HEADER, ENDPOINT_ENV, TOKEN_ENV,
};
use crate::app_state::AppState;

const MAX_PAYLOAD_BYTES: usize = 1024 * 1024;
/// Text of a command, path or URL in a prompt title.
const SUMMARY_CHARS: usize = 200;

pub(super) fn routes() -> Router<AppState> {
    Router::new().route(AGY_HOOK_ROUTE, post(decide_request))
}

/// What a hook answers agy.
#[derive(Debug, PartialEq, Eq)]
enum Decision {
    Allow,
    Deny(String),
    /// Leave the call to agy's own Auto review: the hook prints nothing.
    Defer,
}

impl Decision {
    fn to_json(&self) -> Value {
        match self {
            Self::Allow => json!({ "decision": "allow" }),
            Self::Deny(reason) => json!({ "decision": "deny", "reason": reason }),
            Self::Defer => json!({ "decision": "defer" }),
        }
    }
}

async fn decide_request(State(state): State<AppState>, request: Request) -> Response {
    let conversation_id = match server::local_caller(&state, &request) {
        Ok(conversation_id) => conversation_id,
        Err(response) => return *response,
    };
    let native_auto = native_auto_flag(
        request
            .headers()
            .get(AGY_NATIVE_AUTO_HEADER)
            .map(|value| value.as_bytes()),
    );
    let decision = match axum::body::to_bytes(request.into_body(), MAX_PAYLOAD_BYTES).await {
        Ok(body) => match serde_json::from_slice::<Value>(&body) {
            Ok(payload) => decide(&state, &conversation_id, native_auto, &payload).await,
            Err(error) => Decision::Deny(format!("TodeX could not read the hook payload: {error}")),
        },
        Err(error) => Decision::Deny(format!("TodeX could not read the hook payload: {error}")),
    };
    Json(decision.to_json()).into_response()
}

/// How a tool is treated, by what it can do.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ToolClass {
    /// Reads the workspace or agy's own state.
    Read,
    /// A file read ([`PATH_READ_TOOLS`]) naming a path outside the allowed
    /// roots, or no recognizable path at all.
    ReadOutside,
    /// The agent's own bookkeeping: questions, subagents, waiting.
    Internal,
    /// Writes a file.
    Edit,
    /// Runs or feeds a command.
    Command,
    /// Reaches the network or drives a browser.
    Network,
    /// A tool of an MCP server; TodeX's own servers gate themselves.
    Mcp {
        todex: bool,
    },
    Unknown,
}

fn classify(name: &str, args: &Value) -> ToolClass {
    match name {
        "view_file"
        | "list_dir"
        | "grep_search"
        | "find_by_name"
        | "command_status"
        | "list_resources"
        | "read_resource"
        | "list_permissions"
        | "list_plugin_accounts" => ToolClass::Read,
        "ask_question"
        | "ask_permission"
        | "ask_custom_permission"
        | "finish"
        | "wait"
        | "wait_5_seconds"
        | "define_subagent"
        | "invoke_subagent"
        | "manage_subagents"
        | "send_message"
        | "manage_inbox"
        | "generate_image" => ToolClass::Internal,
        "write_to_file"
        | "replace_file_content"
        | "multi_replace_file_content"
        | "sed_file"
        | "notebook_edit" => ToolClass::Edit,
        "run_command" | "send_command_input" | "manage_task" | "notebook_execution" => {
            ToolClass::Command
        }
        "read_url_content"
        | "search_web"
        | "search_marketplace"
        | "open_browser_url"
        | "click_browser_pixel"
        | "execute_browser_javascript"
        | "list_browser_pages"
        | "read_browser_page" => ToolClass::Network,
        "call_mcp_tool" => ToolClass::Mcp {
            todex: args
                .get("ServerName")
                .and_then(Value::as_str)
                .is_some_and(|server| {
                    server == super::SSH_SERVER || server == super::DESKTOP_SERVER
                }),
        },
        _ if name.starts_with("browser_") || name.starts_with("capture_browser_") => {
            ToolClass::Network
        }
        _ => ToolClass::Unknown,
    }
}

/// What the turn's mode makes of a call, before any prompt.
#[derive(Debug, PartialEq, Eq)]
enum Policy {
    Allow,
    Ask,
    Deny(String),
    Defer,
}

/// `native_auto`: agy itself reviews this turn's calls (`--mode auto`).
fn policy(permission_mode: &str, native_auto: bool, work_mode: &str, class: ToolClass) -> Policy {
    use ToolClass::*;
    if work_mode == "plan" {
        // agy's own review also refuses these: Plan turns keep it on.
        return match class {
            Read | Internal | Mcp { todex: true } => Policy::Allow,
            ReadOutside if permission_mode == "full-access" => Policy::Allow,
            ReadOutside => Policy::Ask,
            _ => Policy::Deny(
                "The conversation is in Plan mode: only reading is allowed. Describe this step in \
                 your plan instead."
                    .to_owned(),
            ),
        };
    }
    match (permission_mode, class) {
        ("full-access", _) => Policy::Allow,
        (_, Read | Internal | Mcp { todex: true }) => Policy::Allow,
        // agy's Auto review decides; without it `auto` asks like `ask`.
        ("auto", _) if native_auto => Policy::Defer,
        // `ask`, and anything else, asks.
        _ => Policy::Ask,
    }
}

async fn decide(
    state: &AppState,
    conversation_id: &str,
    native_auto: bool,
    payload: &Value,
) -> Decision {
    let Some((permission_mode, work_mode)) = state.agent_mcp.turn_mode(conversation_id) else {
        return Decision::Deny("No TodeX turn is running in this conversation.".to_owned());
    };
    let name = payload
        .pointer("/toolCall/name")
        .and_then(Value::as_str)
        .unwrap_or_default();
    let args = payload
        .pointer("/toolCall/args")
        .cloned()
        .unwrap_or(Value::Null);
    let mut class = classify(name, &args);
    if class == ToolClass::Read
        && permission_mode != "full-access"
        && PATH_READ_TOOLS.contains(&name)
    {
        let roots = match state.conversations.get(conversation_id).await {
            Ok(manifest) => read_roots(&manifest.workspace, payload),
            Err(_) => Vec::new(),
        };
        let paths = read_paths(&args);
        let inside = tokio::task::spawn_blocking(move || paths_inside(&roots, &paths))
            .await
            .unwrap_or(false);
        if !inside {
            class = ToolClass::ReadOutside;
        }
    }
    match policy(&permission_mode, native_auto, &work_mode, class) {
        Policy::Allow => Decision::Allow,
        Policy::Deny(reason) => Decision::Deny(reason),
        Policy::Defer => Decision::Defer,
        Policy::Ask => ask(state, conversation_id, name, args).await,
    }
}

/// Read tools that take filesystem paths. In ask mode they are allowed
/// without a prompt only inside [`read_roots`].
const PATH_READ_TOOLS: &[&str] = &["view_file", "list_dir", "grep_search", "find_by_name"];

/// Where reads need no prompt: the conversation's workspace, agy's bundled
/// docs (`~/.gemini/antigravity-cli/builtin`), and this conversation's
/// artifact directory when it lies under agy's `brain` directory.
fn read_roots(workspace: &Path, payload: &Value) -> Vec<PathBuf> {
    let mut roots = vec![workspace.to_path_buf()];
    if let Some(gemini) = crate::provider::antigravity::integration_dir().parent() {
        let cli = gemini.join("antigravity-cli");
        roots.push(cli.join("builtin"));
        if let Some(artifacts) = payload
            .get("artifactDirectoryPath")
            .and_then(Value::as_str)
            .map(PathBuf::from)
        {
            let brain = cli.join("brain");
            let canonical = |path: &Path| std::fs::canonicalize(path).ok();
            if let (Some(brain), Some(artifacts)) = (canonical(&brain), canonical(&artifacts)) {
                if artifacts.starts_with(&brain) && artifacts != brain {
                    roots.push(artifacts);
                }
            }
        }
    }
    roots
}

/// Every path-like argument of a read: string (or string array) values whose
/// key ends in `path`, `paths`, `directory`, `dir`, `file` or `uri`.
/// Booleans, numbers and nulls under such keys (flags like `FullPath`) are
/// not paths and are skipped; other values count as an unclear path.
fn read_paths(args: &Value) -> Vec<String> {
    let Some(args) = args.as_object() else {
        return Vec::new();
    };
    let mut paths = Vec::new();
    for (key, value) in args {
        let key = key.to_ascii_lowercase();
        if !["path", "paths", "directory", "dir", "file", "uri"]
            .iter()
            .any(|suffix| key.ends_with(suffix))
        {
            continue;
        }
        let not_a_path =
            |value: &Value| matches!(value, Value::Bool(_) | Value::Number(_) | Value::Null);
        match value {
            Value::String(path) => paths.push(path.clone()),
            Value::Array(items) => paths.extend(
                items
                    .iter()
                    .filter(|item| !not_a_path(item))
                    .map(|item| item.as_str().map(str::to_owned).unwrap_or_default()),
            ),
            value if not_a_path(value) => {}
            _ => paths.push(String::new()),
        }
    }
    paths
}

/// Whether there is at least one path and each resolves, component-wise,
/// inside one of `roots`. Paths that do not exist yet are judged by their
/// nearest existing ancestor; anything unclear counts as outside.
fn paths_inside(roots: &[PathBuf], paths: &[String]) -> bool {
    let roots: Vec<PathBuf> = roots
        .iter()
        .filter_map(|root| std::fs::canonicalize(root).ok())
        .collect();
    !roots.is_empty()
        && !paths.is_empty()
        && paths.iter().all(|path| {
            resolve_read_path(&roots[0], path)
                .is_some_and(|resolved| roots.iter().any(|root| resolved.starts_with(root)))
        })
}

/// The canonical form of `path` (relative paths are taken from the
/// workspace, the first root).
fn resolve_read_path(workspace: &Path, path: &str) -> Option<PathBuf> {
    let path = path.trim();
    let path = match path.strip_prefix("file://") {
        // `file://host/share` names a remote host.
        Some(local) if !local.starts_with('/') => return None,
        Some(local) => local,
        None => path,
    };
    // A percent-encoded path could decode to something else in agy.
    if path.is_empty() || path.contains(['%', '\0']) || is_remote_or_device_path(path) {
        return None;
    }
    let path = crate::config::expand_home(PathBuf::from(path));
    let path = if path.is_absolute() {
        path
    } else {
        workspace.join(path)
    };
    let mut existing = path.as_path();
    let mut rest = Vec::new();
    loop {
        match std::fs::canonicalize(existing) {
            Ok(canonical) => {
                let mut resolved = canonical;
                for component in rest.iter().rev() {
                    resolved.push(component);
                }
                return Some(resolved);
            }
            Err(_) => {
                let name = existing.file_name()?;
                // Only plain names may follow the existing ancestor.
                if !matches!(
                    Path::new(name).components().next(),
                    Some(Component::Normal(_))
                ) {
                    return None;
                }
                rest.push(name.to_owned());
                existing = existing.parent()?;
            }
        }
    }
}

/// UNC shares (`\\server\share`, `//server/share`), Windows device
/// namespaces (`\\?\`, `\\.\`, `\??\`) and Unix network automount trees
/// (`/net`, `/afs`). They count as outside without being resolved: even a
/// `canonicalize` could reach out to a remote host.
fn is_remote_or_device_path(path: &str) -> bool {
    const NETWORK_ROOTS: &[&str] = &["/net", "/afs"];
    let separator = |ch: char| matches!(ch, '/' | '\\');
    let mut chars = path.chars();
    let doubled_separator =
        chars.next().is_some_and(separator) && chars.next().is_some_and(separator);
    doubled_separator
        || path.starts_with(r"\??\")
        || NETWORK_ROOTS.iter().any(|root| {
            path.strip_prefix(root)
                .is_some_and(|rest| rest.is_empty() || rest.starts_with(separator))
        })
}

/// Asks the user's devices about one call.
async fn ask(state: &AppState, conversation_id: &str, name: &str, args: Value) -> Decision {
    let always_key = format!("agy.{name}");
    let authorizer_state = &state.agent_mcp.inner.authorizer;
    if authorizer_state.always_allows(conversation_id, &always_key) {
        return Decision::Allow;
    }
    // The hook request lives as long as agy waits; a cancelled turn ends
    // the prompt through the turn's own cancellation.
    let (_cancel, cancel) = CancelSignal::manual();
    let command = args.get("CommandLine").and_then(Value::as_str);
    let summary = summary(name, &args);
    let mut details = json!({
        "tool_name": name,
        "input": args,
        "summary": summary,
        "provider": "antigravity",
        "requestId": Uuid::new_v4().simple().to_string(),
    });
    if let Some(command) = command {
        details["command"] = json!(command);
    }
    let prompt = Prompt {
        key: action_key(&format!("agy:{name}"), &args),
        answerer: Answerer::AnyDevice,
        kind: "tool",
        title: format!("Allow {name}: {summary}?"),
        message: String::new(),
        details,
        options: json!([
            { "id": "allow_once", "kind": "allow_once", "name": "Allow once" },
            { "id": "allow_always", "kind": "allow_always", "name": format!("Always allow {name} in this conversation") },
            { "id": "reject_once", "kind": "reject_once", "name": "Reject" }
        ]),
        once: false,
    };
    match state
        .agent_mcp
        .authorizer(&state.conversations)
        .ask(conversation_id, prompt, &cancel)
        .await
    {
        Ok(approval) => {
            if approval.always {
                authorizer_state.allow_always(conversation_id, &always_key);
            }
            Decision::Allow
        }
        Err(denied) => Decision::Deny(format!(
            "{}. Do not work around this with other tools.",
            denied.or_declined(format!("The user rejected {name}"))
        )),
    }
}

/// One line saying what the call would do.
fn summary(name: &str, args: &Value) -> String {
    let text = [
        "CommandLine",
        "TargetFile",
        "AbsolutePath",
        "Url",
        "url",
        "Query",
        "ToolName",
    ]
    .iter()
    .find_map(|key| args.get(*key).and_then(Value::as_str))
    .map(str::to_owned)
    .unwrap_or_else(|| name.to_owned());
    let mut chars = text.chars();
    let short: String = chars.by_ref().take(SUMMARY_CHARS).collect();
    if chars.next().is_some() {
        format!("{short}…")
    } else {
        short
    }
}

/// Whether a native-auto marker ([`AGY_NATIVE_AUTO_ENV`] or
/// [`AGY_NATIVE_AUTO_HEADER`]) says agy runs its own Auto review: exactly
/// `1`, nothing else.
fn native_auto_flag(value: Option<&[u8]>) -> bool {
    value == Some(b"1")
}

/// What the hook prints for the daemon's answer; `None` prints nothing,
/// leaving the call to agy's own review. That is only right when this `agy`
/// process runs agy's Auto (`native_auto`): elsewhere it runs with
/// `--dangerously-skip-permissions`, so a `defer` there is denied.
fn hook_output(native_auto: bool, answer: anyhow::Result<Value>) -> Option<Value> {
    match answer {
        Ok(decision) if decision["decision"] == "defer" => (!native_auto).then(|| {
            Decision::Deny(
                "TodeX was asked to leave this call to agy's Auto review, but this agy process \
                 does not run it; start a new turn."
                    .to_owned(),
            )
            .to_json()
        }),
        Ok(decision) => Some(decision),
        Err(error) => Some(
            Decision::Deny(format!(
                "TodeX could not decide on this tool call ({error}); start a new turn if \
                 todex-agentd restarted."
            ))
            .to_json(),
        ),
    }
}

/// `todex-agentd agy-hook`: reads the hook payload on stdin, asks the daemon
/// and prints its decision. Started outside a TodeX turn, or told to defer to
/// agy's Auto review by a process that runs it, it prints nothing, which
/// leaves agy's own permissions in charge; a failed exchange denies.
pub(crate) async fn run_hook() -> anyhow::Result<()> {
    use tokio::io::AsyncReadExt;
    let env = |name: &str| std::env::var(name).ok().filter(|value| !value.is_empty());
    let (Some(endpoint), Some(token)) = (env(ENDPOINT_ENV), env(TOKEN_ENV)) else {
        let _ = tokio::io::copy(&mut tokio::io::stdin(), &mut tokio::io::sink()).await;
        return Ok(());
    };
    let mut payload = Vec::new();
    tokio::io::stdin()
        .take(MAX_PAYLOAD_BYTES as u64 + 1)
        .read_to_end(&mut payload)
        .await?;
    let native_auto = native_auto_flag(
        std::env::var(AGY_NATIVE_AUTO_ENV)
            .ok()
            .as_deref()
            .map(str::as_bytes),
    );
    let answer = ask_daemon(&endpoint, &token, native_auto, payload).await;
    if let Some(decision) = hook_output(native_auto, answer) {
        println!("{decision}");
    }
    Ok(())
}

async fn ask_daemon(
    endpoint: &str,
    token: &str,
    native_auto: bool,
    payload: Vec<u8>,
) -> anyhow::Result<Value> {
    if payload.len() > MAX_PAYLOAD_BYTES {
        anyhow::bail!("the hook payload is too large");
    }
    let client = reqwest::Client::builder()
        .connect_timeout(Duration::from_secs(5))
        .build()?;
    let mut request = client
        .post(format!("{endpoint}{AGY_HOOK_ROUTE}"))
        .bearer_auth(token);
    if native_auto {
        request = request.header(AGY_NATIVE_AUTO_HEADER, "1");
    }
    let response = request
        .header(reqwest::header::CONTENT_TYPE, "application/json")
        .body(payload)
        .send()
        .await?
        .error_for_status()?;
    let decision: Value = response.json().await?;
    match decision.get("decision").and_then(Value::as_str) {
        Some("allow" | "deny" | "defer") => Ok(decision),
        _ => anyhow::bail!("the daemon answered without a decision"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_exactly_1_marks_native_auto() {
        assert!(native_auto_flag(Some(b"1")));
        for value in [&b"true"[..], b"0", b" 1", b"1 ", b"", b"yes"] {
            assert!(!native_auto_flag(Some(value)), "{value:?}");
        }
        assert!(!native_auto_flag(None));
        // As the route reads the header.
        let header = |value: &'static str| {
            native_auto_flag(Some(axum::http::HeaderValue::from_static(value).as_bytes()))
        };
        assert!(header("1"));
        assert!(!header("true"));
        assert!(!header("0"));
        assert!(!header(" 1"));
    }

    #[test]
    fn a_defer_is_honoured_only_by_a_native_auto_process() {
        let defer = || Ok(json!({ "decision": "defer" }));
        assert_eq!(hook_output(true, defer()), None);
        let denied = hook_output(false, defer()).unwrap();
        assert_eq!(denied["decision"], "deny");
        assert!(denied["reason"].as_str().unwrap().contains("Auto"));
        let allow = json!({ "decision": "allow" });
        assert_eq!(hook_output(false, Ok(allow.clone())), Some(allow.clone()));
        assert_eq!(hook_output(true, Ok(allow.clone())), Some(allow));
        let failed = hook_output(true, Err(anyhow::anyhow!("connection refused"))).unwrap();
        assert_eq!(failed["decision"], "deny");
    }

    #[test]
    fn tools_are_classified_by_what_they_can_do() {
        assert_eq!(classify("view_file", &Value::Null), ToolClass::Read);
        assert_eq!(classify("ask_question", &Value::Null), ToolClass::Internal);
        assert_eq!(classify("write_to_file", &Value::Null), ToolClass::Edit);
        assert_eq!(classify("run_command", &Value::Null), ToolClass::Command);
        assert_eq!(
            classify("browser_click_element", &Value::Null),
            ToolClass::Network
        );
        assert_eq!(
            classify("call_mcp_tool", &json!({ "ServerName": "todex_ssh" })),
            ToolClass::Mcp { todex: true }
        );
        assert_eq!(
            classify("call_mcp_tool", &json!({ "ServerName": "reui" })),
            ToolClass::Mcp { todex: false }
        );
        assert_eq!(classify("run_workflow", &Value::Null), ToolClass::Unknown);
    }

    #[test]
    fn modes_decide_which_calls_ask() {
        use ToolClass::*;
        assert_eq!(
            policy("full-access", false, "implement", Command),
            Policy::Allow
        );
        assert_eq!(policy("ask", false, "implement", Read), Policy::Allow);
        assert_eq!(policy("ask", false, "implement", Edit), Policy::Ask);
        assert_eq!(policy("ask", false, "implement", Command), Policy::Ask);
        assert_eq!(
            policy("ask", false, "implement", Mcp { todex: false }),
            Policy::Ask
        );
        assert_eq!(
            policy("ask", false, "implement", Mcp { todex: true }),
            Policy::Allow
        );
        assert!(matches!(
            policy("full-access", false, "plan", Edit),
            Policy::Deny(_)
        ));
        assert_eq!(policy("ask", false, "plan", Read), Policy::Allow);
        assert_eq!(policy("ask", false, "implement", ReadOutside), Policy::Ask);
        assert_eq!(policy("ask", false, "plan", ReadOutside), Policy::Ask);
        assert_eq!(
            policy("full-access", false, "plan", ReadOutside),
            Policy::Allow
        );
        assert_eq!(
            policy("full-access", false, "implement", ReadOutside),
            Policy::Allow
        );
        // agy's Auto review decides what it covers; reads stay free.
        assert_eq!(policy("auto", true, "implement", Command), Policy::Defer);
        assert_eq!(
            policy("auto", true, "implement", ReadOutside),
            Policy::Defer
        );
        assert_eq!(policy("auto", true, "implement", Read), Policy::Allow);
        assert_eq!(policy("auto", true, "plan", ReadOutside), Policy::Ask);
        assert!(matches!(
            policy("auto", true, "plan", Edit),
            Policy::Deny(_)
        ));
        // Without agy's review (Auto unavailable), `auto` asks; so do unknown
        // modes, and the hint never loosens `ask`.
        assert_eq!(policy("auto", false, "implement", Edit), Policy::Ask);
        assert_eq!(policy("ask", true, "implement", Command), Policy::Ask);
        assert_eq!(policy("bogus", false, "implement", Command), Policy::Ask);
    }

    #[test]
    fn non_string_path_flags_are_not_paths() {
        let args = json!({
            "AbsolutePath": "/workspace/a.txt",
            "FullPath": true,
            "MaxFile": 3,
            "ExtraDir": null,
            "SearchPaths": ["/workspace/b", false],
        });
        assert_eq!(
            read_paths(&args),
            vec!["/workspace/a.txt".to_owned(), "/workspace/b".to_owned()]
        );
        // An object is not a recognizable path: it counts as unclear.
        assert_eq!(read_paths(&json!({ "File": {} })), vec![String::new()]);
    }

    #[test]
    fn remote_and_device_paths_are_outside_without_resolving() {
        let workspace = std::env::temp_dir();
        for path in [
            r"\\server\share\a.txt",
            "//server/share/a.txt",
            r"\\?\C:\secret",
            r"\\.\PhysicalDrive0",
            r"\??\C:\secret",
            r"/\server\share",
            "file://server/share/a.txt",
            "file:////server/share/a.txt",
            "/net/host/export",
            "/net",
            "/afs/cell/user",
        ] {
            assert_eq!(resolve_read_path(&workspace, path), None, "{path}");
        }
        for path in ["/network/a", "/afsx", "/tmp//a", "a//b"] {
            assert!(!is_remote_or_device_path(path), "{path}");
        }
    }

    #[test]
    fn summaries_name_the_command_and_stay_short() {
        assert_eq!(
            summary("run_command", &json!({ "CommandLine": "npm test" })),
            "npm test"
        );
        assert_eq!(summary("finish", &Value::Null), "finish");
        let long = "x".repeat(SUMMARY_CHARS + 10);
        assert!(summary("run_command", &json!({ "CommandLine": long })).ends_with('…'));
    }

    mod route {
        use std::{net::SocketAddr, time::Duration};

        use axum::{body::Body, extract::ConnectInfo, http::header};
        use tower::ServiceExt;

        use super::super::*;
        use crate::{
            config::Config,
            conversation::ProviderKind,
            provider::types::{PermissionDecision, PermissionOutcome},
        };

        struct Harness {
            root: std::path::PathBuf,
            state: AppState,
            conversation_id: String,
            token: String,
        }

        impl Drop for Harness {
            fn drop(&mut self) {
                let _ = std::fs::remove_dir_all(&self.root);
            }
        }

        async fn harness() -> Harness {
            let root = std::env::temp_dir().join(format!("todex-agy-hook-{}", Uuid::new_v4()));
            std::fs::create_dir_all(root.join("workspaces/project")).unwrap();
            let state = AppState::new_for_tests(Config {
                data_dir: root.join("data"),
                workspace_roots: vec![std::fs::canonicalize(root.join("workspaces")).unwrap()],
                ..Config::default()
            })
            .await
            .unwrap();
            state
                .agent_mcp
                .set_listen_addr("127.0.0.1:7345".parse().unwrap());
            let manifest = state
                .conversations
                .create_for_tests(
                    ProviderKind::Antigravity,
                    std::fs::canonicalize(root.join("workspaces/project")).unwrap(),
                )
                .await
                .unwrap();
            let token = state
                .agent_mcp
                .launch_global(&manifest.id)
                .await
                .unwrap()
                .global
                .unwrap()
                .env
                .into_iter()
                .find(|(name, _)| name == TOKEN_ENV)
                .unwrap()
                .1;
            Harness {
                root,
                state,
                conversation_id: manifest.id,
                token,
            }
        }

        impl Harness {
            /// As agy names it: without Windows' verbatim `\\?\` prefix, which
            /// the hook treats as a device path.
            fn workspace(&self) -> String {
                let path = std::fs::canonicalize(self.root.join("workspaces/project"))
                    .unwrap()
                    .display()
                    .to_string();
                match path.strip_prefix(r"\\?\") {
                    Some(local) => local.to_owned(),
                    None => path,
                }
            }

            async fn post(&self, token: Option<&str>, tool: &str, args: Value) -> (u16, Value) {
                self.post_as(token, false, tool, args).await
            }

            async fn post_as(
                &self,
                token: Option<&str>,
                native_auto: bool,
                tool: &str,
                args: Value,
            ) -> (u16, Value) {
                let mut builder = axum::http::Request::post(AGY_HOOK_ROUTE)
                    .header(header::HOST, "127.0.0.1:7345")
                    .header(header::CONTENT_TYPE, "application/json");
                if native_auto {
                    builder = builder.header(AGY_NATIVE_AUTO_HEADER, "1");
                }
                if let Some(token) = token {
                    builder = builder.header(header::AUTHORIZATION, format!("Bearer {token}"));
                }
                let body = json!({ "toolCall": { "name": tool, "args": args }, "stepIdx": 1 });
                let mut request = builder.body(Body::from(body.to_string())).unwrap();
                request
                    .extensions_mut()
                    .insert(ConnectInfo("127.0.0.1:5000".parse::<SocketAddr>().unwrap()));
                let response = crate::server::loopback_test_router(self.state.clone())
                    .oneshot(request)
                    .await
                    .unwrap();
                let status = response.status().as_u16();
                let bytes = axum::body::to_bytes(response.into_body(), 1 << 20)
                    .await
                    .unwrap();
                (
                    status,
                    serde_json::from_slice(&bytes).unwrap_or(Value::Null),
                )
            }

            async fn decide(&self, tool: &str, args: Value) -> Value {
                let (status, decision) = self.post(Some(&self.token), tool, args).await;
                assert_eq!(status, 200);
                decision
            }

            async fn pending_permission(&self) -> Value {
                // A real agy takes seconds to start and reach its tool call.
                for _ in 0..6000 {
                    let events = self
                        .state
                        .conversations
                        .history_for_tests(&self.conversation_id)
                        .await;
                    let resolved: Vec<&Value> = events
                        .iter()
                        .filter(|event| event.event_type == "permission.resolved")
                        .map(|event| &event.payload["permissionId"])
                        .collect();
                    if let Some(event) = events.iter().find(|event| {
                        event.event_type == "permission.requested"
                            && !resolved.contains(&&event.payload["permissionId"])
                    }) {
                        return event.payload.clone();
                    }
                    tokio::time::sleep(Duration::from_millis(20)).await;
                }
                let kinds: Vec<_> = self
                    .state
                    .conversations
                    .history_for_tests(&self.conversation_id)
                    .await
                    .into_iter()
                    .map(|event| (event.event_type, event.payload.get("message").cloned()))
                    .collect();
                panic!("no pending permission: {kinds:?}");
            }

            async fn answer(&self, permission: &Value, outcome: PermissionOutcome, option: &str) {
                self.state
                    .conversations
                    .resolve_permission_owned(
                        "local",
                        "dev_phone",
                        &self.conversation_id,
                        permission["permissionId"].as_str().unwrap(),
                        PermissionDecision {
                            outcome,
                            option_id: Some(option.to_owned()),
                            data: None,
                        },
                    )
                    .await
                    .unwrap();
            }
        }

        #[tokio::test]
        async fn only_local_callers_with_a_token_reach_the_gate() {
            let harness = harness().await;
            assert_eq!(harness.post(None, "view_file", Value::Null).await.0, 401);
            assert_eq!(
                harness.post(Some("nope"), "view_file", Value::Null).await.0,
                401
            );
        }

        #[tokio::test]
        async fn without_a_running_turn_everything_is_denied() {
            let harness = harness().await;
            let decision = harness.decide("view_file", Value::Null).await;
            assert_eq!(decision["decision"], "deny");
        }

        #[tokio::test]
        async fn modes_allow_deny_or_ask() {
            let harness = harness().await;
            let id = &harness.conversation_id;
            let inside = json!({ "TargetFile": format!("{}/a.txt", harness.workspace()) });
            harness
                .state
                .agent_mcp
                .record_turn_mode(id, "full-access", "implement");
            let decision = harness
                .decide("run_command", json!({ "CommandLine": "rm -rf x" }))
                .await;
            assert_eq!(decision["decision"], "allow");

            harness
                .state
                .agent_mcp
                .record_turn_mode(id, "full-access", "implement");
            assert_eq!(
                harness.decide("write_to_file", inside.clone()).await["decision"],
                "allow"
            );

            harness.state.agent_mcp.record_turn_mode(id, "ask", "plan");
            let decision = harness.decide("write_to_file", inside.clone()).await;
            assert_eq!(decision["decision"], "deny");
            assert!(decision["reason"].as_str().unwrap().contains("Plan mode"));
            assert_eq!(
                harness.decide("view_file", inside).await["decision"],
                "allow"
            );
        }

        #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
        async fn auto_defers_to_agy_only_when_agy_reviews() {
            let harness = std::sync::Arc::new(harness().await);
            let id = &harness.conversation_id;
            let command = json!({ "CommandLine": "rm -rf x" });
            harness
                .state
                .agent_mcp
                .record_turn_mode(id, "auto", "implement");
            let (status, decision) = harness
                .post_as(Some(&harness.token), true, "run_command", command.clone())
                .await;
            assert_eq!(status, 200);
            assert_eq!(decision["decision"], "defer");

            // The hint cannot loosen a turn the user put in `ask`.
            harness
                .state
                .agent_mcp
                .record_turn_mode(id, "ask", "implement");
            let pending = {
                let harness = harness.clone();
                tokio::spawn(async move {
                    harness
                        .post_as(Some(&harness.token), true, "run_command", command)
                        .await
                })
            };
            let prompt = harness.pending_permission().await;
            harness
                .answer(&prompt, PermissionOutcome::RejectOnce, "reject_once")
                .await;
            assert_eq!(pending.await.unwrap().1["decision"], "deny");
        }

        #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
        async fn ask_mode_reads_outside_the_workspace_ask_first() {
            let harness = std::sync::Arc::new(harness().await);
            let id = &harness.conversation_id;
            let outside = harness.root.join("outside");
            std::fs::create_dir_all(&outside).unwrap();
            std::fs::write(outside.join("secret.txt"), "x").unwrap();
            #[cfg(unix)]
            std::os::unix::fs::symlink(&outside, harness.root.join("workspaces/project/link"))
                .unwrap();
            harness
                .state
                .agent_mcp
                .record_turn_mode(id, "ask", "implement");
            let workspace = harness.workspace();
            for (tool, args) in [
                (
                    "view_file",
                    json!({ "AbsolutePath": format!("{workspace}/a.txt") }),
                ),
                ("list_dir", json!({ "DirectoryPath": workspace.clone() })),
                (
                    "grep_search",
                    json!({ "SearchPath": format!("{workspace}/src"), "Query": "x" }),
                ),
                (
                    "find_by_name",
                    json!({ "SearchDirectory": "src/new", "Pattern": "*" }),
                ),
                ("command_status", json!({ "CommandId": "1" })),
            ] {
                assert_eq!(
                    harness.decide(tool, args.clone()).await["decision"],
                    "allow",
                    "{tool} {args}"
                );
            }

            // Outside paths (directly, through `..`, or through a symlink),
            // and reads without a recognizable path, ask like an edit.
            #[cfg_attr(not(unix), allow(unused_mut))]
            let mut outside_reads = vec![
                json!({ "AbsolutePath": outside.join("secret.txt").display().to_string() }),
                json!({ "AbsolutePath": format!("{workspace}/../../outside/secret.txt") }),
                json!({ "AbsolutePath": format!("{workspace}/new/../../../outside/x") }),
                json!({ "toolAction": "Reading file" }),
            ];
            #[cfg(unix)]
            outside_reads.push(json!({ "AbsolutePath": format!("{workspace}/link/secret.txt") }));
            for args in outside_reads {
                let call = {
                    let harness = harness.clone();
                    let args = args.clone();
                    tokio::spawn(async move { harness.decide("view_file", args).await })
                };
                let prompt = harness.pending_permission().await;
                assert_eq!(prompt["details"]["tool_name"], "view_file", "{args}");
                harness
                    .answer(&prompt, PermissionOutcome::RejectOnce, "reject_once")
                    .await;
                assert_eq!(call.await.unwrap()["decision"], "deny", "{args}");
            }

            // Full access reads anywhere without asking.
            harness
                .state
                .agent_mcp
                .record_turn_mode(id, "full-access", "implement");
            let anywhere =
                json!({ "AbsolutePath": outside.join("secret.txt").display().to_string() });
            assert_eq!(
                harness.decide("view_file", anywhere).await["decision"],
                "allow"
            );
        }

        #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
        async fn ask_prompts_the_devices_and_remembers_always() {
            let harness = std::sync::Arc::new(harness().await);
            harness
                .state
                .agent_mcp
                .record_turn_mode(&harness.conversation_id, "ask", "implement");

            let call = {
                let harness = harness.clone();
                tokio::spawn(async move {
                    harness
                        .decide("run_command", json!({ "CommandLine": "npm test" }))
                        .await
                })
            };
            let prompt = harness.pending_permission().await;
            assert_eq!(prompt["kind"], "tool");
            assert_eq!(prompt["details"]["command"], "npm test");
            harness
                .answer(&prompt, PermissionOutcome::RejectOnce, "reject_once")
                .await;
            let decision = call.await.unwrap();
            assert_eq!(decision["decision"], "deny");
            assert!(decision["reason"]
                .as_str()
                .unwrap()
                .contains("rejected run_command"));

            let call = {
                let harness = harness.clone();
                tokio::spawn(async move {
                    harness
                        .decide("run_command", json!({ "CommandLine": "cargo build" }))
                        .await
                })
            };
            let prompt = harness.pending_permission().await;
            harness
                .answer(&prompt, PermissionOutcome::AllowAlways, "allow_always")
                .await;
            assert_eq!(call.await.unwrap()["decision"], "allow");
            // Allowed for the rest of the conversation: no new prompt.
            let decision = harness
                .decide("run_command", json!({ "CommandLine": "ls" }))
                .await;
            assert_eq!(decision["decision"], "allow");
        }

        /// Opt-in: a real `agy` turn whose tool call TodeX's installed hook
        /// sends to this test's daemon, first rejected, then approved. Needs
        /// a built daemon (`cargo build`) in `$TODEX_AGY_LIVE_DAEMON`; writes
        /// and afterwards removes TodeX's entries in `~/.gemini/config`.
        #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
        #[ignore = "opt-in: runs the real Antigravity CLI, edits ~/.gemini/config and spends quota"]
        async fn live_agy_tool_calls_wait_for_todex_approval() {
            let daemon = std::path::PathBuf::from(
                std::env::var("TODEX_AGY_LIVE_DAEMON").expect("TODEX_AGY_LIVE_DAEMON"),
            );
            let root = std::env::temp_dir().join(format!("todex-agy-live-{}", Uuid::new_v4()));
            std::fs::create_dir_all(root.join("workspaces/project")).unwrap();
            let workspace = std::fs::canonicalize(root.join("workspaces/project")).unwrap();
            let mut state = AppState::new_for_tests(Config {
                data_dir: root.join("data"),
                workspace_roots: vec![std::fs::canonicalize(root.join("workspaces")).unwrap()],
                ..Config::default()
            })
            .await
            .unwrap();
            // The hook and MCP entries run the built daemon, not this test.
            let mcp = super::super::super::AgentMcp::with_bridge_command(
                &root.join("data"),
                state.ssh.clone(),
                state.agent_desktop.clone(),
                Some(daemon),
            )
            .await
            .unwrap();
            state.agent_mcp = mcp.clone();
            state.conversations = state.conversations.clone().with_agent_mcp(mcp);
            state.agent_desktop.set_enabled(true).await.unwrap();
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            state
                .agent_mcp
                .set_listen_addr(listener.local_addr().unwrap());
            let app = crate::server::loopback_test_router(state.clone());
            tokio::spawn(async move {
                axum::serve(
                    listener,
                    app.into_make_service_with_connect_info::<SocketAddr>(),
                )
                .await
                .unwrap()
            });
            let manifest = state
                .conversations
                .create_for_tests(ProviderKind::Antigravity, workspace.clone())
                .await
                .unwrap();
            let harness = Harness {
                root: root.clone(),
                state: state.clone(),
                conversation_id: manifest.id.clone(),
                token: String::new(),
            };
            let prompt = |text: &str| crate::provider::ConversationPrompt {
                permission_mode: Some("ask".to_owned()),
                work_mode: None,
                client_request_id: None,
                text: text.to_owned(),
                model: Some("gemini-3.8-flash".to_owned()),
                reasoning_effort: Some("low".to_owned()),
                skills: Vec::new(),
                content: Vec::new(),
                permission_profile: None,
                sandbox_mode: None,
                approval_policy: None,
            };
            let turns_done = |count: usize| {
                let state = state.clone();
                let id = manifest.id.clone();
                async move {
                    for _ in 0..600 {
                        let events = state.conversations.history_for_tests(&id).await;
                        if events
                            .iter()
                            .filter(|event| {
                                matches!(
                                    event.event_type.as_str(),
                                    "turn.completed" | "turn.failed"
                                )
                            })
                            .count()
                            >= count
                        {
                            return events;
                        }
                        tokio::time::sleep(Duration::from_millis(500)).await;
                    }
                    panic!("the turn did not finish");
                }
            };
            let command_results = |events: &[crate::conversation::ConversationEvent]| {
                events
                    .iter()
                    .filter(|event| {
                        event.event_type == "tool.completed"
                            && event.payload["toolName"] == "run_command"
                    })
                    .map(|event| {
                        (
                            event.payload["isError"].clone(),
                            event.payload["result"].to_string(),
                        )
                    })
                    .collect::<Vec<_>>()
            };

            state
                .conversations
                .prompt_owned(
                    "local",
                    &manifest.id,
                    prompt("Run the shell command `echo todex-approved` with your run_command tool, then say what happened in one line."),
                )
                .await
                .unwrap();
            let asked = harness.pending_permission().await;
            assert_eq!(asked["details"]["command"], "echo todex-approved");
            harness
                .answer(&asked, PermissionOutcome::RejectOnce, "reject_once")
                .await;
            let events = turns_done(1).await;
            let rejected = command_results(&events);
            assert!(
                rejected.iter().all(|(error, _)| error == &json!(true)),
                "{rejected:?}"
            );

            state
                .conversations
                .prompt_owned(
                    "local",
                    &manifest.id,
                    prompt("Now run `echo todex-approved-again` with run_command and report its exact output."),
                )
                .await
                .unwrap();
            let asked = harness.pending_permission().await;
            harness
                .answer(&asked, PermissionOutcome::AllowOnce, "allow_once")
                .await;
            let events = turns_done(2).await;
            let results = command_results(&events);
            assert!(
                results.iter().any(|(error, output)| error == &json!(false)
                    && output.contains("todex-approved-again")),
                "{results:?}"
            );
            // (Re-asking the rejected command would hit the decline backoff.)
            // The static todex_desktop entry reached this daemon: agy cached
            // the tools it listed.
            let cached = std::env::var_os("HOME")
                .map(std::path::PathBuf::from)
                .unwrap()
                .join(".gemini/antigravity-cli/mcp/todex_desktop");
            assert!(std::fs::read_dir(&cached)
                .map(|dir| dir.count() > 0)
                .unwrap_or(false));

            crate::provider::antigravity::remove_integration(
                &crate::provider::antigravity::integration_dir(),
            )
            .unwrap();
        }
    }
}
