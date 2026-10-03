//! The `todex_ssh` MCP server: `ssh_list_hosts` and `ssh_exec`.
//!
//! Hosts must have agent access; commands run without approval through the
//! same OpenSSH wrapper config as everything else (`BatchMode`, strict host
//! keys), so agents only reach hosts the user already trusts.

use std::{
    net::SocketAddr,
    sync::Arc,
    time::{Duration, Instant},
};

use axum::{
    extract::{ConnectInfo, Request, State},
    http::{header, StatusCode},
    middleware::Next,
    response::{IntoResponse, Response},
    Router,
};
use dashmap::DashMap;
use rmcp::{
    model::{
        CallToolRequestParams, CallToolResponse, CallToolResult, ContentBlock, Implementation,
        InitializeResult, ListToolsResult, PaginatedRequestParams, ServerCapabilities, Tool,
        ToolAnnotations,
    },
    service::RequestContext,
    transport::{
        streamable_http_server::session::local::LocalSessionManager, StreamableHttpServerConfig,
        StreamableHttpService,
    },
    ErrorData, RoleServer, ServerHandler,
};
use serde::Deserialize;
use serde_json::{json, Value};
use tokio::sync::{mpsc, Semaphore};
use uuid::Uuid;

use super::{AgentMcp, SSH_ROUTE as ROUTE, SSH_SERVER as SERVER_NAME};
use crate::{
    app_state::AppState,
    external_command::{
        self, bounded_text, prepare_captured, CommandLimits, ExternalCommandError, OutputChunk,
        OutputStream,
    },
    local_terminal::decode_terminal_output,
    provider::ConversationSupervisor,
    ssh::{classify_failure, SshFailureKind, SshMode},
};

const DEFAULT_TIMEOUT_SECONDS: u64 = 60;
pub(super) const MAX_TIMEOUT_SECONDS: u64 = 600;
/// Per stream; more is dropped and reported as `truncated`.
const OUTPUT_LIMIT: usize = 256 * 1024;
const MAX_COMMAND_BYTES: usize = 64 * 1024;
const MAX_CWD_BYTES: usize = 4096;
const MAX_STDIN_BYTES: usize = 1024 * 1024;
const PER_HOST_CONCURRENCY: usize = 4;
/// Commands are journaled for the conversation; keep the event small.
const EVENT_COMMAND_LIMIT: usize = 4096;
/// Output recorded per stream and call in `ssh.exec.output` events, so many
/// calls cannot fill the conversation journal. The agent still receives up
/// to [`OUTPUT_LIMIT`].
const EVENT_OUTPUT_LIMIT: usize = 64 * 1024;
/// Live output is batched into events at this cadence or size.
const OUTPUT_FLUSH_INTERVAL: Duration = Duration::from_millis(100);
const OUTPUT_FLUSH_BYTES: usize = 16 * 1024;
const ERROR_DETAIL_LIMIT: usize = 4096;

/// The conversation a request was authenticated for.
#[derive(Clone, Debug)]
pub(super) struct Caller {
    pub(super) conversation_id: String,
}

pub(crate) fn routes(state: &AppState) -> Router<AppState> {
    let tools = SshTools {
        mcp: state.agent_mcp.clone(),
        conversations: state.conversations.clone(),
        host_slots: Arc::new(DashMap::new()),
    };
    // Defaults: loopback-only `Host` validation (DNS rebinding), sessions
    // with an idle timeout, which the bridge transparently re-initializes.
    let service = StreamableHttpService::new(
        move || Ok(tools.clone()),
        Arc::new(LocalSessionManager::default()),
        StreamableHttpServerConfig::default(),
    );
    Router::new()
        .route_service(ROUTE, service)
        .route_layer(axum::middleware::from_fn_with_state(state.clone(), guard))
}

/// Only local, non-browser callers holding a conversation token get through.
/// This route deliberately sits outside device auth: the agents calling it
/// run on this machine and never hold a device key.
pub(super) async fn guard(
    State(state): State<AppState>,
    mut request: Request,
    next: Next,
) -> Response {
    let loopback = request
        .extensions()
        .get::<ConnectInfo<SocketAddr>>()
        .is_some_and(|ConnectInfo(peer)| peer.ip().to_canonical().is_loopback());
    // Browsers always send `Origin` on cross-origin POSTs; agents never do.
    if !loopback || request.headers().contains_key(header::ORIGIN) {
        return (StatusCode::FORBIDDEN, "local agents only").into_response();
    }
    let caller = request
        .headers()
        .get(header::AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.strip_prefix("Bearer "))
        .and_then(|token| state.agent_mcp.authenticate(token.trim()));
    let Some(conversation_id) = caller else {
        return (
            StatusCode::UNAUTHORIZED,
            [(header::WWW_AUTHENTICATE, "Bearer")],
            "invalid agent token",
        )
            .into_response();
    };
    request.extensions_mut().insert(Caller { conversation_id });
    next.run(request).await
}

