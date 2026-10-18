//! Agent calling: one call layer for both entry points, the external API
//! (`/api/v1`, API keys) and the `todex_agents` MCP server (an agent
//! calling another agent). Each entry is a thin adapter over the
//! operations here; a [`Caller`] supplies what differs (owner, own scope,
//! workspace, event source), and the global [`AgentScope`] from
//! `agent-calling.json` applies to every caller.

mod caller;
mod calls;
mod events;
mod policy;
mod settings;

use std::{path::Path, time::Duration};

use serde::Serialize;
use serde_json::Value;

use crate::app_state::AppState;
use crate::conversation::{ConversationManifest, ProviderKind};
use crate::error::AppError;
use crate::provider::types::{ProviderModelDescriptor, ProviderSnapshot};
use crate::provider::{CancelOutcome, ConversationPrompt, PermissionDecision, PromptSkillRef};

pub(crate) use caller::{AgentToolCaller, ApiKeyCaller, Caller};
pub(crate) use calls::{AgentCalls, CallResult, Reservation};
#[cfg(test)]
pub(crate) use events::tap_projection;
pub(crate) use events::{assistant_text, ends_turn, EventPump, TurnEvents};
#[allow(unused_imports)]
pub(crate) use policy::{
    is_agent_call_child, AgentCallPermissionPolicy, ApiKeyPermissionPolicy,
    CompositePermissionPolicy, AGENT_CALL_POLICY_PRINCIPAL,
};
#[allow(unused_imports)]
pub(crate) use settings::{
    AgentCallingSettings, AgentCallingStore, AgentCallingUpdate, AgentScope, ApiListenerView,
    McpSettings, McpUpdate,
};

/// Bounds of MCP agent calls, reported to clients with the settings.
#[derive(Clone, Copy, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct CallLimits {
    /// Running child conversations per parent conversation.
    pub max_concurrent_calls: usize,
    pub max_wait_secs: u64,
    pub default_wait_secs: u64,
}

pub(crate) const LIMITS: CallLimits = CallLimits {
    max_concurrent_calls: 4,
    max_wait_secs: 1200,
    default_wait_secs: 300,
};

/// What a turn may set. Permission, sandbox and approval settings are not
/// accepted from callers: the caller's approval policy governs them.
#[derive(Clone, Debug, Default)]
pub(crate) struct TurnInput {
    pub text: String,
    pub model: Option<String>,
    pub reasoning_effort: Option<String>,
    pub skills: Vec<PromptSkillRef>,
    pub client_request_id: Option<String>,
    /// Set only by agent calls: a child turn runs in its parent turn's
    /// permission mode.
    pub permission_mode: Option<String>,
    /// Set only by agent calls: the parent turn's work mode.
    pub work_mode: Option<String>,
}

impl TurnInput {
    /// Rejects a turn without text.
    pub(crate) fn validate(&self) -> Result<(), AppError> {
        if self.text.trim().is_empty() {
            return Err(AppError::InvalidRequest("text is required".to_owned()));
        }
        Ok(())
    }
}

/// A new conversation and its first turn.
#[derive(Clone, Debug)]
pub(crate) struct RunInput {
    pub agent: ProviderKind,
    pub workspace: Option<String>,
    pub title: Option<String>,
    pub profile: Option<String>,
    pub turn: TurnInput,
}

/// A started turn. `events`, when requested, starts with the turn's first
/// event.
pub(crate) struct StartedTurn<E> {
    /// The conversation as it was just before the turn.
    pub manifest: ConversationManifest,
    pub turn_id: String,
    pub events: Option<E>,
    /// The model the turn was sent with (resolved within the scope).
    pub model: Option<String>,
    pub reasoning_effort: Option<String>,
}

/// How waiting for a turn ended.
#[derive(Clone, Debug, PartialEq)]
pub(crate) enum TurnOutcome {
    /// The turn ended: `status` is the terminal event without its
    /// `turn.`/`conversation.` prefix (`completed`, `failed`, `cancelled`,
    /// `interrupted`, ...); `error` is set unless it completed.
    Finished {
        status: String,
        output: String,
        error: Option<Value>,
    },
    /// The wait expired with the turn still running; `output` is the
    /// assistant text so far.
    Pending { output: String },
}

/// `agent` must be inside the global scope and the caller's.
pub(crate) fn ensure_agent<C: Caller + ?Sized>(
    state: &AppState,
    caller: &C,
    agent: ProviderKind,
) -> Result<(), AppError> {
    if !state.agent_calling.scope().allows_agent(agent) {
        return Err(AppError::Unauthorized(format!(
            "agent {} is not enabled for agent calling",
            agent.as_str()
        )));
    }
    caller.check_agent(agent)
}

