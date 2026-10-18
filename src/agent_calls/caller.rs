//! Who makes an agent call: what it owns, which agents and workspaces it
//! may reach on top of the global [`super::AgentScope`], and where it
//! reads events from.
//!
//! [`ApiKeyCaller`] is the external API. [`AgentToolCaller`] is an agent
//! calling another agent (the `todex_agents` MCP server): it owns its
//! children as the parent conversation's owner, is fixed to the parent's
//! workspace, and reads a plaintext tap instead of decrypting.

use std::{path::PathBuf, sync::Arc};

use async_trait::async_trait;
use zeroize::Zeroizing;

use super::events::{tap_projection, EventPump, TurnEvents};
use crate::api_keys::ApiKeyRecord;
use crate::app_state::AppState;
use crate::conversation::server_decrypt::HistoryReader;
use crate::conversation::{ConversationManifest, LiveTap, ProviderKind};
use crate::error::AppError;
use crate::workspace_paths::{validate_workspace_directory, validate_workspace_directory_text};

#[async_trait]
pub(crate) trait Caller: Send + Sync {
    /// The events [`Self::subscribe`] yields.
    type Events: TurnEvents;

    /// Owner id of the conversations this caller creates and reaches.
    fn owner_id(&self) -> String;

    /// The caller's own agent restriction; the global scope is checked
    /// separately by [`super::ensure_agent`].
    fn check_agent(&self, agent: ProviderKind) -> Result<(), AppError>;

    /// The canonical workspace a new conversation of this caller runs in,
    /// given the one it asked for.
    fn authorize_workspace(
        &self,
        state: &AppState,
        requested: Option<&str>,
    ) -> Result<PathBuf, AppError>;

    /// `conversation_id`'s events after sequence `after`. Subscribed before
    /// a prompt from the conversation's `last_sequence`, it starts with the
    /// turn's first event.
    async fn subscribe(
        &self,
        state: &AppState,
        conversation_id: &str,
        after: u64,
    ) -> Result<Self::Events, AppError>;

    /// The conversation this caller's conversations are children of (an
    /// agent calling agents); `None` for top-level conversations.
    fn parent_conversation_id(&self) -> Option<&str> {
        None
    }
}

/// The key a request authenticated with, and the history recipient seed
/// its secret derives; see [`crate::conversation::server_decrypt`]. Owns
/// its conversations as `apikey:<id>`.
#[derive(Clone)]
pub(crate) struct ApiKeyCaller {
    pub record: ApiKeyRecord,
    pub seed: Arc<Zeroizing<[u8; 32]>>,
}

impl ApiKeyCaller {
    pub(crate) fn owner_id(&self) -> String {
        self.record.owner_id()
    }

    /// Decrypts `conversation_id`'s history for this key.
    pub(crate) fn reader(&self, state: &AppState, conversation_id: &str) -> HistoryReader {
        HistoryReader::new(
            state.history_keys.clone(),
            conversation_id,
            self.seed.clone(),
        )
    }
}

#[async_trait]
impl Caller for ApiKeyCaller {
    type Events = EventPump;

    fn owner_id(&self) -> String {
        self.record.owner_id()
    }

    fn check_agent(&self, agent: ProviderKind) -> Result<(), AppError> {
        if self.record.scopes.allows_agent(agent) {
            Ok(())
        } else {
            Err(AppError::Unauthorized(format!(
                "agent {} is outside this API key's scope",
                agent.as_str()
            )))
        }
    }

    /// The path must be inside a workspace root and the key's scope.
    fn authorize_workspace(
        &self,
        state: &AppState,
        requested: Option<&str>,
    ) -> Result<PathBuf, AppError> {
        let requested = requested
            .ok_or_else(|| AppError::InvalidRequest("workspace is required".to_owned()))?;
        let workspace =
            validate_workspace_directory_text(&state.config.workspace_roots, requested)?;
        if !self.record.scopes.allows_workspace(&workspace) {
            return Err(AppError::Unauthorized(
                "workspace is outside this API key's scope".to_owned(),
            ));
        }
        Ok(workspace)
    }

    async fn subscribe(
        &self,
        state: &AppState,
        conversation_id: &str,
        after: u64,
    ) -> Result<EventPump, AppError> {
        EventPump::new(state, self, conversation_id, after).await
    }
}

/// An agent calling other agents through `todex_agents`: the conversation
/// it runs in (the parent). Its calls create children of that
/// conversation, owned by the same owner, in the same workspace, and follow
/// them through a [`LiveTap`].
#[derive(Clone)]
pub(crate) struct AgentToolCaller {
    pub parent: ConversationManifest,
}

#[async_trait]
impl Caller for AgentToolCaller {
    type Events = LiveTap;

    fn owner_id(&self) -> String {
        self.parent.owner_id.clone()
    }

    /// Only the global scope applies.
    fn check_agent(&self, _agent: ProviderKind) -> Result<(), AppError> {
        Ok(())
    }

    /// Always the parent's workspace, which must still be accessible.
    fn authorize_workspace(
        &self,
        state: &AppState,
        requested: Option<&str>,
    ) -> Result<PathBuf, AppError> {
        let workspace = validate_workspace_directory(
            &state.config.workspace_access_roots(),
            &self.parent.workspace,
        )?;
        if let Some(requested) = requested {
            let requested = validate_workspace_directory_text(
                &state.config.workspace_access_roots(),
                requested,
            )?;
            if requested != workspace {
                return Err(AppError::Unauthorized(
                    "agent calls run in the calling conversation's workspace".to_owned(),
                ));
            }
        }
        Ok(workspace)
    }

    /// Taps the child from its next event on. `after` is implied: nothing
    /// before the tap is delivered.
    async fn subscribe(
        &self,
        state: &AppState,
        conversation_id: &str,
        _after: u64,
    ) -> Result<LiveTap, AppError> {
        let manifest = state
            .conversations
            .get_owned(&self.owner_id(), conversation_id)
            .await?;
        if manifest.parent_conversation_id.as_deref() != Some(self.parent.id.as_str()) {
            return Err(AppError::NotFound(format!(
                "conversation {conversation_id} is not an agent call of this conversation"
            )));
        }
        // Refused while another call follows the child, which is never cut
        // off.
        state
            .conversations
            .live_tap(conversation_id, tap_projection)
    }

    fn parent_conversation_id(&self) -> Option<&str> {
        Some(&self.parent.id)
    }
}