#[derive(Clone)]
struct SshTools {
    mcp: AgentMcp,
    conversations: ConversationSupervisor,
    /// Alias → slots; bounds concurrent agent commands per host.
    host_slots: Arc<DashMap<String, Arc<Semaphore>>>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct ExecArgs {
    host: String,
    command: String,
    #[serde(default)]
    cwd: Option<String>,
    #[serde(default)]
    stdin: Option<String>,
    #[serde(default)]
    timeout_sec: Option<u64>,
}

pub(super) fn schema(value: Value) -> Arc<serde_json::Map<String, Value>> {
    match value {
        Value::Object(map) => Arc::new(map),
        _ => unreachable!("tool schemas are objects"),
    }
}

fn tools() -> Vec<Tool> {
    vec![
        Tool::new(
            "ssh_list_hosts",
            "List the SSH hosts the user allowed agents to use. Only these aliases work with ssh_exec.",
            schema(json!({ "type": "object", "properties": {}, "additionalProperties": false })),
        )
        .with_annotations(ToolAnnotations::new().read_only(true).open_world(false)),
        Tool::new(
            "ssh_exec",
            "Run a shell command on an allowed SSH host (non-interactive, no TTY, no password prompts). \
             Returns stdout, stderr and the exit code; a non-zero exit code is a normal result. \
             stdout and stderr are each capped at 256 KiB (truncated=true when cut).",
            schema(json!({
                "type": "object",
                "properties": {
                    "host": { "type": "string", "description": "Host alias from ssh_list_hosts." },
                    "command": { "type": "string", "description": "Command line for the remote login shell." },
                    "cwd": { "type": "string", "description": "Remote working directory; the command runs after `cd` into it." },
                    "stdin": { "type": "string", "description": "Text written to the command's standard input (max 1 MiB)." },
                    "timeoutSec": { "type": "integer", "minimum": 1, "maximum": MAX_TIMEOUT_SECONDS, "description": "Kill the command after this many seconds (default 60)." }
                },
                "required": ["host", "command"],
                "additionalProperties": false
            })),
        )
        .with_annotations(
            ToolAnnotations::new()
                .read_only(false)
                .destructive(true)
                .open_world(true),
        ),
    ]
}

impl ServerHandler for SshTools {
    fn get_info(&self) -> InitializeResult {
        InitializeResult::new(ServerCapabilities::builder().enable_tools().build())
            .with_server_info(Implementation::new(
                SERVER_NAME,
                crate::version::APP_VERSION,
            ))
            .with_instructions(
                "Runs commands on the user's SSH hosts. Call ssh_list_hosts first; \
                 only hosts listed there can be used.",
            )
    }

    async fn list_tools(
        &self,
        _request: Option<PaginatedRequestParams>,
        _context: RequestContext<RoleServer>,
    ) -> Result<ListToolsResult, ErrorData> {
        Ok(ListToolsResult::with_all_items(tools()))
    }

    async fn call_tool(
        &self,
        request: CallToolRequestParams,
        context: RequestContext<RoleServer>,
    ) -> Result<CallToolResponse, ErrorData> {
        let caller = context
            .extensions
            .get::<axum::http::request::Parts>()
            .and_then(|parts| parts.extensions.get::<Caller>())
            .cloned()
            .ok_or_else(|| ErrorData::internal_error("request is not authenticated", None))?;
        let arguments = Value::Object(request.arguments.unwrap_or_default());
        let result = match request.name.as_ref() {
            "ssh_list_hosts" => self.list_hosts().await,
            "ssh_exec" => match serde_json::from_value::<ExecArgs>(arguments) {
                Ok(args) => self.exec(&caller, args, &context).await,
                Err(error) => tool_error(format!("invalid ssh_exec arguments: {error}")),
            },
            other => {
                return Err(ErrorData::invalid_params(
                    format!("unknown tool {other}"),
                    None,
                ))
            }
        };
        Ok(result.into())
    }
}

impl SshTools {
    async fn list_hosts(&self) -> CallToolResult {
        match self.mcp.ssh().agent_hosts().await {
            Ok(hosts) => CallToolResult::structured(json!({
                "hosts": hosts.into_iter().map(|host| {
                    let resolved = host.resolved.unwrap_or_default();
                    json!({
                        "alias": host.alias,
                        "hostName": resolved.host_name,
                        "user": resolved.user,
                        "port": resolved.port,
                    })
                }).collect::<Vec<_>>(),
            })),
            Err(error) => tool_error(format!("cannot list SSH hosts: {error}")),
        }
    }

    async fn exec(
        &self,
        caller: &Caller,
        args: ExecArgs,
        context: &RequestContext<RoleServer>,
    ) -> CallToolResult {
        let request = match validate(args) {
            Ok(request) => request,
            Err(message) => return tool_error(message),
        };
        match self.mcp.ssh().is_agent_host(&request.host).await {
            Ok(true) => {}
            Ok(false) => {
                return tool_error(format!(
                    "SSH host {} is not available to agents. Use a host from ssh_list_hosts; \
                     the user can grant access per host in TodeX SSH settings.",
                    request.host
                ))
            }
            Err(error) => return tool_error(format!("cannot list SSH hosts: {error}")),
        }
        let slots = self
            .host_slots
            .entry(request.host.clone())
            .or_insert_with(|| Arc::new(Semaphore::new(PER_HOST_CONCURRENCY)))
            .clone();
        let _slot = tokio::select! {
            slot = slots.acquire_owned() => match slot {
                Ok(slot) => slot,
                Err(_) => return tool_error("SSH host is shutting down".to_owned()),
            },
            () = context.ct.cancelled() => return tool_error("cancelled".to_owned()),
        };

        let exec_id = format!("sshx_{}", Uuid::new_v4().simple());
        let mut started = json!({
            "execId": exec_id,
            "host": request.host,
            "command": bounded_text(request.command.as_bytes(), EVENT_COMMAND_LIMIT),
        });
        if let Some(cwd) = &request.cwd {
            started["cwd"] = json!(cwd);
        }
        // No audit trail, no command: a conversation that cannot record the
        // call (deleted, journal full) does not get to run it.
        if let Err(error) = self
            .conversations
            .append_agent_event(&caller.conversation_id, "ssh.exec.started", started)
            .await
        {
            return tool_error(format!("cannot record the SSH command: {error}"));
        }

        let begun = Instant::now();
        let (tap, chunks) = mpsc::unbounded_channel();
        let run = async {
            tokio::select! {
                outcome = self.run(&request, tap) => outcome,
                // Dropping the run kills ssh and its process group; it also
                // drops the tap, which ends the recorder below.
                () = context.ct.cancelled() => ExecOutcome::Cancelled,
            }
        };
        let recorder = OutputRecorder {
            conversations: &self.conversations,
            conversation_id: &caller.conversation_id,
            exec_id: &exec_id,
        };
        let (outcome, output_truncated) = tokio::join!(run, recorder.record(chunks));
        let duration_ms = u64::try_from(begun.elapsed().as_millis()).unwrap_or(u64::MAX);
        let (result, mut completed) = outcome.into_result(&request.host, duration_ms);
        completed["execId"] = json!(exec_id);
        completed["outputTruncated"] = json!(output_truncated);
        if let Err(error) = self
            .conversations
            .append_agent_event(&caller.conversation_id, "ssh.exec.completed", completed)
            .await
        {
            tracing::warn!(
                conversation_id = caller.conversation_id,
                %error,
                "failed to record an SSH command result"
            );
        }
        result
    }