/// The providers `caller` may call, each with the models the global scope
/// allows.
pub(crate) async fn list_agents<C: Caller + ?Sized>(
    state: &AppState,
    caller: &C,
) -> Vec<ProviderSnapshot> {
    let scope = state.agent_calling.scope();
    state
        .conversations
        .providers_snapshot()
        .await
        .into_iter()
        .filter(|snapshot| ensure_agent(state, caller, snapshot.id).is_ok())
        .map(|mut snapshot| {
            let agent = snapshot.id;
            snapshot
                .models
                .retain(|model| scope.allows_model(agent, &model.id));
            snapshot
        })
        .collect()
}

/// `agent`'s live models in `workspace` that the global scope allows.
pub(crate) async fn models<C: Caller + ?Sized>(
    state: &AppState,
    caller: &C,
    agent: ProviderKind,
    workspace: Option<&str>,
) -> Result<Vec<ProviderModelDescriptor>, AppError> {
    ensure_agent(state, caller, agent)?;
    let workspace = caller.authorize_workspace(state, workspace)?;
    let mut models = state.conversations.models_live(agent, &workspace).await?;
    let scope = state.agent_calling.scope();
    models.retain(|model| scope.allows_model(agent, &model.id));
    Ok(models)
}

/// A new conversation of `agent` owned by `caller`.
pub(crate) async fn create<C: Caller + ?Sized>(
    state: &AppState,
    caller: &C,
    agent: ProviderKind,
    workspace: Option<&str>,
    title: Option<String>,
    profile: Option<String>,
) -> Result<ConversationManifest, AppError> {
    ensure_agent(state, caller, agent)?;
    let workspace = caller.authorize_workspace(state, workspace)?;
    create_conversation(state, caller, agent, workspace, title, profile).await
}

/// Creates `caller`'s conversation; an agent caller's is a child of its
/// conversation.
async fn create_conversation<C: Caller + ?Sized>(
    state: &AppState,
    caller: &C,
    agent: ProviderKind,
    workspace: std::path::PathBuf,
    title: Option<String>,
    profile: Option<String>,
) -> Result<ConversationManifest, AppError> {
    let owner = caller.owner_id();
    match caller.parent_conversation_id() {
        Some(parent) => {
            state
                .conversations
                .create_child_owned(&owner, agent, workspace, title, profile, parent)
                .await
        }
        None => {
            state
                .conversations
                .create_owned(&owner, agent, workspace, title, profile)
                .await
        }
    }
}

/// The prompt a turn of `agent` in `workspace` sends: the model resolved
/// within the global scope, and the reasoning effort checked against the
/// model's supported efforts when the provider lists them.
pub(crate) async fn prepare_prompt(
    state: &AppState,
    agent: ProviderKind,
    workspace: &Path,
    input: TurnInput,
) -> Result<ConversationPrompt, AppError> {
    input.validate()?;
    let scope = state.agent_calling.scope();
    let needs_catalog = input.reasoning_effort.is_some()
        || (input.model.is_none() && scope.allowed_models(agent).is_some());
    // Discovery failing leaves the model to the scope alone and the effort
    // to the provider.
    let catalog = if needs_catalog {
        state.conversations.models_live(agent, workspace).await.ok()
    } else {
        None
    };
    let model = scope.resolve_model(agent, input.model.as_deref(), catalog.as_deref())?;
    if let (Some(effort), Some(catalog)) = (&input.reasoning_effort, &catalog) {
        let descriptor = match &model {
            Some(id) => catalog.iter().find(|candidate| &candidate.id == id),
            None => catalog.iter().find(|candidate| candidate.is_default),
        };
        if let Some(descriptor) = descriptor {
            if !descriptor.supported_reasoning_efforts.is_empty()
                && !descriptor
                    .supported_reasoning_efforts
                    .iter()
                    .any(|supported| supported == effort)
            {
                return Err(AppError::Unauthorized(format!(
                    "reasoning effort {effort} is not supported by model {}",
                    descriptor.id
                )));
            }
        }
    }
    Ok(ConversationPrompt {
        permission_mode: input.permission_mode,
        work_mode: input.work_mode,
        client_request_id: input.client_request_id,
        text: input.text,
        model,
        reasoning_effort: input.reasoning_effort,
        skills: input.skills,
        content: Vec::new(),
        permission_profile: None,
        sandbox_mode: None,
        approval_policy: None,
    })
}

