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
    http::{header, Method, StatusCode},
    middleware::Next,
    response::{IntoResponse, Response},
    Router,
};
use dashmap::DashMap;
use rmcp::{
    model::{
        CallToolRequestParams, CallToolResponse, CallToolResult, InitializeResult,
        ListPromptsResult, ListResourceTemplatesResult, ListResourcesResult, ListToolsResult,
        PaginatedRequestParams, Tool, ToolAnnotations,
    },
    service::RequestContext,
    ErrorData, RoleServer, ServerHandler,
};
use serde::Deserialize;
use serde_json::{json, Value};
use tokio::sync::{mpsc, Semaphore};
use uuid::Uuid;

use super::{
    authorizer::ToolMode,
    registry::{
        self, parse, schema, tool_error, BoxFuture, Caller, Invocation, Prepared, ToolEntry,
        ToolHost, ToolRegistry,
    },
    AgentMcp, SSH_ROUTE as ROUTE, SSH_SERVER as SERVER_NAME,
};
use crate::{
    app_state::AppState,
    external_command::{
        self, bounded_text, prepare_captured, CommandLimits, ExternalCommandError, OutputChunk,
        OutputStream,
    },
    local_terminal::decode_terminal_output,
    privacy_filter::{self, Categories},
    provider::ConversationSupervisor,
    ssh::{
        classify_failure,
        login::{LoginAsk, LoginOutcome, LoginResolution, LOGIN_TIMEOUT},
        SshFailureKind, SshMode,
    },
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
// The output tap stops at OUTPUT_LIMIT per stream; the recorder can only
// notice (and report) a cut when it sees more than it records.
const _: () = assert!(EVENT_OUTPUT_LIMIT < OUTPUT_LIMIT);
/// Live output is batched into events at this cadence or size.
const OUTPUT_FLUSH_INTERVAL: Duration = Duration::from_millis(100);
const OUTPUT_FLUSH_BYTES: usize = 16 * 1024;
const ERROR_DETAIL_LIMIT: usize = 4096;
/// Command text in an approval prompt.
const SUMMARY_COMMAND_CHARS: usize = 200;
/// Standard input shown in an approval prompt.
const APPROVAL_STDIN_PREVIEW_BYTES: usize = 4096;

pub(crate) fn routes(state: &AppState) -> Router<AppState> {
    let tools = SshTools {
        mcp: state.agent_mcp.clone(),
        conversations: state.conversations.clone(),
        host_slots: Arc::new(DashMap::new()),
        registry: Arc::new(registry_of_tools()),
    };
    registry::mcp_route(state, ROUTE, tools)
}

/// Only local, non-browser callers holding a conversation token get through.
/// This route deliberately sits outside device auth: the agents calling it
/// run on this machine and never hold a device key. Also follows which MCP
/// sessions use each token (initialize opens one, `DELETE` closes it).
pub(super) async fn guard(
    State(state): State<AppState>,
    mut request: Request,
    next: Next,
) -> Response {
    let conversation_id = match local_caller(&state, &request) {
        Ok(conversation_id) => conversation_id,
        Err(response) => return *response,
    };
    // Streamable HTTP: only `initialize` is posted without a session id.
    let has_session = request.headers().contains_key("mcp-session-id");
    let opens = request.method() == Method::POST && !has_session;
    let closes = request.method() == Method::DELETE && has_session;
    request.extensions_mut().insert(Caller {
        conversation_id: conversation_id.clone(),
    });
    let response = next.run(request).await;
    if response.status().is_success() {
        if opens {
            state.agent_mcp.session_opened(&conversation_id);
        } else if closes {
            state.agent_mcp.session_closed(&conversation_id).await;
        }
    }
    response
}

/// The conversation of a local, non-browser caller presenting its token.
pub(super) fn local_caller(state: &AppState, request: &Request) -> Result<String, Box<Response>> {
    let loopback = request
        .extensions()
        .get::<ConnectInfo<SocketAddr>>()
        .is_some_and(|ConnectInfo(peer)| peer.ip().to_canonical().is_loopback());
    // Browsers always send `Origin` on cross-origin POSTs; agents never do.
    if !loopback || request.headers().contains_key(header::ORIGIN) {
        return Err(Box::new(
            (StatusCode::FORBIDDEN, "local agents only").into_response(),
        ));
    }
    request
        .headers()
        .get(header::AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.strip_prefix("Bearer "))
        .and_then(|token| state.agent_mcp.authenticate(token.trim()))
        .ok_or_else(|| {
            Box::new(
                (
                    StatusCode::UNAUTHORIZED,
                    [(header::WWW_AUTHENTICATE, "Bearer")],
                    "invalid agent token",
                )
                    .into_response(),
            )
        })
}

#[derive(Clone)]
struct SshTools {
    mcp: AgentMcp,
    conversations: ConversationSupervisor,
    /// Alias → slots; bounds concurrent agent commands per host.
    host_slots: Arc<DashMap<String, Arc<Semaphore>>>,
    registry: Arc<ToolRegistry<SshTools, SshCall>>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ListHostsArgs {}

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

enum SshCall {
    ListHosts,
    Exec(ExecRequest),
}

fn prepare_list_hosts(arguments: Value) -> Result<Prepared<SshCall>, String> {
    let _: ListHostsArgs = parse("ssh_list_hosts", arguments)?;
    Ok(Prepared {
        call: SshCall::ListHosts,
        summary: "list SSH hosts".to_owned(),
        input: json!({}),
    })
}

fn prepare_exec(arguments: Value) -> Result<Prepared<SshCall>, String> {
    let request = validate(parse("ssh_exec", arguments)?)?;
    let total_chars = request.command.chars().count();
    let mut command: String = request
        .command
        .chars()
        .take(SUMMARY_COMMAND_CHARS)
        .collect();
    if total_chars > SUMMARY_COMMAND_CHARS {
        command.push_str(&format!("… (truncated, {total_chars} chars)"));
    }
    let mut input = json!({
        "host": request.host,
        "command": bounded_text(request.command.as_bytes(), EVENT_COMMAND_LIMIT),
        "timeoutSec": request.timeout.as_secs(),
    });
    if let Some(cwd) = &request.cwd {
        input["cwd"] = json!(cwd);
    }
    // The approver sees what the command reads: the start of standard input
    // (secret-looking values redacted, as it is journaled with the prompt).
    if let Some(stdin) = &request.stdin {
        let mut preview = redact_stdin_preview(&bounded_text(
            stdin.as_bytes(),
            APPROVAL_STDIN_PREVIEW_BYTES,
        ));
        if stdin.len() > APPROVAL_STDIN_PREVIEW_BYTES {
            preview.push_str(&format!("… (truncated, {} bytes)", stdin.len()));
        }
        input["stdinBytes"] = json!(stdin.len());
        input["stdinPreview"] = json!(preview);
    }
    Ok(Prepared {
        summary: format!("run `{command}` on {}", request.host),
        input,
        call: SshCall::Exec(request),
    })
}

/// Standard input as shown to the approver and journaled with the prompt:
/// keys and credentials are redacted (whatever the agent output filter is
/// set to, as the preview is stored with the conversation).
fn redact_stdin_preview(text: &str) -> String {
    const PREVIEW: Categories = Categories {
        private_key: true,
        public_key: false,
        credential: true,
        bank_card: false,
        id_card: false,
    };
    privacy_filter::redact(text, &PREVIEW).text
}

/// Why `ssh_exec` is refused before asking: in ask mode the user approves
/// the whole call, and the prompt and journal keep only
/// [`EVENT_COMMAND_LIMIT`] bytes of the command and
/// [`APPROVAL_STDIN_PREVIEW_BYTES`] of standard input, so a longer one could
/// hide what it does past the cut.
fn ask_mode_exec_refusal(arguments: Option<&rmcp::model::JsonObject>) -> Option<String> {
    let arguments = arguments?;
    let length = |field: &str| {
        arguments
            .get(field)
            .and_then(Value::as_str)
            .map_or(0, str::len)
    };
    let (field, limit, bytes) = if length("command") > EVENT_COMMAND_LIMIT {
        ("command", EVENT_COMMAND_LIMIT, length("command"))
    } else if length("stdin") > APPROVAL_STDIN_PREVIEW_BYTES {
        ("stdin", APPROVAL_STDIN_PREVIEW_BYTES, length("stdin"))
    } else {
        return None;
    };
    Some(format!(
        "ssh_exec {field} is limited to {limit} bytes while the user approves each call \
         (this one is {bytes} bytes). Upload the script to the host as a file first (for \
         example ask the user to copy it there), then run it with a short command."
    ))
}

fn run<'a>(
    tools: &'a SshTools,
    call: SshCall,
    invocation: Invocation<'a>,
) -> BoxFuture<'a, CallToolResult> {
    Box::pin(async move {
        match call {
            SshCall::ListHosts => tools.list_hosts().await,
            SshCall::Exec(request) => tools.exec(&invocation, request).await,
        }
    })
}

