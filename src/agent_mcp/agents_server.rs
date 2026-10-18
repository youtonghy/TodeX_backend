//! The `todex_agents` MCP server: an agent running in TodeX calls another
//! TodeX agent (`agents_list`, `agent_run`, `agent_turn`, `agent_wait`,
//! `agent_cancel`), mirroring the external API (`/api/v1`) over the shared
//! call layer [`crate::agent_calls`].
//!
//! Each call starts or continues a child conversation of the calling one
//! (`parentConversationId`), owned by the same owner, in the same
//! workspace, run in the calling turn's permission mode (never above it).
//! Children cannot call agents themselves: the server is not injected into
//! them and refuses them. The global agent scope from `agent-calling.json`
//! bounds which agents and models may be called; `mcp.approval` answers the
//! children's permission requests.

use std::{sync::Arc, time::Duration};

use axum::Router;
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

use super::{
    registry::{
        self, parse, schema, tool_error, BoxFuture, Invocation, Prepared, ToolEntry, ToolHost,
        ToolRegistry,
    },
    AgentMcp, AGENTS_ROUTE as ROUTE, AGENTS_SERVER as SERVER_NAME,
};
use crate::{
    agent_calls::{self, AgentToolCaller, CallResult, RunInput, TurnInput, LIMITS},
    app_state::AppState,
    conversation::{ConversationManifest, ProviderKind},
    error::AppError,
    provider::{profile::profile, CancelOutcome, ConversationSupervisor},
};

/// Prompt text an agent may send in one call.
const MAX_TEXT_BYTES: usize = 256 * 1024;
/// Prompt text shown in an approval (ask mode).
const APPROVAL_TEXT_BYTES: usize = 4096;
const MAX_TITLE_CHARS: usize = 200;
/// Permission modes from least to most permissive.
const MODE_ORDER: [&str; 3] = ["ask", "auto", "full-access"];

pub(super) fn routes(state: &AppState) -> Router<AppState> {
    let tools = AgentsTools {
        state: state.clone(),
        registry: Arc::new(registry_of_tools()),
    };
    registry::mcp_route(state, ROUTE, tools)
}