/// Starts a turn in `caller`'s conversation. With `follow`, the returned
/// events are subscribed before the prompt.
pub(crate) async fn start_turn<C: Caller + ?Sized>(
    state: &AppState,
    caller: &C,
    conversation_id: &str,
    input: TurnInput,
    follow: bool,
) -> Result<StartedTurn<C::Events>, AppError> {
    input.validate()?;
    let owner = caller.owner_id();
    let manifest = state
        .conversations
        .get_owned(&owner, conversation_id)
        .await?;
    ensure_agent(state, caller, manifest.provider)?;
    let prompt = prepare_prompt(state, manifest.provider, &manifest.workspace, input).await?;
    let (model, reasoning_effort) = (prompt.model.clone(), prompt.reasoning_effort.clone());
    let events = if follow {
        Some(
            caller
                .subscribe(state, conversation_id, manifest.last_sequence)
                .await?,
        )
    } else {
        None
    };
    let turn_id = state
        .conversations
        .prompt_owned(&owner, conversation_id, prompt)
        .await?;
    Ok(StartedTurn {
        manifest,
        turn_id,
        events,
        model,
        reasoning_effort,
    })
}

/// One call: a new conversation and its first turn, followed from the
/// turn's first event (`events` is always set).
pub(crate) async fn run<C: Caller + ?Sized>(
    state: &AppState,
    caller: &C,
    input: RunInput,
) -> Result<StartedTurn<C::Events>, AppError> {
    input.turn.validate()?;
    let agent = input.agent;
    ensure_agent(state, caller, agent)?;
    let workspace = caller.authorize_workspace(state, input.workspace.as_deref())?;
    // Before the conversation exists, so a refused model leaves nothing.
    let prompt = prepare_prompt(state, agent, &workspace, input.turn).await?;
    let (model, reasoning_effort) = (prompt.model.clone(), prompt.reasoning_effort.clone());
    let owner = caller.owner_id();
    let manifest =
        create_conversation(state, caller, agent, workspace, input.title, input.profile).await?;
    let events = caller
        .subscribe(state, &manifest.id, manifest.last_sequence)
        .await?;
    let turn_id = state
        .conversations
        .prompt_owned(&owner, &manifest.id, prompt)
        .await?;
    Ok(StartedTurn {
        manifest,
        turn_id,
        events: Some(events),
        model,
        reasoning_effort,
    })
}

/// Waits up to `wait` for `turn_id` to end, appending its main-agent
/// assistant text to `output` (which carries over between waits). A wait
/// that expires leaves the turn running.
pub(crate) async fn await_turn<E: TurnEvents + ?Sized>(
    events: &mut E,
    turn_id: &str,
    wait: Duration,
    output: &mut String,
) -> Result<TurnOutcome, AppError> {
    let collected = tokio::time::timeout(wait, async {
        loop {
            for event in events.next_batch().await? {
                let ours = event.payload.get("turnId").and_then(Value::as_str) == Some(turn_id);
                if ours {
                    if let Some(text) = assistant_text(&event) {
                        output.push_str(text);
                    }
                }
                if ends_turn(&event, Some(turn_id)) {
                    return Ok::<_, AppError>(event);
                }
            }
        }
    })
    .await;
    let terminal = match collected {
        Ok(result) => result?,
        Err(_) => {
            return Ok(TurnOutcome::Pending {
                output: output.clone(),
            })
        }
    };
    let status = terminal
        .event_type
        .strip_prefix("turn.")
        .or_else(|| terminal.event_type.strip_prefix("conversation."))
        .unwrap_or(&terminal.event_type)
        .to_owned();
    let error = terminal
        .payload
        .get("error")
        .or_else(|| terminal.payload.get("message"))
        .filter(|_| status != "completed")
        .cloned();
    Ok(TurnOutcome::Finished {
        status,
        output: output.clone(),
        error,
    })
}

/// Cancels the running turn of `caller`'s conversation (only `turn_id`, if
/// given).
pub(crate) async fn cancel<C: Caller + ?Sized>(
    state: &AppState,
    caller: &C,
    conversation_id: &str,
    turn_id: Option<&str>,
) -> Result<CancelOutcome, AppError> {
    state
        .conversations
        .cancel_owned(&caller.owner_id(), conversation_id, turn_id)
        .await
}

/// Answers a permission request of `caller`'s conversation as `caller`.
pub(crate) async fn resolve_permission<C: Caller + ?Sized>(
    state: &AppState,
    caller: &C,
    conversation_id: &str,
    permission_id: &str,
    decision: PermissionDecision,
) -> Result<(), AppError> {
    let owner = caller.owner_id();
    state
        .conversations
        .resolve_permission_owned(&owner, &owner, conversation_id, permission_id, decision)
        .await
}
