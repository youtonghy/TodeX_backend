//! Standing answers to permission requests nobody is around to answer:
//! API key conversations follow their key's `approval`, agent-call child
//! conversations follow `mcp.approval` from `agent-calling.json`. The
//! daemon installs [`CompositePermissionPolicy`] over both.

use std::sync::Arc;

use async_trait::async_trait;
use serde_json::Value;

use super::settings::AgentCallingStore;
use crate::api_keys::{ApiKeyStore, ApprovalPolicy};
use crate::conversation::{ConversationManifest, ConversationStore};
use crate::provider::{PermissionDecision, PermissionOutcome, PermissionPolicy};

/// Asks each policy in turn; the first decision wins.
pub(crate) struct CompositePermissionPolicy {
    policies: Vec<Arc<dyn PermissionPolicy>>,
}

impl CompositePermissionPolicy {
    pub(crate) fn new(policies: Vec<Arc<dyn PermissionPolicy>>) -> Self {
        Self { policies }
    }

    /// The daemon's policy: API keys, then agent-call children.
    pub(crate) fn for_daemon(
        conversations: ConversationStore,
        keys: ApiKeyStore,
        settings: AgentCallingStore,
    ) -> Self {
        Self::new(vec![
            Arc::new(ApiKeyPermissionPolicy {
                state_conversations: conversations.clone(),
                keys,
            }),
            Arc::new(AgentCallPermissionPolicy {
                conversations,
                settings,
            }),
        ])
    }
}

#[async_trait]
impl PermissionPolicy for CompositePermissionPolicy {
    async fn decide(
        &self,
        conversation_id: &str,
        options: &Value,
        device_bound: bool,
    ) -> Option<(PermissionDecision, String)> {
        for policy in &self.policies {
            if let Some(decision) = policy.decide(conversation_id, options, device_bound).await {
                return Some(decision);
            }
        }
        None
    }
}

/// Answers permission requests of API key conversations whose key is set
/// to `auto-approve` or `reject`.
pub(crate) struct ApiKeyPermissionPolicy {
    pub state_conversations: ConversationStore,
    pub keys: ApiKeyStore,
}

#[async_trait]
impl PermissionPolicy for ApiKeyPermissionPolicy {
    async fn decide(
        &self,
        conversation_id: &str,
        options: &Value,
        device_bound: bool,
    ) -> Option<(PermissionDecision, String)> {
        let manifest = self.state_conversations.get(conversation_id).await.ok()?;
        let id = manifest
            .owner_id
            .strip_prefix(crate::api_keys::OWNER_PREFIX)?;
        let record = self.keys.get(id).ok().flatten()?;
        let decision = standing_decision(record.approval, options, device_bound)?;
        Some((decision, format!("apikey-policy:{id}")))
    }
}

/// Answers permission requests of agent-call child conversations by
/// `mcp.approval`.
pub(crate) struct AgentCallPermissionPolicy {
    pub conversations: ConversationStore,
    pub settings: AgentCallingStore,
}

/// Principal recorded for answers given by `mcp.approval`.
pub(crate) const AGENT_CALL_POLICY_PRINCIPAL: &str = "agent-call-policy";

#[async_trait]
impl PermissionPolicy for AgentCallPermissionPolicy {
    async fn decide(
        &self,
        conversation_id: &str,
        options: &Value,
        device_bound: bool,
    ) -> Option<(PermissionDecision, String)> {
        let manifest = self.conversations.get(conversation_id).await.ok()?;
        if !is_agent_call_child(&manifest) {
            return None;
        }
        let approval = self.settings.settings().mcp.approval;
        let decision = standing_decision(approval, options, device_bound)?;
        Some((decision, AGENT_CALL_POLICY_PRINCIPAL.to_owned()))
    }
}

/// Whether `manifest` is a conversation another agent started through the
/// `todex_agents` MCP server.
///
/// Hook: no conversation is one yet. Once `ConversationManifest` carries
/// `parentConversationId`, this becomes
/// `manifest.parent_conversation_id.is_some()`, and
/// [`AgentCallPermissionPolicy`] answers those conversations.
pub(crate) fn is_agent_call_child(manifest: &ConversationManifest) -> bool {
    let _ = manifest;
    false
}

/// The option `approval` picks among `options`, or `None` to leave the
/// request to the conversation's clients. A device-bound request is never
/// approved on a standing policy's behalf.
fn standing_decision(
    approval: ApprovalPolicy,
    options: &Value,
    device_bound: bool,
) -> Option<PermissionDecision> {
    let wanted: &[PermissionOutcome] = match (approval, device_bound) {
        (ApprovalPolicy::Ask, _) => return None,
        (ApprovalPolicy::AutoApprove, false) => &[PermissionOutcome::AllowOnce],
        (ApprovalPolicy::AutoApprove, true) | (ApprovalPolicy::Reject, _) => &[
            PermissionOutcome::RejectOnce,
            PermissionOutcome::RejectAlways,
            PermissionOutcome::AbortTurn,
        ],
    };
    let options = options.as_array()?;
    let (outcome, option_id) = wanted.iter().find_map(|outcome| {
        options
            .iter()
            .find(|option| option.get("kind").and_then(Value::as_str) == Some(outcome.as_str()))
            .map(|option| {
                (
                    *outcome,
                    option
                        .get("optionId")
                        .and_then(Value::as_str)
                        .map(ToOwned::to_owned),
                )
            })
    })?;
    Some(PermissionDecision {
        outcome,
        option_id,
        data: None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn standing_decisions_pick_the_matching_option() {
        let options = json!([
            { "optionId": "yes", "kind": "allow_once" },
            { "optionId": "no", "kind": "reject_once" },
        ]);
        assert!(standing_decision(ApprovalPolicy::Ask, &options, false).is_none());
        let pick = |approval, device_bound| {
            standing_decision(approval, &options, device_bound)
                .and_then(|decision| decision.option_id)
        };
        assert_eq!(
            pick(ApprovalPolicy::AutoApprove, false).as_deref(),
            Some("yes")
        );
        assert_eq!(
            pick(ApprovalPolicy::AutoApprove, true).as_deref(),
            Some("no")
        );
        assert_eq!(pick(ApprovalPolicy::Reject, false).as_deref(), Some("no"));
        assert!(standing_decision(ApprovalPolicy::Reject, &json!([]), false).is_none());
    }
}