    async fn run(
        &self,
        request: &ExecRequest,
        tap: mpsc::UnboundedSender<OutputChunk>,
    ) -> ExecOutcome {
        let mut command = self.mcp.ssh().command(SshMode::Batch);
        command
            .arg("--")
            .arg(&request.host)
            .arg(remote_command(&request.command, request.cwd.as_deref()));
        prepare_captured(&mut command, request.stdin.is_some());
        let limits = CommandLimits {
            timeout: request.timeout,
            output_limit: OUTPUT_LIMIT,
        };
        match external_command::run_streaming(
            command,
            request.stdin.clone().map(String::into_bytes),
            limits,
            tap,
        )
        .await
        {
            Ok(output) => ExecOutcome::Finished(output),
            Err(ExternalCommandError::TimedOut) => ExecOutcome::TimedOut(request.timeout),
            Err(ExternalCommandError::NotFound) => {
                ExecOutcome::NotStarted("ssh executable not found on the TodeX host".to_owned())
            }
            Err(error) => ExecOutcome::NotStarted(error.to_string()),
        }
    }
}

/// Turns the live output of one `ssh_exec` into batched `ssh.exec.output`
/// events so clients can follow each call in its own view.
struct OutputRecorder<'a> {
    conversations: &'a ConversationSupervisor,
    conversation_id: &'a str,
    exec_id: &'a str,
}

#[derive(Default)]
struct RecordedStream {
    /// Bytes of an incomplete UTF-8 sequence carried to the next chunk.
    pending: Vec<u8>,
    text: String,
    recorded: usize,
    truncated: bool,
}

impl RecordedStream {
    fn accept(&mut self, bytes: &[u8]) {
        let room = EVENT_OUTPUT_LIMIT - self.recorded;
        let taken = bytes.len().min(room);
        if taken < bytes.len() {
            self.truncated = true;
        }
        if taken == 0 {
            return;
        }
        self.recorded += taken;
        let text = decode_terminal_output(&mut self.pending, &bytes[..taken], false);
        self.text.push_str(&text);
    }

    fn finish(&mut self) {
        let text = decode_terminal_output(&mut self.pending, &[], true);
        self.text.push_str(&text);
    }
}

impl OutputRecorder<'_> {
    /// Records until the tap closes; returns whether any output was cut.
    async fn record(self, mut chunks: mpsc::UnboundedReceiver<OutputChunk>) -> bool {
        let mut streams = [RecordedStream::default(), RecordedStream::default()];
        let mut writable = true;
        let mut ticker = tokio::time::interval(OUTPUT_FLUSH_INTERVAL);
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            tokio::select! {
                chunk = chunks.recv() => match chunk {
                    Some(chunk) => {
                        let index = stream_index(chunk.stream);
                        streams[index].accept(&chunk.bytes);
                        if streams[index].text.len() >= OUTPUT_FLUSH_BYTES {
                            self.flush(&mut streams[index], index, &mut writable).await;
                        }
                    }
                    None => break,
                },
                _ = ticker.tick() => {
                    for (index, stream) in streams.iter_mut().enumerate() {
                        self.flush(stream, index, &mut writable).await;
                    }
                }
            }
        }
        for (index, stream) in streams.iter_mut().enumerate() {
            stream.finish();
            self.flush(stream, index, &mut writable).await;
        }
        streams.iter().any(|stream| stream.truncated)
    }

    async fn flush(&self, stream: &mut RecordedStream, index: usize, writable: &mut bool) {
        if stream.text.is_empty() {
            return;
        }
        let data = std::mem::take(&mut stream.text);
        if !*writable {
            return;
        }
        let payload = json!({
            "execId": self.exec_id,
            "stream": if index == 0 { "stdout" } else { "stderr" },
            "data": data,
        });
        if let Err(error) = self
            .conversations
            .append_agent_event(self.conversation_id, "ssh.exec.output", payload)
            .await
        {
            // The command keeps running for the agent; only the view stops.
            tracing::warn!(
                conversation_id = self.conversation_id,
                %error,
                "failed to record SSH command output"
            );
            *writable = false;
        }
    }
}

fn stream_index(stream: OutputStream) -> usize {
    match stream {
        OutputStream::Stdout => 0,
        OutputStream::Stderr => 1,
    }
}

struct ExecRequest {
    host: String,
    command: String,
    cwd: Option<String>,
    stdin: Option<String>,
    timeout: Duration,
}