fn registry_of_tools() -> ToolRegistry<SshTools, SshCall> {
    ToolRegistry::new(vec![
        ToolEntry {
            tool: Tool::new(
                "ssh_list_hosts",
                "List the SSH hosts the user allowed agents to use. Only these aliases work with ssh_exec.",
                schema(json!({ "type": "object", "properties": {}, "additionalProperties": false })),
            )
            .with_annotations(ToolAnnotations::new().read_only(true).open_world(false)),
            side_effect: false,
            prepare: prepare_list_hosts,
            run,
        },
        ToolEntry {
            tool: Tool::new(
                "ssh_exec",
                "Run a shell command on an allowed SSH host (non-interactive, no TTY, no password prompts). \
                 Returns stdout, stderr and the exit code; a non-zero exit code is a normal result. \
                 stdout and stderr are each capped at 256 KiB (truncated=true when cut). Unavailable \
                 in Plan mode; in ask mode the user approves each call and command and stdin are each limited \
                 to 4096 bytes (upload a longer script to the host as a file first, then run it). \
                 The user's privacy filter may replace keys, passwords, tokens, card and ID numbers in the \
                 output with [REDACTED:<kind>] placeholders (redacted=true).",
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
            side_effect: true,
            prepare: prepare_exec,
            run,
        },
    ])
}

impl ToolHost for SshTools {
    type Call = SshCall;
    const SERVER: &'static str = SERVER_NAME;

    fn registry(&self) -> &ToolRegistry<Self, SshCall> {
        &self.registry
    }

    fn mcp(&self) -> &AgentMcp {
        &self.mcp
    }

    fn conversations(&self) -> &ConversationSupervisor {
        &self.conversations
    }
}

impl ServerHandler for SshTools {
    fn get_info(&self) -> InitializeResult {
        registry::server_info(
            SERVER_NAME,
            "Runs commands on the user's SSH hosts. Call ssh_list_hosts first; \
             only hosts listed there can be used.",
        )
    }

    async fn list_tools(
        &self,
        _request: Option<PaginatedRequestParams>,
        _context: RequestContext<RoleServer>,
    ) -> Result<ListToolsResult, ErrorData> {
        Ok(self.registry.list(|_| true))
    }

    async fn call_tool(
        &self,
        request: CallToolRequestParams,
        context: RequestContext<RoleServer>,
    ) -> Result<CallToolResponse, ErrorData> {
        if request.name == "ssh_exec" {
            let caller = registry::caller(&context)?;
            let mode = self.mcp.inner.authorizer.tool_mode(&caller.conversation_id);
            if mode == ToolMode::Ask {
                if let Some(message) = ask_mode_exec_refusal(request.arguments.as_ref()) {
                    return Ok(tool_error(message).into());
                }
            }
        }
        self.registry().call(self, request, &context).await
    }

    async fn list_prompts(
        &self,
        _request: Option<PaginatedRequestParams>,
        _context: RequestContext<RoleServer>,
    ) -> Result<ListPromptsResult, ErrorData> {
        Ok(registry::no_prompts())
    }

    async fn list_resources(
        &self,
        _request: Option<PaginatedRequestParams>,
        _context: RequestContext<RoleServer>,
    ) -> Result<ListResourcesResult, ErrorData> {
        Ok(registry::no_resources())
    }

    async fn list_resource_templates(
        &self,
        _request: Option<PaginatedRequestParams>,
        _context: RequestContext<RoleServer>,
    ) -> Result<ListResourceTemplatesResult, ErrorData> {
        Ok(registry::no_resource_templates())
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

    async fn exec(&self, invocation: &Invocation<'_>, request: ExecRequest) -> CallToolResult {
        let caller = &invocation.caller;
        let context = invocation.context;
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
        if let Some(device) = &invocation.approved_by {
            started["approvedBy"] = json!(device);
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

        // Read per call, so a changed setting applies to the next command.
        let filter = self.mcp.ssh().agent_output_filter().await;
        let begun = Instant::now();
        let (tap, chunks) = mpsc::unbounded_channel();
        let run = async {
            tokio::select! {
                outcome = self.run_logged_in(&caller.conversation_id, &exec_id, &request, tap) => outcome,
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
        let (result, mut completed) =
            outcome.into_result(&request.host, duration_ms, filter.active());
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

    /// [`Self::run`]; when ssh cannot authenticate on its own, asks the
    /// conversation's clients for a login (see [`crate::ssh::login`]) and
    /// runs the command once more on the connection it opened.
    async fn run_logged_in(
        &self,
        conversation_id: &str,
        exec_id: &str,
        request: &ExecRequest,
        tap: mpsc::UnboundedSender<OutputChunk>,
    ) -> ExecOutcome {
        let outcome = self.run(request, tap.clone()).await;
        let ExecOutcome::Finished(output) = &outcome else {
            return outcome;
        };
        let auth_failed = output.status.code() == Some(SSH_ERROR_STATUS)
            && classify_failure(&String::from_utf8_lossy(&output.stderr))
                == SshFailureKind::AuthenticationFailed;
        if !auth_failed || !self.mcp.ssh().password_login_supported(&request.host).await {
            return outcome;
        }
        match self.log_in(conversation_id, exec_id, &request.host).await {
            Ok(()) => self.run(request, tap).await,
            Err(hint) => match outcome {
                ExecOutcome::Finished(output) => ExecOutcome::LoginFailed(output, hint),
                other => other,
            },
        }
    }

    /// Waits for a login to `host`; the error is the hint for the agent.
    async fn log_in(&self, conversation_id: &str, exec_id: &str, host: &str) -> Result<(), String> {
        let resolution = match self.mcp.ssh().ask_login(conversation_id, host) {
            LoginAsk::BackedOff => {
                return Err(format!(
                    "The user declined or did not answer a login to {host} moments ago. Ask them \
                     to log in from the TodeX Terminal view, then retry."
                ))
            }
            LoginAsk::Joined(waiter) => waiter.wait().await,
            LoginAsk::New(request) => {
                let request_id = request.id().to_owned();
                let expires_at = chrono::Utc::now()
                    + chrono::Duration::from_std(LOGIN_TIMEOUT).unwrap_or_default();
                if let Err(error) = self
                    .conversations
                    .append_agent_event(
                        conversation_id,
                        "ssh.login.requested",
                        json!({
                            "requestId": request_id,
                            "host": host,
                            "execId": exec_id,
                            "expiresAt": expires_at.to_rfc3339_opts(chrono::SecondsFormat::Millis, true),
                        }),
                    )
                    .await
                {
                    return Err(format!("cannot record the login request: {error}"));
                }
                // Dropped when the tool call is cancelled: the request then
                // ends for its clients too.
                let mut resolved = ResolvedEvent {
                    conversations: self.conversations.clone(),
                    conversation_id: conversation_id.to_owned(),
                    request_id,
                    host: host.to_owned(),
                    resolution: None,
                };
                let resolution = request.wait().await;
                resolved.resolution = Some(resolution);
                resolution
            }
        };
        match resolution.outcome {
            LoginOutcome::Connected => Ok(()),
            LoginOutcome::Declined => Err(format!(
                "The user declined to log in to {host}. Do not retry unless they ask you to."
            )),
            LoginOutcome::TimedOut => Err(format!(
                "Nobody answered the login request for {host} within {}s. Ask the user to log in \
                 from the TodeX Terminal view, then retry.",
                LOGIN_TIMEOUT.as_secs()
            )),
            LoginOutcome::Cancelled => Err("The login request was cancelled.".to_owned()),
        }
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

/// Records `ssh.login.resolved` for a login request once it ended, also
/// when the waiting tool call is cancelled (dropped) first.
struct ResolvedEvent {
    conversations: ConversationSupervisor,
    conversation_id: String,
    request_id: String,
    host: String,
    resolution: Option<LoginResolution>,
}

impl Drop for ResolvedEvent {
    fn drop(&mut self) {
        let resolution = self.resolution.unwrap_or(LoginResolution {
            outcome: LoginOutcome::Cancelled,
            saved_password: false,
        });
        let conversations = self.conversations.clone();
        let conversation_id = std::mem::take(&mut self.conversation_id);
        let payload = json!({
            "requestId": self.request_id,
            "host": self.host,
            "outcome": resolution.outcome,
            // Not `savedPassword`: journaled keys naming a password are redacted.
            "remembered": resolution.saved_password,
        });
        let Ok(runtime) = tokio::runtime::Handle::try_current() else {
            return;
        };
        runtime.spawn(async move {
            if let Err(error) = conversations
                .append_agent_event(&conversation_id, "ssh.login.resolved", payload)
                .await
            {
                tracing::warn!(conversation_id, %error, "failed to record an SSH login result");
            }
        });
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
    /// An authentication failure no login fixed; the hint says why.
    LoginFailed(external_command::TruncatedOutput, String),
    TimedOut(Duration),
    NotStarted(String),
    Cancelled,
}

/// `ssh` reserves exit status 255 for its own failures.
const SSH_ERROR_STATUS: i32 = 255;

impl ExecOutcome {
    /// The tool result and the `ssh.exec.completed` payload.
    /// `filter`: the categories redacted from output the agent receives.
    fn into_result(
        self,
        host: &str,
        duration_ms: u64,
        filter: Option<&Categories>,
    ) -> (CallToolResult, Value) {
        let mut completed = json!({ "host": host, "durationMs": duration_ms, "truncated": false });
        let result = match self {
            Self::Finished(output) => {
                finished_result(output, None, host, duration_ms, filter, &mut completed)
            }
            Self::LoginFailed(output, hint) => finished_result(
                output,
                Some(hint),
                host,
                duration_ms,
                filter,
                &mut completed,
            ),
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

/// The result of an ssh run that exited; `login_hint` replaces the usual
/// hint when a login was tried and did not happen. Output is redacted with
/// `filter` before the agent sees it; the counts go to `completed`.
fn finished_result(
    output: external_command::TruncatedOutput,
    login_hint: Option<String>,
    host: &str,
    duration_ms: u64,
    filter: Option<&Categories>,
    completed: &mut Value,
) -> CallToolResult {
    let raw_stderr = String::from_utf8_lossy(&output.stderr).into_owned();
    let exit_code = output.status.code();
    completed["exitCode"] = json!(exit_code);
    completed["truncated"] = json!(output.truncated);
    let mut redactions = std::collections::BTreeMap::<_, usize>::new();
    let mut filtered = |text: String| match filter {
        Some(categories) => {
            let redacted = privacy_filter::redact(&text, categories);
            for (category, count) in redacted.counts {
                *redactions.entry(category).or_default() += count;
            }
            redacted.text
        }
        None => text,
    };
    let stdout = filtered(String::from_utf8_lossy(&output.stdout).into_owned());
    let stderr = filtered(raw_stderr.clone());
    let (mut body, failed) = match exit_code {
        Some(code) if code != SSH_ERROR_STATUS => (
            json!({
                "stdout": stdout,
                "stderr": stderr,
                "exitCode": code,
                "truncated": output.truncated,
                "durationMs": duration_ms,
            }),
            false,
        ),
        _ => {
            // Classified from what ssh printed, before redaction.
            let failure = if exit_code.is_some() {
                classify_failure(&raw_stderr)
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
            if let Some(hint) = login_hint.or_else(|| failure_hint(failure).map(str::to_owned)) {
                error["hint"] = json!(hint);
            }
            (error, true)
        }
    };
    if !redactions.is_empty() {
        body["redacted"] = json!(true);
        body["note"] = json!(REDACTION_NOTE);
        completed["redactions"] = json!(redactions);
    }
    if failed {
        CallToolResult::structured_error(body)
    } else {
        CallToolResult::structured(body)
    }
}

/// Tells the agent what the placeholders in a redacted result are.
const REDACTION_NOTE: &str = "The user's TodeX privacy filter replaced sensitive values in this \
    output (keys, passwords, tokens, card or ID numbers) with [REDACTED:<kind>] placeholders. \
    They are not the real content. Do not try to recover or work around them; if the task needs \
    such a value, ask the user to handle that step.";

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
            "Key/agent authentication failed and no login was possible from here. For password-only hosts, ask the user to log in from the TodeX Terminal view; that connection is reused until it has been idle for 5 minutes.",
        ),
        SshFailureKind::Unreachable => Some("The host could not be reached from the TodeX machine."),
        SshFailureKind::TimedOut => Some("Connecting to the host timed out."),
        SshFailureKind::Other => None,
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use crate::app_state::AppState;
    use crate::config::Config;
    use crate::conversation::ProviderKind;
    use crate::privacy_filter::AgentOutputFilter;
    use axum::body::Body;
    use rmcp::model::CacheScope;
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
    /// `slow*` hang; `big*` print 300 KiB; `pw*` refuse BatchMode logins
    /// until a password login made `<root>/master-<alias>`; others echo
    /// their command line.
    async fn harness() -> Harness {
        // Short enough for connection sharing (see `MAX_CONTROL_DIR_LEN`).
        let root = std::env::temp_dir().join(format!("tsm-{}", uuid::Uuid::new_v4().simple()));
        let home = root.join("home");
        let workspace = root.join("workspaces/project");
        std::fs::create_dir_all(home.join(".ssh")).unwrap();
        std::fs::create_dir_all(&workspace).unwrap();
        std::fs::write(
            home.join(".ssh/config"),
            "Host web bad1 slow1 big1 off pw1\n",
        )
        .unwrap();
        let fake = root.join("fake-ssh");
        std::fs::write(
            &fake,
            format!(
                r#"#!/bin/sh
printf '%s\n' "$*" >> "{log}"
last=""; for arg in "$@"; do prev="$last"; last="$arg"; done
case " $* " in
  *" -G "*) printf 'hostname %s.example\nuser ops\nport 2222\nidentityfile ~/.ssh/id_ed25519\n' "$last"; exit 0;;
  *" -O check "*) [ -f "{root}/master-$last" ] && exit 0; exit 255;;
esac
case "$prev" in
  bad*) echo "Host key verification failed." >&2; exit 255;;
  pw*)
    if [ -n "$TODEX_SSH_ASKPASS_FILE" ]; then : > "{root}/master-$prev"; exit 0; fi
    [ -f "{root}/master-$prev" ] || {{ echo "ops@$prev: Permission denied (password)." >&2; exit 255; }};;
  slow*) sleep 30; exit 0;;
  big*) head -c 300000 /dev/zero | tr '\0' x; exit 0;;
esac
printf 'ran:%s\n' "$last"
if [ ! -t 0 ]; then cat; fi
echo warn >&2
exit 3
"#,
                log = root.join("ssh.log").display(),
                root = root.display(),
            ),
        )
        .unwrap();
        std::fs::set_permissions(&fake, std::fs::Permissions::from_mode(0o755)).unwrap();

        let config = Config {
            data_dir: root.join("data"),
            workspace_roots: vec![std::fs::canonicalize(root.join("workspaces")).unwrap()],
            ..Config::default()
        };
        let mut state = AppState::new_for_tests(config).await.unwrap();
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
        // A full-access turn, as most tests exercise tools without approval;
        // tests for other modes record their own.
        state
            .agent_mcp
            .record_turn_mode(&manifest.id, "full-access", "implement");
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
        let app = || crate::server::loopback_test_router(harness.state.clone());
        let status = |request: Request| async { app().oneshot(request).await.unwrap().status() };

        // Remote peers are already stopped by the transport v2 enforcement.
        assert_eq!(
            status(mcp_request(
                "192.168.1.9:5000",
                Some(&harness.token),
                initialize()
            ))
            .await,
            StatusCode::UPGRADE_REQUIRED
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
        let app = crate::server::loopback_test_router(harness.state.clone());
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

    /// Antigravity opens with the MCP 2026 `server/discover` before
    /// `initialize`; each request must get an answer through the bridge,
    /// or agy waits on the server for good.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn bridge_answers_a_discover_first_handshake() {
        use tokio::io::{AsyncBufReadExt, AsyncWriteExt};
        let harness = harness().await;
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let app = crate::server::loopback_test_router(harness.state.clone());
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
            let _ = super::super::bridge::proxy(&url, &token, bridge_read, bridge_write).await;
        });
        let (read, mut write) = tokio::io::split(client_side);
        let mut lines = tokio::io::BufReader::new(read).lines();
        for (id, message) in [
            (
                1,
                json!({"jsonrpc":"2.0","id":1,"method":"server/discover","params":{"_meta":{}}}),
            ),
            (
                2,
                json!({"jsonrpc":"2.0","id":2,"method":"initialize","params":{"protocolVersion":"2025-11-25","capabilities":{},"clientInfo":{"name":"antigravity-client","version":"v1.0.0"}}}),
            ),
        ] {
            write
                .write_all(format!("{message}\n").as_bytes())
                .await
                .unwrap();
            let reply = tokio::time::timeout(Duration::from_secs(10), lines.next_line())
                .await
                .unwrap_or_else(|_| panic!("no answer to request {id}"))
                .unwrap()
                .unwrap();
            let reply: Value = serde_json::from_str(&reply).unwrap();
            assert_eq!(reply["id"], id, "{reply}");
        }
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

        let listed = client.list_tools(None).await.unwrap();
        // MCP 2026-07-28 clients reject a list without its cache hints.
        assert_eq!(listed.ttl_ms, Some(0));
        assert_eq!(listed.cache_scope, Some(CacheScope::Private));
        let names: Vec<_> = listed.tools.iter().map(|tool| tool.name.as_ref()).collect();
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

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn exec_output_reaches_the_agent_redacted() {
        let harness = harness().await;
        let client = bridged_client(&harness).await;
        let stdin = "DB_PASSWORD=Zq8x7Lk9w\ncard 4111 1111 1111 1111\n";

        let result = call(
            &client,
            "ssh_exec",
            json!({ "host": "web", "command": "cat", "stdin": stdin }),
        )
        .await;
        let output = structured(&result);
        assert_eq!(
            output["stdout"],
            "ran:cat\nDB_PASSWORD=[REDACTED:credential]\ncard [REDACTED:bank_card ****1111]\n"
        );
        assert_eq!(output["redacted"], true);
        assert!(output["note"].as_str().unwrap().contains("privacy filter"));

        // Off: the agent gets the output as printed.
        harness
            .state
            .ssh
            .set_agent_output_filter(AgentOutputFilter {
                enabled: false,
                ..AgentOutputFilter::default()
            })
            .await
            .unwrap();
        let raw = call(
            &client,
            "ssh_exec",
            json!({ "host": "web", "command": "cat", "stdin": stdin }),
        )
        .await;
        assert_eq!(structured(&raw)["stdout"], format!("ran:cat\n{stdin}"));
        assert!(structured(&raw).get("redacted").is_none());

        // The user's live view keeps the original; completion reports counts.
        let history = harness
            .state
            .conversations
            .history_for_tests(&harness.conversation_id)
            .await;
        let completed: Vec<_> = history
            .iter()
            .filter(|event| event.event_type == "ssh.exec.completed")
            .map(|event| event.payload.clone())
            .collect();
        assert_eq!(
            completed[0]["redactions"],
            json!({ "credential": 1, "bankCard": 1 })
        );
        assert!(completed[1].get("redactions").is_none());
        let first = &completed[0]["execId"];
        let live: String = history
            .iter()
            .filter(|event| event.event_type == "ssh.exec.output")
            .filter(|event| {
                &event.payload["execId"] == first && event.payload["stream"] == "stdout"
            })
            .map(|event| event.payload["data"].as_str().unwrap().to_owned())
            .collect();
        assert_eq!(live, format!("ran:cat\n{stdin}"));

        client.cancel().await.unwrap();
    }

    /// The id of the conversation's open `ssh.login.requested`, waiting for
    /// one to be recorded.
    async fn login_request(harness: &Harness, seen: usize) -> String {
        for _ in 0..200 {
            let history = harness
                .state
                .conversations
                .history_for_tests(&harness.conversation_id)
                .await;
            let requests: Vec<_> = history
                .iter()
                .filter(|event| event.event_type == "ssh.login.requested")
                .collect();
            if requests.len() > seen {
                let payload = &requests[seen].payload;
                assert_eq!(payload["host"], "pw1", "{payload}");
                assert!(payload["execId"].as_str().unwrap().starts_with("sshx_"));
                assert!(payload["expiresAt"].as_str().is_some(), "{payload}");
                return payload["requestId"].as_str().unwrap().to_owned();
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
        panic!("no login request was recorded");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn exec_waits_for_a_login_when_ssh_cannot_authenticate() {
        let harness = harness().await;
        harness
            .state
            .ssh
            .set_agent_access("pw1", true)
            .await
            .unwrap();
        let client = bridged_client(&harness).await;
        let exec = || {
            call(
                &client,
                "ssh_exec",
                json!({ "host": "pw1", "command": "ls" }),
            )
        };

        // Declined: the original failure, with a hint saying so.
        let answer = async {
            let id = login_request(&harness, 0).await;
            harness
                .state
                .ssh
                .answer_login("pw1", &id, None, false)
                .await
                .unwrap();
        };
        let (declined, ()) = tokio::join!(exec(), answer);
        assert_eq!(declined.is_error, Some(true));
        assert_eq!(structured(&declined)["failure"], "authenticationFailed");
        assert!(structured(&declined)["hint"]
            .as_str()
            .unwrap()
            .contains("declined"));
        // Asked again only after the backoff.
        let backed_off = exec().await;
        assert!(structured(&backed_off)["hint"]
            .as_str()
            .unwrap()
            .contains("moments ago"));
        harness
            .state
            .ssh
            .logins()
            .clear_backoff_for_tests(&harness.conversation_id, "pw1");

        // Answered with a password: the command runs on the new master.
        let answer = async {
            let id = login_request(&harness, 1).await;
            harness
                .state
                .ssh
                .answer_login("pw1", &id, Some("hunter2"), true)
                .await
                .unwrap();
        };
        let (ran, ()) = tokio::join!(exec(), answer);
        assert_eq!(ran.is_error, Some(false), "{ran:?}");
        assert_eq!(structured(&ran)["stdout"], "ran:ls\n");
        // Later commands reuse it without asking.
        let again = exec().await;
        assert_eq!(again.is_error, Some(false));

        let history = harness
            .state
            .conversations
            .history_for_tests(&harness.conversation_id)
            .await;
        assert_eq!(
            history
                .iter()
                .filter(|event| event.event_type == "ssh.login.requested")
                .count(),
            2
        );
        let mut resolved = Vec::new();
        for _ in 0..200 {
            resolved = harness
                .state
                .conversations
                .history_for_tests(&harness.conversation_id)
                .await
                .into_iter()
                .filter(|event| event.event_type == "ssh.login.resolved")
                .map(|event| event.payload)
                .collect();
            if resolved.len() == 2 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
        assert_eq!(resolved.len(), 2, "{resolved:?}");
        assert_eq!(resolved[0]["outcome"], "declined");
        assert_eq!(resolved[1]["outcome"], "connected");
        assert_eq!(resolved[1]["remembered"], true);
        assert!(!resolved
            .iter()
            .any(|event| event.to_string().contains("hunter2")));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn plan_mode_refuses_ssh_exec_but_lists_hosts() {
        let harness = harness().await;
        let client = bridged_client(&harness).await;
        harness
            .state
            .agent_mcp
            .record_turn_mode(&harness.conversation_id, "ask", "plan");
        let hosts = call(&client, "ssh_list_hosts", json!({})).await;
        assert_eq!(hosts.is_error, Some(false));
        let refused = call(
            &client,
            "ssh_exec",
            json!({ "host": "web", "command": "ls" }),
        )
        .await;
        assert_eq!(refused.is_error, Some(true));
        let log = std::fs::read_to_string(harness.root.join("ssh.log")).unwrap_or_default();
        assert!(!log.lines().any(|line| line.contains("-- web ")));
        let history = harness
            .state
            .conversations
            .history_for_tests(&harness.conversation_id)
            .await;
        assert!(!history
            .iter()
            .any(|event| event.event_type == "ssh.exec.started"));
        client.cancel().await.unwrap();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn ask_mode_refuses_commands_too_long_to_review() {
        let harness = harness().await;
        let client = bridged_client(&harness).await;
        let long = format!("echo {}", "x".repeat(EVENT_COMMAND_LIMIT));
        // Without approval the full command runs.
        let ran = call(
            &client,
            "ssh_exec",
            json!({ "host": "web", "command": long }),
        )
        .await;
        assert_eq!(ran.is_error, Some(false));

        harness
            .state
            .agent_mcp
            .record_turn_mode(&harness.conversation_id, "ask", "implement");
        let refused = call(
            &client,
            "ssh_exec",
            json!({ "host": "web", "command": long }),
        )
        .await;
        assert_eq!(refused.is_error, Some(true));
        let text = serde_json::to_string(&refused.content).unwrap();
        assert!(text.contains("Upload the script"), "{text}");
        // Standard input is shown in the prompt only up to the same limit.
        let refused = call(
            &client,
            "ssh_exec",
            json!({
                "host": "web",
                "command": "cat > run.sh",
                "stdin": "x".repeat(APPROVAL_STDIN_PREVIEW_BYTES + 1),
            }),
        )
        .await;
        assert_eq!(refused.is_error, Some(true));
        let text = serde_json::to_string(&refused.content).unwrap();
        assert!(
            text.contains("stdin is limited") && text.contains("Upload the script"),
            "{text}"
        );
        let history = harness
            .state
            .conversations
            .history_for_tests(&harness.conversation_id)
            .await;
        assert!(!history
            .iter()
            .any(|event| event.event_type == "permission.requested"));
        client.cancel().await.unwrap();
    }

    #[test]
    fn approval_shows_truncation_and_a_stdin_preview() {
        let command = format!("echo {}", "y".repeat(300));
        let stdin = format!("{}tail", "z".repeat(APPROVAL_STDIN_PREVIEW_BYTES));
        let prepared = prepare_exec(json!({
            "host": "web",
            "command": command,
            "stdin": stdin,
        }))
        .unwrap();
        assert!(
            prepared
                .summary
                .contains(&format!("… (truncated, {} chars)", command.chars().count())),
            "{}",
            prepared.summary
        );
        assert_eq!(prepared.input["stdinBytes"], json!(stdin.len()));
        let preview = prepared.input["stdinPreview"].as_str().unwrap();
        assert!(preview.starts_with("zzz"));
        assert!(!preview.contains("tail"));
        assert!(preview.ends_with(&format!("… (truncated, {} bytes)", stdin.len())));

        let short = prepare_exec(json!({
            "host": "web",
            "command": "cat > run.sh",
            "stdin": "#!/bin/sh\nTOKEN=sk-ant-abcdefghijklmnop0123\necho hi\n",
        }))
        .unwrap();
        assert_eq!(short.summary, "run `cat > run.sh` on web");
        let preview = short.input["stdinPreview"].as_str().unwrap();
        assert!(preview.contains("echo hi") && !preview.contains("truncated"));
        assert!(
            !preview.contains("sk-ant-abcdefghijklmnop0123"),
            "{preview}"
        );
    }

    #[test]
    fn stdin_preview_redacts_pem_blocks_and_secret_assignments() {
        let stdin = "#!/bin/sh\n\
            export DB_PASSWORD=hunter2\n\
            api-key: abc123\n\
            \"client_secret\": \"s3cr3t\",\n\
            Private_Key = xyz\n\
            USER=alice\n\
            cat <<'EOF' > id\n\
            -----BEGIN OPENSSH PRIVATE KEY-----\n\
            b3BlbnNzaC1rZXktdjEAAAAA\n\
            -----END OPENSSH PRIVATE KEY-----\n\
            EOF\n\
            echo done\n\
            -----BEGIN RSA PRIVATE KEY-----\n\
            MIIEowIBAAKCAQEA";
        let preview = redact_stdin_preview(stdin);
        for secret in [
            "hunter2",
            "abc123",
            "s3cr3t",
            "xyz",
            "b3BlbnNzaC1",
            "MIIEow",
        ] {
            assert!(!preview.contains(secret), "{secret} leaked: {preview}");
        }
        assert!(
            preview.contains("export DB_PASSWORD=[REDACTED:credential]"),
            "{preview}"
        );
        assert!(preview.contains("USER=alice"), "{preview}");
        assert!(
            preview.contains("echo done\nEOF") || preview.contains("EOF\necho done"),
            "{preview}"
        );
        assert_eq!(
            preview.matches("[REDACTED:private_key]").count(),
            2,
            "{preview}"
        );

        let prepared = prepare_exec(json!({
            "host": "web",
            "command": "cat > .env",
            "stdin": "TOKEN=plain-value\n",
        }))
        .unwrap();
        assert_eq!(
            prepared.input["stdinPreview"],
            json!("TOKEN=[REDACTED:credential]\n")
        );
    }

    async fn open_session(harness: &Harness) -> String {
        let response = crate::server::router(harness.state.clone())
            .oneshot(mcp_request(
                "127.0.0.1:5000",
                Some(&harness.token),
                initialize(),
            ))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        response.headers()["mcp-session-id"]
            .to_str()
            .unwrap()
            .to_owned()
    }

    async fn close_session(harness: &Harness, session: &str) {
        let mut request = mcp_request("127.0.0.1:5000", Some(&harness.token), json!({}));
        *request.method_mut() = Method::DELETE;
        request
            .headers_mut()
            .insert("mcp-session-id", session.parse().unwrap());
        let status = crate::server::router(harness.state.clone())
            .oneshot(request)
            .await
            .unwrap()
            .status();
        assert!(status.is_success(), "{status}");
    }

    async fn write_claude_config(harness: &Harness) -> PathBuf {
        let launch = harness
            .state
            .agent_mcp
            .launch(&harness.conversation_id)
            .await
            .unwrap();
        let args = crate::provider::mcp_injection::claude_args(&launch)
            .await
            .unwrap();
        PathBuf::from(args[0].strip_prefix("--mcp-config=").unwrap())
    }

    #[tokio::test]
    async fn the_claude_config_goes_once_no_session_needs_it() {
        let harness = harness().await;
        let path = write_claude_config(&harness).await;
        let first = open_session(&harness).await;
        assert!(path.is_file(), "kept while a session is open");
        // A newer launch has not connected yet: its config must survive the
        // previous session ending.
        write_claude_config(&harness).await;
        close_session(&harness, &first).await;
        assert!(path.is_file());
        let second = open_session(&harness).await;
        close_session(&harness, &second).await;
        assert!(!path.exists(), "removed once no session needs it");
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
        let app = crate::server::loopback_test_router(harness.state.clone());
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
        let app = crate::server::loopback_test_router(harness.state.clone());
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
        let (key, value) = crate::provider::mcp_injection::codex_config(&server);

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
        let app = crate::server::loopback_test_router(harness.state.clone());
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
            .args(
                crate::provider::mcp_injection::claude_args(&launch)
                    .await
                    .unwrap(),
            )
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

    /// Manual check against the installed Pi CLI: `pi --mode rpc` loads
    /// `todex-pi-mcp.mjs` via `-e` and invokes `mcp__todex_ssh__ssh_list_hosts`
    /// through the real `todex-agentd agent-mcp-bridge` even when
    /// `pi-mcp-adapter` is installed in the user's `~/.pi/agent/settings.json`.
    /// `cargo build && TODEX_PI_REAL=1 cargo test pi_loads_the_bridge -- --nocapture`
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn pi_loads_the_bridge() {
        use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
        if std::env::var_os("TODEX_PI_REAL").is_none() {
            return;
        }
        let harness = harness().await;
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let app = crate::server::loopback_test_router(harness.state.clone());
        tokio::spawn(async move {
            axum::serve(
                listener,
                app.into_make_service_with_connect_info::<SocketAddr>(),
            )
            .await
            .unwrap();
        });
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

        // Write a tiny probe extension that verifies the registered tools and
        // calls `mcp__todex_ssh__ssh_list_hosts` on a custom command.
        let probe_ext = harness.root.join("probe-todex-mcp.mjs");
        std::fs::write(
            &probe_ext,
            r#"export default function probe(pi) {
  pi.registerCommand("todex-mcp-probe", {
    description: "Probe TodeX MCP tools",
    handler: async (_args, ctx) => {
      const tools = pi.getAllTools().map((t) => t.name);
      const active = pi.getActiveTools();
      ctx.ui.notify(JSON.stringify({ todex_probe: true, tools, active }), "info");
    },
  });
}
"#,
        )
        .unwrap();

        let (env_key, env_val) = crate::provider::mcp_injection::pi_env(&launch).unwrap();
        let mut child = tokio::process::Command::new("pi")
            .args(["--mode", "rpc", "--no-session", "--approve"])
            .args(crate::provider::mcp_injection::pi_args(&launch))
            .args(["-e", &probe_ext.display().to_string()])
            .env(env_key, env_val)
            .current_dir(&harness.root)
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .unwrap();
        let mut stdin = child.stdin.take().unwrap();
        let mut lines = BufReader::new(child.stdout.take().unwrap()).lines();
        stdin
            .write_all(b"{\"id\":\"p\",\"type\":\"prompt\",\"message\":\"/todex-mcp-probe\"}\n")
            .await
            .unwrap();
        let mut tools_json = String::new();
        while let Ok(Ok(Some(line))) =
            tokio::time::timeout(Duration::from_secs(30), lines.next_line()).await
        {
            let value: Value = serde_json::from_str(&line).unwrap_or(Value::Null);
            if value["type"] == "extension_ui_request" && value["method"] == "notify" {
                let msg = value["message"].as_str().unwrap_or_default();
                if msg.contains("todex_probe") {
                    tools_json = msg.to_owned();
                    break;
                }
            }
        }
        assert!(
            tools_json.contains("mcp__todex_ssh__ssh_list_hosts"),
            "registered tools: {tools_json}"
        );
        assert!(
            tools_json.contains("mcp__todex_ssh__ssh_exec"),
            "registered tools: {tools_json}"
        );
        assert!(
            tools_json.contains("mcp__todex_desktop__browser_snapshot"),
            "registered tools: {tools_json}"
        );
        let _ = child.kill().await;
    }
}