#[derive(Clone)]
struct AgentsTools {
    state: AppState,
    registry: Arc<ToolRegistry<AgentsTools, AgentsCall>>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ListArgs {}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct RunArgs {
    agent: String,
    text: String,
    #[serde(default)]
    model: Option<String>,
    #[serde(default)]
    reasoning_effort: Option<String>,
    #[serde(default)]
    title: Option<String>,
    #[serde(default)]
    wait_secs: Option<u64>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct TurnArgs {
    conversation_id: String,
    text: String,
    #[serde(default)]
    model: Option<String>,
    #[serde(default)]
    reasoning_effort: Option<String>,
    #[serde(default)]
    wait_secs: Option<u64>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct WaitArgs {
    conversation_id: String,
    #[serde(default)]
    turn_id: Option<String>,
    #[serde(default)]
    wait_secs: Option<u64>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct CancelArgs {
    conversation_id: String,
    #[serde(default)]
    turn_id: Option<String>,
}

struct RunCall {
    agent: ProviderKind,
    text: String,
    model: Option<String>,
    reasoning_effort: Option<String>,
    title: Option<String>,
    wait: Duration,
}

struct TurnCall {
    conversation_id: String,
    text: String,
    model: Option<String>,
    reasoning_effort: Option<String>,
    wait: Duration,
}

enum AgentsCall {
    List,
    Run(RunCall),
    Turn(TurnCall),
    Wait {
        conversation_id: String,
        turn_id: Option<String>,
        wait: Duration,
    },
    Cancel {
        conversation_id: String,
        turn_id: Option<String>,
    },
}

/// `waitSecs`, defaulted and clamped to [`LIMITS`].
fn wait_of(wait_secs: Option<u64>) -> Duration {
    Duration::from_secs(
        wait_secs
            .unwrap_or(LIMITS.default_wait_secs)
            .clamp(1, LIMITS.max_wait_secs),
    )
}

fn validate_text(text: &str) -> Result<(), String> {
    if text.trim().is_empty() {
        return Err("INVALID_REQUEST: text is required".to_owned());
    }
    if text.len() > MAX_TEXT_BYTES {
        return Err(format!(
            "INVALID_REQUEST: text is limited to {MAX_TEXT_BYTES} bytes"
        ));
    }
    Ok(())
}

fn non_empty(value: Option<String>) -> Option<String> {
    value
        .map(|value| value.trim().to_owned())
        .filter(|value| !value.is_empty())
}

/// What an approval shows of a prompt.
fn approval_text(text: &str) -> String {
    if text.len() <= APPROVAL_TEXT_BYTES {
        return text.to_owned();
    }
    let mut end = APPROVAL_TEXT_BYTES;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}… (truncated, {} bytes)", &text[..end], text.len())
}

fn prepare_list(arguments: Value) -> Result<Prepared<AgentsCall>, String> {
    let _: ListArgs = parse("agents_list", arguments)?;
    Ok(Prepared {
        call: AgentsCall::List,
        summary: "list agents".to_owned(),
        input: json!({}),
    })
}

fn prepare_run(arguments: Value) -> Result<Prepared<AgentsCall>, String> {
    let args: RunArgs = parse("agent_run", arguments)?;
    validate_text(&args.text)?;
    let agent: ProviderKind = args
        .agent
        .parse()
        .map_err(|error: String| format!("INVALID_REQUEST: {error}"))?;
    let title = non_empty(args.title).map(|title| title.chars().take(MAX_TITLE_CHARS).collect());
    let input = json!({
        "agent": agent,
        "model": args.model,
        "reasoningEffort": args.reasoning_effort,
        "title": title,
        "text": approval_text(&args.text),
    });
    Ok(Prepared {
        summary: format!("start a {} conversation", profile(agent).display_name),
        input,
        call: AgentsCall::Run(RunCall {
            agent,
            text: args.text,
            model: non_empty(args.model),
            reasoning_effort: non_empty(args.reasoning_effort),
            title,
            wait: wait_of(args.wait_secs),
        }),
    })
}

fn prepare_turn(arguments: Value) -> Result<Prepared<AgentsCall>, String> {
    let args: TurnArgs = parse("agent_turn", arguments)?;
    validate_text(&args.text)?;
    let input = json!({
        "conversationId": args.conversation_id,
        "model": args.model,
        "reasoningEffort": args.reasoning_effort,
        "text": approval_text(&args.text),
    });
    Ok(Prepared {
        summary: "continue an agent call".to_owned(),
        input,
        call: AgentsCall::Turn(TurnCall {
            conversation_id: args.conversation_id,
            text: args.text,
            model: non_empty(args.model),
            reasoning_effort: non_empty(args.reasoning_effort),
            wait: wait_of(args.wait_secs),
        }),
    })
}

fn prepare_wait(arguments: Value) -> Result<Prepared<AgentsCall>, String> {
    let args: WaitArgs = parse("agent_wait", arguments)?;
    Ok(Prepared {
        summary: "wait for an agent call".to_owned(),
        input: json!({ "conversationId": args.conversation_id, "turnId": args.turn_id }),
        call: AgentsCall::Wait {
            conversation_id: args.conversation_id,
            turn_id: non_empty(args.turn_id),
            wait: wait_of(args.wait_secs),
        },
    })
}

fn prepare_cancel(arguments: Value) -> Result<Prepared<AgentsCall>, String> {
    let args: CancelArgs = parse("agent_cancel", arguments)?;
    Ok(Prepared {
        summary: "cancel an agent call".to_owned(),
        input: json!({ "conversationId": args.conversation_id, "turnId": args.turn_id }),
        call: AgentsCall::Cancel {
            conversation_id: args.conversation_id,
            turn_id: non_empty(args.turn_id),
        },
    })
}

fn run<'a>(
    tools: &'a AgentsTools,
    call: AgentsCall,
    invocation: Invocation<'a>,
) -> BoxFuture<'a, CallToolResult> {
    Box::pin(async move {
        let result = match call {
            AgentsCall::List => tools.list(&invocation).await,
            AgentsCall::Run(call) => tools.run_call(&invocation, call).await,
            AgentsCall::Turn(call) => tools.turn_call(&invocation, call).await,
            AgentsCall::Wait {
                conversation_id,
                turn_id,
                wait,
            } => {
                tools
                    .wait_call(&invocation, &conversation_id, turn_id.as_deref(), wait)
                    .await
            }
            AgentsCall::Cancel {
                conversation_id,
                turn_id,
            } => {
                tools
                    .cancel_call(&invocation, &conversation_id, turn_id.as_deref())
                    .await
            }
        };
        match result {
            Ok(value) => CallToolResult::structured(value),
            Err(message) => tool_error(message),
        }
    })
}

/// `CODE: message` of an error passed through to the agent.
fn failure(error: AppError) -> String {
    format!("{}: {error}", error.code())
}

const WAIT_SECS_SCHEMA: &str = "Seconds to wait for the called agent's turn to finish \
     (default 300, 1-1200). When it is still running then, the result has status \"running\" \
     with the output so far; the agent keeps working, and agent_wait picks the result up later.";

fn registry_of_tools() -> ToolRegistry<AgentsTools, AgentsCall> {
    let wait_secs = json!({
        "type": "integer", "minimum": 1, "maximum": LIMITS.max_wait_secs,
        "description": WAIT_SECS_SCHEMA,
    });
    ToolRegistry::new(vec![
        ToolEntry {
            tool: Tool::new(
                "agents_list",
                "List the TodeX agents you may call and their models. Use an agent id and \
                 model id from here with agent_run.",
                schema(json!({ "type": "object", "properties": {}, "additionalProperties": false })),
            )
            .with_annotations(ToolAnnotations::new().read_only(true).open_world(false)),
            side_effect: false,
            prepare: prepare_list,
            run,
        },
        ToolEntry {
            tool: Tool::new(
                "agent_run",
                "Start another TodeX agent on a task in a new conversation in this workspace and \
                 wait for its answer. It runs with this conversation's permission mode (never \
                 more). Returns conversationId, turnId, status (running, completed, failed, \
                 cancelled, interrupted), output (the agent's reply text) and error. Continue the \
                 same conversation with agent_turn. At most 4 calls of this conversation run at \
                 once. Unavailable in Plan mode.",
                schema(json!({
                    "type": "object",
                    "properties": {
                        "agent": { "type": "string", "description": "Agent id from agents_list, e.g. \"codex\" or \"claude-code\"." },
                        "text": { "type": "string", "description": "The task for the agent; include all context it needs." },
                        "model": { "type": "string", "description": "Model id from agents_list (default: the agent's default model)." },
                        "reasoningEffort": { "type": "string", "description": "One of the model's supportedReasoningEfforts." },
                        "title": { "type": "string", "description": "Title of the new conversation." },
                        "waitSecs": wait_secs,
                    },
                    "required": ["agent", "text"],
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
            prepare: prepare_run,
            run,
        },
        ToolEntry {
            tool: Tool::new(
                "agent_turn",
                "Send a follow-up to a conversation you started with agent_run and wait for the \
                 answer. Same result as agent_run. Fails with CONFLICT while that conversation \
                 is still running a turn (use agent_wait first). Unavailable in Plan mode.",
                schema(json!({
                    "type": "object",
                    "properties": {
                        "conversationId": { "type": "string", "description": "conversationId from agent_run." },
                        "text": { "type": "string", "description": "The follow-up message." },
                        "model": { "type": "string", "description": "Model id from agents_list." },
                        "reasoningEffort": { "type": "string", "description": "One of the model's supportedReasoningEfforts." },
                        "waitSecs": wait_secs,
                    },
                    "required": ["conversationId", "text"],
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
            prepare: prepare_turn,
            run,
        },
        ToolEntry {
            tool: Tool::new(
                "agent_wait",
                "Wait for the latest turn of a conversation you started with agent_run (or the \
                 given turnId) to finish. Same result as agent_run; the output of a finished \
                 turn is returned once.",
                schema(json!({
                    "type": "object",
                    "properties": {
                        "conversationId": { "type": "string", "description": "conversationId from agent_run." },
                        "turnId": { "type": "string", "description": "turnId from agent_run or agent_turn (default: the latest)." },
                        "waitSecs": wait_secs,
                    },
                    "required": ["conversationId"],
                    "additionalProperties": false
                })),
            )
            .with_annotations(ToolAnnotations::new().read_only(true).open_world(false)),
            side_effect: false,
            prepare: prepare_wait,
            run,
        },
        ToolEntry {
            tool: Tool::new(
                "agent_cancel",
                "Stop the running turn of a conversation you started with agent_run. Returns \
                 conversationId, cancelled (whether a running turn was told to stop) and turnId.",
                schema(json!({
                    "type": "object",
                    "properties": {
                        "conversationId": { "type": "string", "description": "conversationId from agent_run." },
                        "turnId": { "type": "string", "description": "Only stop this turn." },
                    },
                    "required": ["conversationId"],
                    "additionalProperties": false
                })),
            )
            .with_annotations(
                ToolAnnotations::new()
                    .read_only(false)
                    .destructive(false)
                    .open_world(false),
            ),
            side_effect: false,
            prepare: prepare_cancel,
            run,
        },
    ])
}

impl ToolHost for AgentsTools {
    type Call = AgentsCall;
    const SERVER: &'static str = SERVER_NAME;

    fn registry(&self) -> &ToolRegistry<Self, AgentsCall> {
        &self.registry
    }

    fn mcp(&self) -> &AgentMcp {
        &self.state.agent_mcp
    }

    fn conversations(&self) -> &ConversationSupervisor {
        &self.state.conversations
    }

    async fn precheck(&self, _tool: &str) -> Result<(), String> {
        if self.state.agent_calling.settings().mcp.enabled {
            Ok(())
        } else {
            Err(
                "AGENT_CALLING_DISABLED: agent calling is turned off in TodeX settings (Agent \
                 Calling)."
                    .to_owned(),
            )
        }
    }
}

impl ServerHandler for AgentsTools {
    fn get_info(&self) -> InitializeResult {
        registry::server_info(
            SERVER_NAME,
            "Calls other TodeX agents (Codex, Claude Code, ...) in this workspace. Call \
             agents_list first; agent_run starts an agent on a task and returns its answer, \
             agent_turn continues that conversation, agent_wait collects an answer that took \
             longer than waitSecs, agent_cancel stops it. The called agents' replies are their \
             own work: check them before relying on them.",
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
        // Agents call agents one level deep, and only for paired devices'
        // conversations.
        let caller = registry::caller(&context)?;
        if let Ok(manifest) = self.state.conversations.get(&caller.conversation_id).await {
            if let Err(message) = caller_refusal(&manifest) {
                return Ok(tool_error(message).into());
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

/// Why `manifest`'s conversation may not call agents.
fn caller_refusal(manifest: &ConversationManifest) -> Result<(), String> {
    if manifest.parent_conversation_id.is_some() {
        return Err(
            "UNAUTHORIZED: a conversation started by an agent call cannot call agents itself"
                .to_owned(),
        );
    }
    if manifest.owner_id.starts_with(crate::api_keys::OWNER_PREFIX) {
        return Err("UNAUTHORIZED: API key conversations cannot call agents".to_owned());
    }
    Ok(())
}

/// The permission mode a child of `agent` runs in under a parent turn in
/// `parent_mode`: the most permissive mode the agent supports that does not
/// exceed the parent's.
fn inherited_mode(agent: ProviderKind, parent_mode: &str) -> Result<&'static str, AppError> {
    let rank = |mode: &str| MODE_ORDER.iter().position(|known| *known == mode);
    // Anything unknown counts as the most restrictive.
    let ceiling = rank(parent_mode).unwrap_or(0);
    let config = profile(agent).permission_config;
    config
        .modes
        .iter()
        .copied()
        .filter(|mode| rank(mode).is_some_and(|rank| rank <= ceiling))
        .max_by_key(|mode| rank(mode))
        .ok_or_else(|| {
            AppError::Unauthorized(format!(
                "{} only runs in {} mode, which exceeds this turn's {parent_mode} mode",
                profile(agent).display_name,
                config.modes.join("/"),
            ))
        })
}

impl AgentsTools {
    /// The calling conversation, as an agent caller.
    async fn caller(&self, invocation: &Invocation<'_>) -> Result<AgentToolCaller, String> {
        let parent = self
            .state
            .conversations
            .get(&invocation.caller.conversation_id)
            .await
            .map_err(failure)?;
        caller_refusal(&parent)?;
        Ok(AgentToolCaller { parent })
    }

    /// The parent turn's permission and work mode, inherited by a child of
    /// `agent`.
    fn turn_input(
        &self,
        caller: &AgentToolCaller,
        agent: ProviderKind,
        text: String,
        model: Option<String>,
        reasoning_effort: Option<String>,
    ) -> Result<TurnInput, String> {
        let (permission_mode, work_mode) = self
            .state
            .agent_mcp
            .turn_mode(&caller.parent.id)
            .unwrap_or_else(|| ("ask".to_owned(), "implement".to_owned()));
        if work_mode == "plan" {
            return Err(
                "PLAN_MODE: agents cannot be called while the conversation is in Plan mode"
                    .to_owned(),
            );
        }
        let permission_mode = inherited_mode(agent, &permission_mode).map_err(failure)?;
        Ok(TurnInput {
            text,
            model,
            reasoning_effort,
            skills: Vec::new(),
            client_request_id: None,
            permission_mode: Some(permission_mode.to_owned()),
            work_mode: Some("implement".to_owned()),
        })
    }

    /// `conversation_id` must be a child of the caller.
    async fn child(
        &self,
        caller: &AgentToolCaller,
        conversation_id: &str,
    ) -> Result<ConversationManifest, String> {
        let not_found = || {
            format!(
                "NOT_FOUND: conversation {conversation_id} is not an agent call of this \
                 conversation"
            )
        };
        let manifest = self
            .state
            .conversations
            .get(conversation_id)
            .await
            .map_err(|_| not_found())?;
        if manifest.parent_conversation_id.as_deref() != Some(caller.parent.id.as_str())
            || manifest.owner_id != caller.parent.owner_id
        {
            return Err(not_found());
        }
        Ok(manifest)
    }

    fn reserve(&self, parent_id: &str) -> Result<agent_calls::Reservation, String> {
        self.state.agent_calls.reserve(parent_id).ok_or_else(|| {
            format!(
                "AGENT_CALL_LIMIT: at most {} agent calls of this conversation run at once; \
                 wait for one with agent_wait or stop one with agent_cancel",
                LIMITS.max_concurrent_calls
            )
        })
    }

    /// Waits for the turn unless the tool call itself is cancelled.
    async fn wait(
        &self,
        invocation: &Invocation<'_>,
        child_id: &str,
        turn_id: Option<&str>,
        wait: Duration,
    ) -> Result<Value, String> {
        let calls = &self.state.agent_calls;
        let waited = tokio::select! {
            result = calls.wait(child_id, turn_id, wait) => result,
            () = invocation.context.ct.cancelled() => calls.wait(child_id, turn_id, Duration::ZERO).await,
        };
        waited
            .map(|result: CallResult| result.to_json())
            .map_err(failure)
    }

    async fn list(&self, invocation: &Invocation<'_>) -> Result<Value, String> {
        let caller = self.caller(invocation).await?;
        let agents = agent_calls::list_agents(&self.state, &caller).await;
        Ok(json!({
            "agents": agents.into_iter().map(|agent| json!({
                "agent": agent.id,
                "displayName": agent.display_name,
                "available": agent.available,
                "models": agent.models.into_iter().map(|model| {
                    let mut entry = json!({
                        "id": model.id,
                        "displayName": model.display_name,
                        "isDefault": model.is_default,
                        "supportedReasoningEfforts": model.supported_reasoning_efforts,
                    });
                    if let Some(effort) = model.default_reasoning_effort {
                        entry["defaultReasoningEffort"] = json!(effort);
                    }
                    entry
                }).collect::<Vec<_>>(),
            })).collect::<Vec<_>>(),
            "limits": LIMITS,
        }))
    }

    async fn run_call(&self, invocation: &Invocation<'_>, call: RunCall) -> Result<Value, String> {
        let caller = self.caller(invocation).await?;
        let reservation = self.reserve(&caller.parent.id)?;
        let turn = self.turn_input(
            &caller,
            call.agent,
            call.text.clone(),
            call.model,
            call.reasoning_effort,
        )?;
        self.state.agent_calls.note_parent(&caller.parent.id);
        let title = call
            .title
            .unwrap_or_else(|| format!("Agent call · {}", profile(call.agent).display_name));
        let started = agent_calls::run(
            &self.state,
            &caller,
            RunInput {
                agent: call.agent,
                workspace: None,
                title: Some(title.clone()),
                profile: None,
                turn,
            },
        )
        .await
        .map_err(failure)?;
        let child_id = started.manifest.id.clone();
        let turn_id = started.turn_id.clone();
        self.state
            .agent_calls
            .track(
                reservation,
                &caller.parent,
                started,
                &call.text,
                Some(&title),
            )
            .await;
        self.wait(invocation, &child_id, Some(&turn_id), call.wait)
            .await
    }

    async fn turn_call(
        &self,
        invocation: &Invocation<'_>,
        call: TurnCall,
    ) -> Result<Value, String> {
        let caller = self.caller(invocation).await?;
        let child = self.child(&caller, &call.conversation_id).await?;
        if let Some(turn_id) = self
            .state
            .agent_calls
            .running_turn(&child.id)
            .or_else(|| self.state.conversations.active_turn_id(&child.id))
        {
            return Err(format!(
                "CONFLICT: conversation {} is still running turn {turn_id}; wait for it with \
                 agent_wait or stop it with agent_cancel",
                child.id
            ));
        }
        let reservation = self.reserve(&caller.parent.id)?;
        let turn = self.turn_input(
            &caller,
            child.provider,
            call.text.clone(),
            call.model,
            call.reasoning_effort,
        )?;
        let started = agent_calls::start_turn(&self.state, &caller, &child.id, turn, true)
            .await
            .map_err(failure)?;
        let turn_id = started.turn_id.clone();
        self.state
            .agent_calls
            .track(reservation, &caller.parent, started, &call.text, None)
            .await;
        self.wait(invocation, &child.id, Some(&turn_id), call.wait)
            .await
    }

    async fn wait_call(
        &self,
        invocation: &Invocation<'_>,
        conversation_id: &str,
        turn_id: Option<&str>,
        wait: Duration,
    ) -> Result<Value, String> {
        let caller = self.caller(invocation).await?;
        let child = self.child(&caller, conversation_id).await?;
        self.wait(invocation, &child.id, turn_id, wait).await
    }

    async fn cancel_call(
        &self,
        invocation: &Invocation<'_>,
        conversation_id: &str,
        turn_id: Option<&str>,
    ) -> Result<Value, String> {
        let caller = self.caller(invocation).await?;
        let child = self.child(&caller, conversation_id).await?;
        let running = self.state.conversations.active_turn_id(&child.id);
        let target = turn_id.map(ToOwned::to_owned).or(running.clone());
        let cancelled = match &target {
            Some(target) if running.as_deref() == Some(target.as_str()) => matches!(
                agent_calls::cancel(&self.state, &caller, &child.id, Some(target))
                    .await
                    .map_err(failure)?,
                CancelOutcome::Signalled
            ),
            _ => false,
        };
        Ok(json!({
            "conversationId": child.id,
            "cancelled": cancelled,
            "turnId": target,
        }))
    }
}

#[cfg(all(test, unix))]
mod tests {
    use std::{net::SocketAddr, path::PathBuf};

    use rmcp::{service::RunningService, RoleClient};

    use super::*;
    use crate::agent_calls::AgentCallingUpdate;
    use crate::config::Config;
    use crate::conversation::e2e_support::{all_bytes, contains};

    struct Harness {
        root: PathBuf,
        workspace: PathBuf,
        state: AppState,
        parent: String,
    }

    impl Drop for Harness {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.root);
        }
    }

    /// A daemon whose Antigravity CLI is the stream-json fixture, agent
    /// calling switched on, and a Codex parent conversation in a
    /// full-access turn. The agent MCP endpoint stays unset, so no provider
    /// launch injects TodeX's servers (Antigravity would write them into
    /// the user's global config); tests reach the route directly.
    async fn harness() -> Harness {
        use std::os::unix::fs::PermissionsExt;
        let root = std::env::temp_dir().join(format!(
            "todex-agent-calls-{}",
            uuid::Uuid::new_v4().simple()
        ));
        std::fs::create_dir_all(root.join("workspaces/project")).unwrap();
        let root = std::fs::canonicalize(root).unwrap();
        let workspace = root.join("workspaces/project");
        let agy = root.join("agy-fixture");
        std::fs::write(
            &agy,
            include_str!("../../tests/fixtures/antigravity_stream_fixture.py"),
        )
        .unwrap();
        std::fs::set_permissions(&agy, std::fs::Permissions::from_mode(0o755)).unwrap();
        let mut config = Config {
            port: 0,
            data_dir: root.join("data"),
            workspace_roots: vec![root.join("workspaces")],
            ..Config::default()
        };
        config.agent.antigravity_bin = agy.to_string_lossy().into_owned();
        config.agent.provider_idle_timeout_minutes = 0;
        let state = AppState::new_for_tests(config).await.unwrap();
        update(&state, json!({ "mcp": { "enabled": true } })).await;
        let parent = state
            .conversations
            .create_owned("local", ProviderKind::Codex, workspace.clone(), None, None)
            .await
            .unwrap()
            .id;
        state
            .agent_mcp
            .record_turn_mode(&parent, "full-access", "implement");
        Harness {
            root,
            workspace,
            state,
            parent,
        }
    }

    async fn update(state: &AppState, update: Value) {
        let update: AgentCallingUpdate = serde_json::from_value(update).unwrap();
        state.agent_calling.update(update).await.unwrap();
    }

    type Client = RunningService<RoleClient, rmcp::model::ClientInfo>;

    /// An MCP client of `conversation_id`'s token through the stdio bridge,
    /// against the real router on a loopback port.
    async fn connect(state: &AppState, conversation_id: &str) -> Client {
        use rmcp::ServiceExt;
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let app = crate::server::loopback_test_router(state.clone());
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
        let token = state.agent_mcp.token_for_tests(conversation_id);
        tokio::spawn(async move {
            let _ = super::super::bridge::proxy(&url, &token, bridge_read, bridge_write).await;
        });
        let (read, write) = tokio::io::split(client_side);
        rmcp::model::ClientInfo::default()
            .serve((read, write))
            .await
            .unwrap()
    }

    async fn call(client: &Client, name: &'static str, arguments: Value) -> CallToolResult {
        let Value::Object(arguments) = arguments else {
            unreachable!()
        };
        client
            .call_tool(CallToolRequestParams::new(name).with_arguments(arguments))
            .await
            .unwrap()
    }

    /// The structured result of a successful call.
    fn ok(result: &CallToolResult) -> Value {
        assert_eq!(result.is_error, Some(false), "{result:?}");
        result.structured_content.clone().unwrap()
    }

    /// The `CODE: message` text of a refused call.
    fn refused(result: &CallToolResult) -> String {
        assert_eq!(result.is_error, Some(true), "{result:?}");
        serde_json::to_string(&result.content).unwrap()
    }

    /// The parent's `agent.call.*` events, decrypted.
    async fn call_events(state: &AppState, parent: &str) -> Vec<(String, Value)> {
        state
            .conversations
            .history_for_tests(parent)
            .await
            .into_iter()
            .filter(|event| event.event_type.starts_with("agent.call."))
            .map(|event| (event.event_type, event.payload))
            .collect()
    }

    /// Waits until the parent journalled `count` events of `event_type`.
    async fn wait_for_events(
        state: &AppState,
        parent: &str,
        event_type: &str,
        count: usize,
    ) -> Vec<(String, Value)> {
        for _ in 0..200 {
            let events = call_events(state, parent).await;
            if events.iter().filter(|(kind, _)| kind == event_type).count() >= count {
                return events;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        panic!("no {event_type} in {:?}", call_events(state, parent).await);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn run_and_turn_return_the_called_agents_reply() {
        let harness = harness().await;
        let state = &harness.state;
        let client = connect(state, &harness.parent).await;
        let listed = client.list_tools(None).await.unwrap();
        let names: Vec<_> = listed.tools.iter().map(|tool| tool.name.as_ref()).collect();
        assert_eq!(
            names,
            [
                "agents_list",
                "agent_run",
                "agent_turn",
                "agent_wait",
                "agent_cancel"
            ]
        );
        let agents = ok(&call(&client, "agents_list", json!({})).await);
        assert_eq!(agents["limits"]["maxConcurrentCalls"], 4);
        assert!(agents["agents"]
            .as_array()
            .unwrap()
            .iter()
            .any(|agent| agent["agent"] == "antigravity"));

        let run = ok(&call(
            &client,
            "agent_run",
            json!({ "agent": "antigravity", "text": "hello" }),
        )
        .await);
        assert_eq!(run["status"], "completed", "{run}");
        assert_eq!(run["output"], "Hi there");
        assert_eq!(run["error"], Value::Null);
        let child_id = run["conversationId"].as_str().unwrap().to_owned();
        let child = state.conversations.get(&child_id).await.unwrap();
        assert_eq!(
            child.parent_conversation_id.as_deref(),
            Some(harness.parent.as_str())
        );
        assert_eq!(child.owner_id, "local");
        assert_eq!(child.workspace, harness.workspace);
        // The child ran in the parent turn's mode.
        let history = state.conversations.history_for_tests(&child_id).await;
        let started = history
            .iter()
            .find(|event| event.event_type == "turn.started")
            .unwrap();
        assert_eq!(
            started.payload["effectivePermissions"]["permissionMode"],
            "full-access"
        );

        // The result was handed out: a later wait has the status only.
        let again = ok(&call(
            &client,
            "agent_wait",
            json!({ "conversationId": child_id, "waitSecs": 1 }),
        )
        .await);
        assert_eq!(again["status"], "completed");
        assert_eq!(again["output"], "");

        let turn = ok(&call(
            &client,
            "agent_turn",
            json!({ "conversationId": child_id, "text": "again" }),
        )
        .await);
        assert_eq!(turn["status"], "completed", "{turn}");
        assert_eq!(turn["conversationId"], child_id.as_str());
        assert_ne!(turn["turnId"], run["turnId"]);
        assert_eq!(turn["output"], "Hi there");

        let events = wait_for_events(state, &harness.parent, "agent.call.completed", 2).await;
        let kinds: Vec<_> = events.iter().map(|(kind, _)| kind.as_str()).collect();
        assert_eq!(
            kinds,
            [
                "agent.call.started",
                "agent.call.completed",
                "agent.call.started",
                "agent.call.completed"
            ]
        );
        let (_, first) = &events[0];
        assert_eq!(first["childConversationId"], child_id.as_str());
        assert_eq!(first["childTurnId"], run["turnId"]);
        assert_eq!(first["agent"], "antigravity");
        assert_eq!(first["text"], "hello");
        assert_eq!(first["status"], "running");
        assert!(first["title"].as_str().unwrap().starts_with("Agent call"));
        let (_, completed) = &events[1];
        assert_eq!(completed["output"], "Hi there");
        assert_eq!(completed["status"], "completed");
        assert!(completed["durationMs"].is_u64());
        // Nothing the calls read in plaintext was written in plaintext.
        assert!(!contains(
            &all_bytes(&harness.root.join("data")),
            "Hi there"
        ));
        assert!(!harness.root.join("data").join("agent-calls.json").exists());

        // Deleting the parent takes its child along.
        state
            .conversations
            .delete_owned("local", &harness.parent)
            .await
            .unwrap();
        for _ in 0..200 {
            if state.conversations.get(&child_id).await.is_err() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        assert!(matches!(
            state.conversations.get(&child_id).await,
            Err(AppError::NotFound(_))
        ));
        client.cancel().await.unwrap();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn an_expired_wait_leaves_the_call_running_for_agent_wait() {
        let harness = harness().await;
        let state = &harness.state;
        let client = connect(state, &harness.parent).await;
        let run = ok(&call(
            &client,
            "agent_run",
            json!({ "agent": "antigravity", "text": "SLOW 3", "waitSecs": 1 }),
        )
        .await);
        assert_eq!(run["status"], "running", "{run}");
        let child_id = run["conversationId"].as_str().unwrap().to_owned();
        // Recorded as running for a restart to report.
        let saved: Value = serde_json::from_slice(
            &std::fs::read(harness.root.join("data/agent-calls.json")).unwrap(),
        )
        .unwrap();
        assert_eq!(saved["calls"][0]["childConversationId"], child_id.as_str());
        assert_eq!(
            saved["calls"][0]["parentConversationId"],
            harness.parent.as_str()
        );
        let busy = refused(
            &call(
                &client,
                "agent_turn",
                json!({ "conversationId": child_id, "text": "more" }),
            )
            .await,
        );
        assert!(busy.contains("CONFLICT"), "{busy}");

        let waited = ok(&call(
            &client,
            "agent_wait",
            json!({ "conversationId": child_id, "turnId": run["turnId"], "waitSecs": 60 }),
        )
        .await);
        assert_eq!(waited["status"], "completed", "{waited}");
        assert_eq!(waited["output"], "Hi there");
        wait_for_events(state, &harness.parent, "agent.call.completed", 1).await;
        assert!(!harness.root.join("data/agent-calls.json").exists());
        client.cancel().await.unwrap();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn cancel_stops_a_call_and_permission_waits_are_reported() {
        let harness = harness().await;
        let state = &harness.state;
        let client = connect(state, &harness.parent).await;
        let run = ok(&call(
            &client,
            "agent_run",
            json!({ "agent": "antigravity", "text": "HANG", "waitSecs": 1 }),
        )
        .await);
        assert_eq!(run["status"], "running", "{run}");
        let child_id = run["conversationId"].as_str().unwrap().to_owned();

        // A permission request of the child's turn shows on the parent.
        state
            .conversations
            .append_agent_event(
                &child_id,
                "permission.requested",
                json!({ "permissionId": "p1" }),
            )
            .await
            .unwrap();
        let events = wait_for_events(state, &harness.parent, "agent.call.updated", 1).await;
        let (_, updated) = events.last().unwrap();
        assert_eq!(updated["status"], "waiting_permission");
        assert_eq!(updated["childTurnId"], run["turnId"]);
        state
            .conversations
            .append_agent_event(
                &child_id,
                "permission.resolved",
                json!({ "permissionId": "p1" }),
            )
            .await
            .unwrap();
        let events = wait_for_events(state, &harness.parent, "agent.call.updated", 2).await;
        assert_eq!(events.last().unwrap().1["status"], "running");

        let cancelled = ok(&call(
            &client,
            "agent_cancel",
            json!({ "conversationId": child_id }),
        )
        .await);
        assert_eq!(cancelled["cancelled"], true);
        assert_eq!(cancelled["turnId"], run["turnId"]);
        let waited = ok(&call(
            &client,
            "agent_wait",
            json!({ "conversationId": child_id, "waitSecs": 60 }),
        )
        .await);
        assert_eq!(waited["status"], "cancelled", "{waited}");
        let events = wait_for_events(state, &harness.parent, "agent.call.cancelled", 1).await;
        assert_eq!(events.last().unwrap().1["status"], "cancelled");
        // Nothing left to stop.
        let again = ok(&call(
            &client,
            "agent_cancel",
            json!({ "conversationId": child_id }),
        )
        .await);
        assert_eq!(again["cancelled"], false);
        client.cancel().await.unwrap();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn scope_limit_and_ownership_bound_the_calls() {
        let harness = harness().await;
        let state = &harness.state;
        let client = connect(state, &harness.parent).await;

        update(state, json!({ "scope": { "agents": ["codex"] } })).await;
        let outside = refused(
            &call(
                &client,
                "agent_run",
                json!({ "agent": "antigravity", "text": "hello" }),
            )
            .await,
        );
        assert!(outside.contains("UNAUTHORIZED"), "{outside}");
        let agents = ok(&call(&client, "agents_list", json!({})).await);
        assert!(agents["agents"]
            .as_array()
            .unwrap()
            .iter()
            .all(|agent| agent["agent"] == "codex"));
        update(
            state,
            json!({ "scope": { "agents": null, "models": { "antigravity": ["gemini-3.8-flash-low"] } } }),
        )
        .await;
        let model = refused(
            &call(
                &client,
                "agent_run",
                json!({ "agent": "antigravity", "text": "hello", "model": "other" }),
            )
            .await,
        );
        assert!(model.contains("UNAUTHORIZED"), "{model}");
        update(state, json!({ "scope": { "agents": null, "models": {} } })).await;

        // Four running calls at most.
        let mut children = Vec::new();
        for _ in 0..LIMITS.max_concurrent_calls {
            let run = ok(&call(
                &client,
                "agent_run",
                json!({ "agent": "antigravity", "text": "HANG", "waitSecs": 1 }),
            )
            .await);
            assert_eq!(run["status"], "running", "{run}");
            children.push(run["conversationId"].as_str().unwrap().to_owned());
        }
        let limited = refused(
            &call(
                &client,
                "agent_run",
                json!({ "agent": "antigravity", "text": "HANG", "waitSecs": 1 }),
            )
            .await,
        );
        assert!(limited.contains("AGENT_CALL_LIMIT"), "{limited}");

        // Only the caller's own children are reachable.
        let other = state
            .conversations
            .create_owned(
                "local",
                ProviderKind::Antigravity,
                harness.workspace.clone(),
                None,
                None,
            )
            .await
            .unwrap();
        for (tool, arguments) in [
            (
                "agent_wait",
                json!({ "conversationId": other.id, "waitSecs": 1 }),
            ),
            ("agent_cancel", json!({ "conversationId": other.id })),
            (
                "agent_turn",
                json!({ "conversationId": other.id, "text": "hi" }),
            ),
            (
                "agent_wait",
                json!({ "conversationId": "nope", "waitSecs": 1 }),
            ),
        ] {
            let message = refused(&call(&client, tool, arguments).await);
            assert!(message.contains("NOT_FOUND"), "{tool}: {message}");
        }

        // A child cannot call agents itself.
        let child_client = connect(state, &children[0]).await;
        let nested = refused(
            &call(
                &child_client,
                "agent_run",
                json!({ "agent": "antigravity", "text": "hello" }),
            )
            .await,
        );
        assert!(nested.contains("UNAUTHORIZED"), "{nested}");
        child_client.cancel().await.unwrap();

        for child in &children {
            ok(&call(&client, "agent_cancel", json!({ "conversationId": child })).await);
        }
        wait_for_events(
            state,
            &harness.parent,
            "agent.call.cancelled",
            LIMITS.max_concurrent_calls,
        )
        .await;
        client.cancel().await.unwrap();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn plan_mode_and_the_setting_refuse_calls() {
        let harness = harness().await;
        let state = &harness.state;
        let client = connect(state, &harness.parent).await;
        state
            .agent_mcp
            .record_turn_mode(&harness.parent, "ask", "plan");
        ok(&call(&client, "agents_list", json!({})).await);
        let planning = refused(
            &call(
                &client,
                "agent_run",
                json!({ "agent": "antigravity", "text": "hello" }),
            )
            .await,
        );
        assert!(planning.contains("PLAN_MODE"), "{planning}");

        update(state, json!({ "mcp": { "enabled": false } })).await;
        let disabled = refused(&call(&client, "agents_list", json!({})).await);
        assert!(disabled.contains("AGENT_CALLING_DISABLED"), "{disabled}");
        assert!(call_events(state, &harness.parent).await.is_empty());
        client.cancel().await.unwrap();
    }

    #[test]
    fn children_never_run_above_the_parent_turns_mode() {
        assert_eq!(inherited_mode(ProviderKind::Codex, "auto").unwrap(), "auto");
        assert_eq!(inherited_mode(ProviderKind::Devin, "auto").unwrap(), "ask");
        assert_eq!(
            inherited_mode(ProviderKind::Codex, "unknown").unwrap(),
            "ask"
        );
        assert_eq!(
            inherited_mode(ProviderKind::Pi, "full-access").unwrap(),
            "full-access"
        );
        assert_eq!(
            inherited_mode(ProviderKind::Pi, "ask").unwrap_err().code(),
            "UNAUTHORIZED"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn a_cancelled_parent_turn_cancels_its_calls() {
        let harness = harness().await;
        let state = &harness.state;
        // A parent that really runs a turn (Antigravity: its MCP launch is
        // off without an endpoint).
        let parent = state
            .conversations
            .create_owned(
                "local",
                ProviderKind::Antigravity,
                harness.workspace.clone(),
                None,
                None,
            )
            .await
            .unwrap()
            .id;
        let prompt = crate::provider::ConversationPrompt {
            client_request_id: None,
            text: "HANG".to_owned(),
            model: None,
            reasoning_effort: None,
            skills: Vec::new(),
            content: Vec::new(),
            permission_mode: Some("full-access".to_owned()),
            work_mode: None,
            permission_profile: None,
            sandbox_mode: None,
            approval_policy: None,
        };
        state
            .conversations
            .prompt_owned("local", &parent, prompt)
            .await
            .unwrap();
        let client = connect(state, &parent).await;
        let run = ok(&call(
            &client,
            "agent_run",
            json!({ "agent": "antigravity", "text": "HANG", "waitSecs": 1 }),
        )
        .await);
        assert_eq!(run["status"], "running", "{run}");
        let child_id = run["conversationId"].as_str().unwrap().to_owned();
        let parent_turn = state.conversations.active_turn_id(&parent).unwrap();
        let events = call_events(state, &parent).await;
        // The parent's own turn is stamped into its call events.
        assert_eq!(events[0].1["turnId"], parent_turn.as_str());

        state
            .conversations
            .cancel_owned("local", &parent, None)
            .await
            .unwrap();
        wait_for_events(state, &parent, "agent.call.cancelled", 1).await;
        for _ in 0..200 {
            if state.conversations.active_turn_id(&child_id).is_none() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        assert!(state.conversations.active_turn_id(&child_id).is_none());
        client.cancel().await.unwrap();
    }

    #[tokio::test]
    async fn children_follow_the_mcp_approval_policy() {
        use crate::provider::PermissionPolicy;
        let harness = harness().await;
        let state = &harness.state;
        let child = state
            .conversations
            .create_child_owned(
                "local",
                ProviderKind::Antigravity,
                harness.workspace.clone(),
                None,
                None,
                &harness.parent,
            )
            .await
            .unwrap();
        update(state, json!({ "mcp": { "approval": "auto-approve" } })).await;
        let policy = crate::agent_calls::AgentCallPermissionPolicy {
            conversations: state.conversation_store().clone(),
            settings: state.agent_calling.clone(),
        };
        let options = json!([
            { "optionId": "yes", "kind": "allow_once" },
            { "optionId": "no", "kind": "reject_once" },
        ]);
        let (decision, principal) = policy.decide(&child.id, &options, false).await.unwrap();
        assert_eq!(decision.option_id.as_deref(), Some("yes"));
        assert_eq!(principal, crate::agent_calls::AGENT_CALL_POLICY_PRINCIPAL);
        // The parent is not an agent call: its own clients answer.
        assert!(policy
            .decide(&harness.parent, &options, false)
            .await
            .is_none());
    }

    #[tokio::test]
    async fn calls_cut_off_by_a_restart_fail_on_their_parent() {
        let harness = harness().await;
        let state = &harness.state;
        let path = harness.root.join("data/agent-calls.json");
        std::fs::write(
            &path,
            json!({ "calls": [{
                "parentConversationId": harness.parent,
                "childConversationId": "child-1",
                "childTurnId": "turn_1",
                "agent": "codex",
                "startedAt": "2026-10-18T00:00:00Z",
            }] })
            .to_string(),
        )
        .unwrap();
        state.agent_calls.start().await;
        let events = call_events(state, &harness.parent).await;
        assert_eq!(events.len(), 1);
        let (kind, payload) = &events[0];
        assert_eq!(kind, "agent.call.failed");
        assert_eq!(payload["childConversationId"], "child-1");
        assert_eq!(payload["childTurnId"], "turn_1");
        assert_eq!(payload["status"], "failed");
        assert_eq!(payload["error"]["code"], "DAEMON_RESTARTED");
        assert!(!path.exists());
    }

    #[tokio::test]
    async fn the_tap_reads_plaintext_that_is_only_stored_sealed() {
        let harness = harness().await;
        let state = &harness.state;
        let mut tap = state
            .conversations
            .live_tap(&harness.parent, crate::agent_calls::tap_projection);
        let marker = "tap-marker-6f1c2a";
        state
            .conversations
            .append_agent_event(
                &harness.parent,
                "message.delta",
                json!({ "delta": marker, "turnId": "t1" }),
            )
            .await
            .unwrap();
        state
            .conversations
            .append_agent_event(&harness.parent, "tool.output", json!({ "output": "x" }))
            .await
            .unwrap();
        let batch = tap.recv_batch().await.unwrap();
        assert_eq!(batch.len(), 1, "only the projection is delivered");
        assert_eq!(batch[0].payload["delta"], marker);
        // Journalled (readable to clients), but only sealed on disk.
        let history = state.conversations.history_for_tests(&harness.parent).await;
        assert!(history.iter().any(|event| event.payload["delta"] == marker));
        assert!(!contains(&all_bytes(&harness.root.join("data")), marker));
        // Removing the conversation ends the tap.
        state
            .conversations
            .delete_owned("local", &harness.parent)
            .await
            .unwrap();
        assert!(tap.recv_batch().await.is_none());
    }
}