fn validate(args: ExecArgs) -> Result<ExecRequest, String> {
    let host = args.host.trim().to_owned();
    if host.is_empty() {
        return Err("host is required".to_owned());
    }
    if args.command.trim().is_empty() {
        return Err("command is required".to_owned());
    }
    if args.command.len() > MAX_COMMAND_BYTES || args.command.contains('\0') {
        return Err(format!(
            "command must be at most {MAX_COMMAND_BYTES} bytes without NUL characters"
        ));
    }
    let cwd = args.cwd.filter(|cwd| !cwd.is_empty());
    if cwd
        .as_ref()
        .is_some_and(|cwd| cwd.len() > MAX_CWD_BYTES || cwd.chars().any(char::is_control))
    {
        return Err(format!(
            "cwd must be at most {MAX_CWD_BYTES} bytes without control characters"
        ));
    }
    if args
        .stdin
        .as_ref()
        .is_some_and(|stdin| stdin.len() > MAX_STDIN_BYTES)
    {
        return Err(format!("stdin must be at most {MAX_STDIN_BYTES} bytes"));
    }
    let timeout = match args.timeout_sec {
        None => DEFAULT_TIMEOUT_SECONDS,
        Some(seconds @ 1..=MAX_TIMEOUT_SECONDS) => seconds,
        Some(_) => {
            return Err(format!(
                "timeoutSec must be between 1 and {MAX_TIMEOUT_SECONDS}"
            ))
        }
    };
    Ok(ExecRequest {
        host,
        command: args.command,
        cwd,
        stdin: args.stdin,
        timeout: Duration::from_secs(timeout),
    })
}

/// The remote shell line: `cd '<cwd>' || exit 1` then `<command>`. The command is the
/// agent's shell input by design; only the directory needs quoting. POSIX
/// quoting, so `cwd` assumes a POSIX login shell on the remote side.
fn remote_command(command: &str, cwd: Option<&str>) -> String {
    match cwd {
        // `&&` would bind only to the first command of a list (`a; b`), so a
        // failed `cd` must end the shell before any of the command runs.
        Some(cwd) => format!("cd {} || exit 1\n{command}", shell_quote(cwd)),
        None => command.to_owned(),
    }
}

fn shell_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', r"'\''"))
}

enum ExecOutcome {
    Finished(external_command::TruncatedOutput),
    TimedOut(Duration),
    NotStarted(String),
    Cancelled,
}

/// `ssh` reserves exit status 255 for its own failures.
const SSH_ERROR_STATUS: i32 = 255;

impl ExecOutcome {
    /// The tool result and the `ssh.exec.completed` payload.
    fn into_result(self, host: &str, duration_ms: u64) -> (CallToolResult, Value) {
        let mut completed = json!({ "host": host, "durationMs": duration_ms, "truncated": false });
        let result = match self {
            Self::Finished(output) => {
                let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
                let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
                let exit_code = output.status.code();
                completed["exitCode"] = json!(exit_code);
                completed["truncated"] = json!(output.truncated);
                match exit_code {
                    Some(code) if code != SSH_ERROR_STATUS => CallToolResult::structured(json!({
                        "stdout": stdout,
                        "stderr": stderr,
                        "exitCode": code,
                        "truncated": output.truncated,
                        "durationMs": duration_ms,
                    })),
                    _ => {
                        let failure = if exit_code.is_some() {
                            classify_failure(&stderr)
                        } else {
                            SshFailureKind::Other
                        };
                        completed["failure"] = json!(failure);
                        let message = match exit_code {
                            Some(_) => format!("ssh to {host} failed"),
                            None => format!("ssh to {host} was terminated by a signal"),
                        };
                        let mut error = json!({
                            "failure": failure,
                            "message": message,
                            "stdout": stdout,
                            "stderr": bounded_text(stderr.as_bytes(), ERROR_DETAIL_LIMIT),
                            "exitCode": exit_code,
                            "truncated": output.truncated,
                            "durationMs": duration_ms,
                        });
                        if let Some(hint) = failure_hint(failure) {
                            error["hint"] = json!(hint);
                        }
                        CallToolResult::structured_error(error)
                    }
                }
            }
            Self::TimedOut(timeout) => {
                completed["failure"] = json!(SshFailureKind::TimedOut);
                CallToolResult::structured_error(json!({
                    "failure": SshFailureKind::TimedOut,
                    "message": format!("the command on {host} did not finish within {}s and was killed", timeout.as_secs()),
                    "durationMs": duration_ms,
                }))
            }
            Self::NotStarted(detail) => {
                completed["failure"] = json!(SshFailureKind::Other);
                CallToolResult::structured_error(json!({
                    "failure": SshFailureKind::Other,
                    "message": format!("ssh could not run: {detail}"),
                    "durationMs": duration_ms,
                }))
            }
            Self::Cancelled => {
                completed["failure"] = json!("cancelled");
                tool_error("cancelled".to_owned())
            }
        };
        (result, completed)
    }
}

/// What the agent (and the user reading along) should do about a failure.
fn failure_hint(failure: SshFailureKind) -> Option<&'static str> {
    match failure {
        SshFailureKind::HostKeyUnverified => Some(
            "The host key is not trusted yet. Ask the user to connect once from the TodeX Terminal view to confirm it.",
        ),
        SshFailureKind::HostKeyChanged => Some(
            "The host key changed since it was trusted (reinstalled host or an attack). Ask the user to verify it; do not retry.",
        ),
        SshFailureKind::AuthenticationFailed => Some(
            "Key/agent authentication failed. For password-only hosts, ask the user to log in from the TodeX Terminal view; that connection is reused for about 5 minutes.",
        ),
        SshFailureKind::Unreachable => Some("The host could not be reached from the TodeX machine."),
        SshFailureKind::TimedOut => Some("Connecting to the host timed out."),
        SshFailureKind::Other => None,
    }
}

pub(super) fn tool_error(message: String) -> CallToolResult {
    CallToolResult::error(vec![ContentBlock::text(message)])
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use crate::app_state::AppState;
    use crate::config::Config;
    use crate::conversation::ProviderKind;
    use axum::body::Body;
    use std::os::unix::fs::PermissionsExt;
    use std::path::PathBuf;
    use tower::ServiceExt;

    #[test]
    fn recorded_output_keeps_split_utf8_and_stops_at_the_limit() {
        let mut stream = RecordedStream::default();
        let text = "你好";
        stream.accept(&text.as_bytes()[..2]);
        assert_eq!(stream.text, "");
        stream.accept(&text.as_bytes()[2..]);
        assert_eq!(stream.text, "你好");

        let mut stream = RecordedStream::default();
        stream.accept(&vec![b'a'; EVENT_OUTPUT_LIMIT - 1]);
        stream.accept(b"bc");
        stream.finish();
        assert!(stream.truncated);
        assert_eq!(stream.recorded, EVENT_OUTPUT_LIMIT);
        assert!(stream.text.ends_with("ab"));
    }

    #[test]
    fn quotes_the_working_directory_for_posix_shells() {
        assert_eq!(remote_command("ls -la", None), "ls -la");
        assert_eq!(
            remote_command("make", Some("/srv/it's here")),
            "cd '/srv/it'\\''s here' || exit 1\nmake"
        );
        assert_eq!(
            remote_command("pwd", Some("$(rm -rf /)")),
            "cd '$(rm -rf /)' || exit 1\npwd"
        );
    }

    #[test]
    fn validates_exec_arguments() {
        let args = |value: Value| serde_json::from_value::<ExecArgs>(value).unwrap();
        let ok = validate(args(json!({ "host": "web", "command": "ls" }))).unwrap();
        assert_eq!(ok.timeout, Duration::from_secs(60));
        assert!(validate(args(json!({ "host": "web", "command": " " }))).is_err());
        assert!(validate(args(
            json!({ "host": "web", "command": "ls", "timeoutSec": 601 })
        ))
        .is_err());
        assert!(validate(args(
            json!({ "host": "web", "command": "ls", "timeoutSec": 0 })
        ))
        .is_err());
        assert!(validate(args(
            json!({ "host": "web", "command": "ls", "cwd": "a\nb" })
        ))
        .is_err());
        assert!(serde_json::from_value::<ExecArgs>(json!({ "host": "web" })).is_err());
    }

    struct Harness {
        root: PathBuf,
        state: AppState,
        conversation_id: String,
        token: String,
    }

    impl Drop for Harness {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.root);
        }
    }

    /// A daemon state whose `ssh` is a fake that answers `-G`, logs argv and
    /// stdin, and runs nothing remotely. Hosts `bad*` fail host-key checks;
    /// `slow*` hang; `big*` print 300 KiB; others echo their command line.
    async fn harness() -> Harness {
        let root = std::env::temp_dir().join(format!("todex-ssh-mcp-{}", uuid::Uuid::new_v4()));
        let home = root.join("home");
        let workspace = root.join("workspaces/project");
        std::fs::create_dir_all(home.join(".ssh")).unwrap();
        std::fs::create_dir_all(&workspace).unwrap();
        std::fs::write(home.join(".ssh/config"), "Host web bad1 slow1 big1 off\n").unwrap();
        let fake = root.join("fake-ssh");
        std::fs::write(
            &fake,
            format!(
                r#"#!/bin/sh
printf '%s\n' "$*" >> "{log}"
last=""; for arg in "$@"; do prev="$last"; last="$arg"; done
case " $* " in
  *" -G "*) printf 'hostname %s.example\nuser ops\nport 2222\nidentityfile ~/.ssh/id_ed25519\n' "$last"; exit 0;;
esac
case "$prev" in
  bad*) echo "Host key verification failed." >&2; exit 255;;
  slow*) sleep 30; exit 0;;
  big*) head -c 300000 /dev/zero | tr '\0' x; exit 0;;
esac
printf 'ran:%s\n' "$last"
if [ ! -t 0 ]; then cat; fi
echo warn >&2
exit 3
"#,
                log = root.join("ssh.log").display()
            ),
        )
        .unwrap();
        std::fs::set_permissions(&fake, std::fs::Permissions::from_mode(0o755)).unwrap();

        let config = Config {
            data_dir: root.join("data"),
            workspace_roots: vec![std::fs::canonicalize(root.join("workspaces")).unwrap()],
            ..Config::default()
        };
        let mut state = AppState::new(config).await.unwrap();
        state.ssh = crate::ssh::SshService::with_home(
            &root.join("data"),
            fake.display().to_string(),
            Some(home),
        )
        .await
        .unwrap();
        state.agent_mcp = super::super::tests::registry(state.ssh.clone(), &root).await;
        for alias in ["web", "bad1", "slow1", "big1"] {
            state.ssh.set_agent_access(alias, true).await.unwrap();
        }
        let manifest = state
            .conversations
            .create_for_tests(
                ProviderKind::Codex,
                std::fs::canonicalize(&workspace).unwrap(),
            )
            .await
            .unwrap();
        let token = state
            .agent_mcp
            .launch(&manifest.id)
            .await
            .unwrap()
            .servers
            .remove(0)
            .env
            .into_iter()
            .find(|(name, _)| name == super::super::TOKEN_ENV)
            .unwrap()
            .1;
        Harness {
            root,
            state,
            conversation_id: manifest.id,
            token,
        }
    }

    fn mcp_request(peer: &str, token: Option<&str>, body: Value) -> Request {
        let mut builder = axum::http::Request::post(ROUTE)
            .header(header::HOST, "127.0.0.1:7345")
            .header(header::CONTENT_TYPE, "application/json")
            .header(header::ACCEPT, "application/json, text/event-stream");
        if let Some(token) = token {
            builder = builder.header(header::AUTHORIZATION, format!("Bearer {token}"));
        }
        let mut request = builder.body(Body::from(body.to_string())).unwrap();
        request
            .extensions_mut()
            .insert(ConnectInfo(peer.parse::<SocketAddr>().unwrap()));
        request
    }

    fn initialize() -> Value {
        json!({
            "jsonrpc": "2.0", "id": 1, "method": "initialize",
            "params": { "protocolVersion": "2025-06-18", "capabilities": {}, "clientInfo": { "name": "t", "version": "1" } }
        })
    }

    #[tokio::test]
    async fn route_rejects_remote_peers_browsers_and_bad_tokens() {
        let harness = harness().await;
        let app = || crate::server::router(harness.state.clone());
        let status = |request: Request| async { app().oneshot(request).await.unwrap().status() };

        assert_eq!(
            status(mcp_request(
                "192.168.1.9:5000",
                Some(&harness.token),
                initialize()
            ))
            .await,
            StatusCode::FORBIDDEN
        );
        let mut browser = mcp_request("127.0.0.1:5000", Some(&harness.token), initialize());
        browser
            .headers_mut()
            .insert(header::ORIGIN, "https://evil.example".parse().unwrap());
        assert_eq!(status(browser).await, StatusCode::FORBIDDEN);
        assert_eq!(
            status(mcp_request("127.0.0.1:5000", None, initialize())).await,
            StatusCode::UNAUTHORIZED
        );
        assert_eq!(
            status(mcp_request("127.0.0.1:5000", Some("nope"), initialize())).await,
            StatusCode::UNAUTHORIZED
        );
        // No device signature needed; IPv4-mapped loopback counts as local.
        assert_eq!(
            status(mcp_request(
                "[::ffff:127.0.0.1]:5000",
                Some(&harness.token),
                initialize()
            ))
            .await,
            StatusCode::OK
        );
        harness
            .state
            .agent_mcp
            .revoke(&harness.conversation_id)
            .await;
        assert_eq!(
            status(mcp_request(
                "127.0.0.1:5000",
                Some(&harness.token),
                initialize()
            ))
            .await,
            StatusCode::UNAUTHORIZED
        );
    }

    /// Serves the real router on a loopback port and returns an rmcp client
    /// connected through the stdio bridge.
    async fn bridged_client(
        harness: &Harness,
    ) -> rmcp::service::RunningService<rmcp::RoleClient, rmcp::model::ClientInfo> {
        use rmcp::ServiceExt;
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let app = crate::server::router(harness.state.clone());
        tokio::spawn(async move {
            axum::serve(
                listener,
                app.into_make_service_with_connect_info::<SocketAddr>(),
            )
            .await
            .unwrap();
        });
        let (client_side, bridge_side) = tokio::io::duplex(1 << 20);
        let (bridge_read, bridge_write) = tokio::io::split(bridge_side);
        let url = format!("http://{addr}{ROUTE}");
        let token = harness.token.clone();
        tokio::spawn(async move {
            super::super::bridge::proxy(&url, &token, bridge_read, bridge_write)
                .await
                .unwrap();
        });
        let (read, write) = tokio::io::split(client_side);
        rmcp::model::ClientInfo::default()
            .serve((read, write))
            .await
            .unwrap()
    }

    fn structured(result: &CallToolResult) -> &Value {
        result.structured_content.as_ref().unwrap()
    }

    async fn call(
        client: &rmcp::service::RunningService<rmcp::RoleClient, rmcp::model::ClientInfo>,
        name: &'static str,
        arguments: Value,
    ) -> CallToolResult {
        let Value::Object(arguments) = arguments else {
            unreachable!()
        };
        client
            .call_tool(CallToolRequestParams::new(name).with_arguments(arguments))
            .await
            .unwrap()
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn bridge_exposes_tools_and_exec_records_events() {
        let harness = harness().await;
        let client = bridged_client(&harness).await;

        let tools = client.list_tools(None).await.unwrap().tools;
        let names: Vec<_> = tools.iter().map(|tool| tool.name.as_ref()).collect();
        assert_eq!(names, ["ssh_list_hosts", "ssh_exec"]);

        let hosts = call(&client, "ssh_list_hosts", json!({})).await;
        let hosts = structured(&hosts)["hosts"].as_array().unwrap().clone();
        assert_eq!(hosts.len(), 4, "{hosts:?}");
        let web = hosts.iter().find(|host| host["alias"] == "web").unwrap();
        assert_eq!(
            web,
            &json!({ "alias": "web", "hostName": "web.example", "user": "ops", "port": 2222 })
        );

        let result = call(
            &client,
            "ssh_exec",
            json!({ "host": "web", "command": "uname -a", "cwd": "/srv/a b", "stdin": "input-data" }),
        )
        .await;
        assert_eq!(result.is_error, Some(false));
        let output = structured(&result);
        assert_eq!(output["exitCode"], 3, "non-zero exit is a normal result");
        assert_eq!(
            output["stdout"],
            "ran:cd '/srv/a b' || exit 1\nuname -a\ninput-data"
        );
        assert_eq!(output["stderr"], "warn\n");
        assert_eq!(output["truncated"], false);
        let log = std::fs::read_to_string(harness.root.join("ssh.log")).unwrap();
        let line = log
            .lines()
            .find(|line| line.ends_with("|| exit 1"))
            .unwrap();
        assert!(line.contains("-o BatchMode=yes -o StrictHostKeyChecking=yes"));
        assert!(log.contains("-- web cd '/srv/a b' || exit 1\nuname -a"));

        let big = call(
            &client,
            "ssh_exec",
            json!({ "host": "big1", "command": "dump" }),
        )
        .await;
        assert_eq!(big.is_error, Some(false));
        assert_eq!(structured(&big)["truncated"], true);
        assert_eq!(
            structured(&big)["stdout"].as_str().unwrap().len(),
            OUTPUT_LIMIT
        );

        let failed = call(
            &client,
            "ssh_exec",
            json!({ "host": "bad1", "command": "ls" }),
        )
        .await;
        assert_eq!(failed.is_error, Some(true));
        assert_eq!(structured(&failed)["failure"], "hostKeyUnverified");
        assert!(structured(&failed)["hint"]
            .as_str()
            .unwrap()
            .contains("TodeX Terminal"));

        let timed_out = call(
            &client,
            "ssh_exec",
            json!({ "host": "slow1", "command": "sleep", "timeoutSec": 1 }),
        )
        .await;
        assert_eq!(timed_out.is_error, Some(true));
        assert_eq!(structured(&timed_out)["failure"], "timedOut");

        // A host without agent access is refused before ssh runs.
        let refused = call(
            &client,
            "ssh_exec",
            json!({ "host": "off", "command": "ls" }),
        )
        .await;
        assert_eq!(refused.is_error, Some(true));
        let log = std::fs::read_to_string(harness.root.join("ssh.log")).unwrap();
        assert!(!log.lines().any(|line| line.contains("-- off ")));

        let history = harness
            .state
            .conversations
            .history_for_tests(&harness.conversation_id)
            .await;
        let started: Vec<_> = history
            .iter()
            .filter(|event| event.event_type == "ssh.exec.started")
            .map(|event| event.payload.clone())
            .collect();
        let completed: Vec<_> = history
            .iter()
            .filter(|event| event.event_type == "ssh.exec.completed")
            .map(|event| event.payload.clone())
            .collect();
        assert_eq!(started.len(), 4);
        assert_eq!(completed.len(), 4);
        let exec_id = started[0]["execId"].as_str().unwrap().to_owned();
        assert!(exec_id.starts_with("sshx_"));
        assert_eq!(
            started[0],
            json!({ "execId": exec_id, "host": "web", "command": "uname -a", "cwd": "/srv/a b" })
        );
        let ids: std::collections::HashSet<_> = started
            .iter()
            .map(|event| event["execId"].clone())
            .collect();
        assert_eq!(ids.len(), 4, "every call has its own id");
        for (start, end) in started.iter().zip(&completed) {
            assert_eq!(start["execId"], end["execId"]);
        }

        // Live output, per call and stream, in order and before completion.
        let output = |id: &Value, stream: &str| -> String {
            history
                .iter()
                .filter(|event| event.event_type == "ssh.exec.output")
                .filter(|event| &event.payload["execId"] == id && event.payload["stream"] == stream)
                .map(|event| event.payload["data"].as_str().unwrap().to_owned())
                .collect()
        };
        assert_eq!(
            output(&started[0]["execId"], "stdout"),
            "ran:cd '/srv/a b' || exit 1\nuname -a\ninput-data"
        );
        assert_eq!(output(&started[0]["execId"], "stderr"), "warn\n");
        let first_completed = history
            .iter()
            .position(|event| event.event_type == "ssh.exec.completed")
            .unwrap();
        assert!(history[..first_completed]
            .iter()
            .any(|event| event.event_type == "ssh.exec.output"));
        assert_eq!(completed[0]["outputTruncated"], false);
        // The agent got 256 KiB; the recorded view stops at 64 KiB.
        assert_eq!(
            output(&started[1]["execId"], "stdout").len(),
            EVENT_OUTPUT_LIMIT
        );
        assert_eq!(completed[1]["outputTruncated"], true);
        assert_eq!(completed[0]["exitCode"], 3);
        assert_eq!(completed[0]["truncated"], false);
        assert!(completed[0].get("failure").is_none());
        assert_eq!(completed[1]["truncated"], true);
        assert_eq!(completed[2]["failure"], "hostKeyUnverified");
        assert_eq!(completed[3]["failure"], "timedOut");

        client.cancel().await.unwrap();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn bridge_fails_clearly_with_a_revoked_token() {
        let harness = harness().await;
        harness
            .state
            .agent_mcp
            .revoke(&harness.conversation_id)
            .await;
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let app = crate::server::router(harness.state.clone());
        tokio::spawn(async move {
            axum::serve(
                listener,
                app.into_make_service_with_connect_info::<SocketAddr>(),
            )
            .await
            .unwrap();
        });
        let (client_side, bridge_side) = tokio::io::duplex(1 << 16);
        let (bridge_read, bridge_write) = tokio::io::split(bridge_side);
        let url = format!("http://{addr}{ROUTE}");
        let token = harness.token.clone();
        let bridge = tokio::spawn(async move {
            super::super::bridge::proxy(&url, &token, bridge_read, bridge_write).await
        });
        use rmcp::ServiceExt;
        let (read, write) = tokio::io::split(client_side);
        let client = tokio::time::timeout(
            Duration::from_secs(10),
            rmcp::model::ClientInfo::default().serve((read, write)),
        )
        .await
        .expect("initialize must not hang");
        assert!(client.is_err());
        let error = tokio::time::timeout(Duration::from_secs(10), bridge)
            .await
            .unwrap()
            .unwrap()
            .unwrap_err();
        assert!(error.to_string().contains("TodeX"), "{error}");
    }

    /// Manual check against the installed Codex CLI, without a model turn:
    /// `codex app-server` launches the real bridge from `thread/start`
    /// config and lists the tools through it.
    /// `cargo build && TODEX_CODEX_REAL=1 cargo test codex_app_server_loads -- --nocapture`
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn codex_app_server_loads_the_bridge() {
        use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
        if std::env::var_os("TODEX_CODEX_REAL").is_none() {
            return;
        }
        let harness = harness().await;
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let app = crate::server::router(harness.state.clone());
        tokio::spawn(async move {
            axum::serve(
                listener,
                app.into_make_service_with_connect_info::<SocketAddr>(),
            )
            .await
            .unwrap();
        });
        let mut server = harness
            .state
            .agent_mcp
            .launch(&harness.conversation_id)
            .await
            .unwrap()
            .servers
            .remove(0);
        // target/debug/deps/<test> → target/debug/todex-agentd
        server.command = std::env::current_exe()
            .unwrap()
            .parent()
            .unwrap()
            .parent()
            .unwrap()
            .join("todex-agentd");
        server.env[0].1 = format!("http://{addr}{ROUTE}");
        let (key, value) = server.codex_config();

        let codex_home = harness.root.join("codex-home");
        std::fs::create_dir_all(&codex_home).unwrap();
        // The override must merge with, not replace, the user's own servers.
        std::fs::write(
            codex_home.join("config.toml"),
            "[mcp_servers.user_server]\ncommand = \"/bin/cat\"\n",
        )
        .unwrap();
        let mut child = tokio::process::Command::new("codex")
            .args(["app-server", "--listen", "stdio://"])
            .env("CODEX_HOME", &codex_home)
            .current_dir(&harness.root)
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .unwrap();
        let mut stdin = child.stdin.take().unwrap();
        let mut lines = BufReader::new(child.stdout.take().unwrap()).lines();
        async fn send(stdin: &mut tokio::process::ChildStdin, message: Value) {
            stdin
                .write_all(format!("{message}\n").as_bytes())
                .await
                .unwrap();
        }
        async fn response(
            lines: &mut tokio::io::Lines<BufReader<tokio::process::ChildStdout>>,
            id: &str,
        ) -> Value {
            loop {
                let line = tokio::time::timeout(Duration::from_secs(60), lines.next_line())
                    .await
                    .expect("codex answers")
                    .unwrap()
                    .expect("codex stdout open");
                let value: Value = serde_json::from_str(&line).unwrap_or(Value::Null);
                if value["id"] == id {
                    assert!(value.get("error").is_none(), "{value}");
                    return value["result"].clone();
                }
            }
        }
        send(&mut stdin, json!({"id":"i","method":"initialize","params":{"clientInfo":{"name":"todex-test","version":"0"},"capabilities":{"experimentalApi":true}}})).await;
        response(&mut lines, "i").await;
        send(&mut stdin, json!({"method":"initialized"})).await;
        let mut config = serde_json::Map::new();
        config.insert(key, value);
        send(&mut stdin, json!({"id":"t","method":"thread/start","params":{"cwd": harness.root, "config": config}})).await;
        let thread = response(&mut lines, "t").await;
        let thread_id = thread["thread"]["id"].as_str().unwrap().to_owned();
        let mut found = Value::Null;
        let mut last = Value::Null;
        for attempt in 0..30 {
            send(&mut stdin, json!({"id":format!("s{attempt}"),"method":"mcpServerStatus/list","params":{"threadId":thread_id}})).await;
            let status = response(&mut lines, &format!("s{attempt}")).await;
            println!("{status}");
            last = status.clone();
            if let Some(entry) = status["data"]
                .as_array()
                .and_then(|servers| servers.iter().find(|server| server["name"] == SERVER_NAME))
            {
                if entry.to_string().contains("ssh_exec") {
                    found = entry.clone();
                    break;
                }
            }
            tokio::time::sleep(Duration::from_millis(500)).await;
        }
        assert!(found.to_string().contains("ssh_list_hosts"), "{found}");
        assert!(
            last["data"]
                .as_array()
                .unwrap()
                .iter()
                .any(|server| server["name"] == "user_server"),
            "{last}"
        );
        let _ = child.kill().await;
    }

    /// Manual check against the installed Claude Code CLI: its `init` frame
    /// reports the bridged server and tools; the invalid model makes the run
    /// fail before any API usage.
    /// `cargo build && TODEX_CLAUDE_REAL=1 cargo test claude_code_loads -- --nocapture`
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn claude_code_loads_the_bridge() {
        if std::env::var_os("TODEX_CLAUDE_REAL").is_none() {
            return;
        }
        let harness = harness().await;
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let app = crate::server::router(harness.state.clone());
        tokio::spawn(async move {
            axum::serve(
                listener,
                app.into_make_service_with_connect_info::<SocketAddr>(),
            )
            .await
            .unwrap();
        });
        // Both TodeX servers, to check the merged config and allow list.
        harness
            .state
            .agent_mcp
            .desktop()
            .set_enabled(true)
            .await
            .unwrap();
        let mut launch = harness
            .state
            .agent_mcp
            .launch(&harness.conversation_id)
            .await
            .unwrap();
        assert_eq!(launch.servers.len(), 2);
        for server in &mut launch.servers {
            server.command = std::env::current_exe()
                .unwrap()
                .parent()
                .unwrap()
                .parent()
                .unwrap()
                .join("todex-agentd");
            let route = if server.name == SERVER_NAME {
                ROUTE
            } else {
                super::super::DESKTOP_ROUTE
            };
            server.env[0].1 = format!("http://{addr}{route}");
        }
        let output = tokio::process::Command::new("claude")
            .args([
                "-p",
                "--no-session-persistence",
                "--output-format",
                "stream-json",
                "--verbose",
                "--model",
                "todex-nonexistent-model",
            ])
            .args(launch.claude_args().await.unwrap())
            .arg("hi")
            .current_dir(&harness.root)
            .output()
            .await
            .unwrap();
        let stdout = String::from_utf8_lossy(&output.stdout);
        let init: Value = stdout
            .lines()
            .filter_map(|line| serde_json::from_str::<Value>(line).ok())
            .find(|line| line["subtype"] == "init")
            .unwrap_or_else(|| panic!("no init frame: {stdout}"));
        let status = init["mcp_servers"]
            .as_array()
            .unwrap()
            .iter()
            .find(|server| server["name"] == SERVER_NAME)
            .cloned();
        println!("{status:?}");
        assert_eq!(status.unwrap()["status"], "connected");
        let tools = init["tools"].to_string();
        assert!(tools.contains("mcp__todex_ssh__ssh_exec"), "{tools}");
        assert!(tools.contains("mcp__todex_ssh__ssh_list_hosts"), "{tools}");
        assert!(
            tools.contains("mcp__todex_desktop__browser_snapshot"),
            "{tools}"
        );
        let desktop = init["mcp_servers"]
            .as_array()
            .unwrap()
            .iter()
            .find(|server| server["name"] == "todex_desktop")
            .cloned();
        assert_eq!(desktop.unwrap()["status"], "connected");
    }
}
