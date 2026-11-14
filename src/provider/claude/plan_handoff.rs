//! Smart work mode: a Plan-mode turn whose prompt carries `afterPlan` hands
//! the live Claude process over to execution when the user approves Claude's
//! `ExitPlanMode`.
//!
//! Before the approval is sent, the turn asks Claude to switch model
//! (`set_model`), permission mode (`set_permission_mode`) and, when one was
//! chosen, reasoning effort (`apply_flag_settings` with `effortLevel`, which
//! Claude applies from its next turn). Claude answers each control request
//! with a `control_response` carrying the same `request_id`; the read loop
//! routes those answers here ([`ControlReplies`]). The rest of the turn then
//! runs on the execute settings, and the resident process's launch key is
//! updated so the next implement prompt with those settings reuses it.
//!
//! A switch Claude refuses or never answers still lets the approval through
//! (the user approved the plan); the settings then apply at the next prompt,
//! whose differing launch key restarts the process with the right flags.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde_json::{json, Value};
use tokio::sync::{mpsc, oneshot};

use crate::conversation::ProviderKind;
use crate::provider::types::AfterPlan;

/// The tool Claude calls to leave Plan mode with a plan for approval.
pub(super) const EXIT_PLAN_MODE_TOOL: &str = "ExitPlanMode";

/// How long one switch waits for Claude's answer.
const REPLY_TIMEOUT: Duration = Duration::from_secs(10);

/// Unique `request_id` suffixes for the switch requests.
static NEXT_REQUEST: AtomicU64 = AtomicU64::new(1);

/// What a turn's detached control-request tasks hand the read loop, in
/// order: a frame to write to Claude's stdin, or a finished handoff to apply
/// to the turn before the approval that follows it is written.
pub(super) enum TurnMessage {
    Send(Value),
    HandedOff(HandoffOutcome),
}

/// Control requests TodeX sent to Claude mid-turn that a task waits on, by
/// `request_id`.
#[derive(Clone, Default)]
pub(super) struct ControlReplies(Arc<Mutex<HashMap<String, oneshot::Sender<Value>>>>);

impl ControlReplies {
    fn expect(&self, request_id: &str) -> oneshot::Receiver<Value> {
        let (sender, receiver) = oneshot::channel();
        self.0
            .lock()
            .expect("Claude control replies lock")
            .insert(request_id.to_owned(), sender);
        receiver
    }

    fn forget(&self, request_id: &str) {
        self.0
            .lock()
            .expect("Claude control replies lock")
            .remove(request_id);
    }

    /// Hands a `control_response` frame to the task waiting on its
    /// `request_id`; false when nobody waits on it.
    pub(super) fn deliver(&self, message: &Value) -> bool {
        let Some(request_id) = message
            .pointer("/response/request_id")
            .and_then(Value::as_str)
        else {
            return false;
        };
        let waiting = self
            .0
            .lock()
            .expect("Claude control replies lock")
            .remove(request_id);
        match waiting {
            Some(sender) => {
                let _ = sender.send(message.clone());
                true
            }
            None => false,
        }
    }
}

/// A turn's pending handoff: the prompt's `afterPlan` and Claude's name for
/// its permission mode.
#[derive(Clone)]
pub(super) struct PlanHandoff {
    after_plan: AfterPlan,
    native_mode: String,
    replies: ControlReplies,
    /// Set once the first approved `ExitPlanMode` hands off; a turn does so
    /// at most once.
    claimed: Arc<AtomicBool>,
}

impl PlanHandoff {
    /// The handoff of a turn in `work_mode` whose prompt carries
    /// `after_plan`; `None` when it does not hand off.
    pub(super) fn new(
        after_plan: Option<&AfterPlan>,
        work_mode: &str,
        replies: ControlReplies,
    ) -> Option<Self> {
        let after_plan = after_plan.filter(|_| work_mode == "plan")?;
        let native_mode = native_permission_mode(&after_plan.permission_mode)?;
        Some(Self {
            after_plan: after_plan.clone(),
            native_mode,
            replies,
            claimed: Arc::new(AtomicBool::new(false)),
        })
    }

    /// Whether this approval is the one that hands off: the first approved
    /// `ExitPlanMode` of the turn.
    pub(super) fn claim(&self) -> bool {
        !self.claimed.swap(true, Ordering::SeqCst)
    }

    /// Asks Claude to switch to the execute settings and waits for each
    /// answer. Every switch is attempted; the outcome says which took.
    pub(super) async fn apply(
        &self,
        outgoing: &mpsc::UnboundedSender<TurnMessage>,
    ) -> HandoffOutcome {
        let mut failures = Vec::new();
        let model = self
            .switch(
                outgoing,
                "model",
                json!({ "subtype": "set_model", "model": self.after_plan.model }),
            )
            .await;
        let model_applied = match model {
            Ok(_) => true,
            Err(error) => {
                failures.push(format!("model: {error}"));
                false
            }
        };
        let mode = self
            .switch(
                outgoing,
                "mode",
                json!({ "subtype": "set_permission_mode", "mode": self.native_mode }),
            )
            .await;
        let mode_applied = match mode {
            // Claude names the mode it now runs in; older releases answer
            // without a body.
            Ok(body) => Some(
                body.get("mode")
                    .and_then(Value::as_str)
                    .unwrap_or(&self.native_mode)
                    .to_owned(),
            ),
            Err(error) => {
                failures.push(format!("permission mode: {error}"));
                None
            }
        };
        let effort_applied = match self.after_plan.reasoning_effort.as_deref() {
            Some(effort) => match self
                .switch(
                    outgoing,
                    "effort",
                    json!({
                        "subtype": "apply_flag_settings",
                        "settings": { "effortLevel": effort },
                    }),
                )
                .await
            {
                Ok(_) => true,
                Err(error) => {
                    tracing::warn!(%error, "Claude Code did not take the execute reasoning effort");
                    false
                }
            },
            None => false,
        };
        HandoffOutcome {
            after_plan: self.after_plan.clone(),
            native_mode: self.native_mode.clone(),
            model_applied,
            mode_applied,
            effort_applied,
            failure: (!failures.is_empty()).then(|| failures.join("; ")),
        }
    }

    /// One control request and its answer's body.
    async fn switch(
        &self,
        outgoing: &mpsc::UnboundedSender<TurnMessage>,
        kind: &str,
        request: Value,
    ) -> Result<Value, String> {
        let request_id = format!(
            "todex-handoff-{kind}-{}",
            NEXT_REQUEST.fetch_add(1, Ordering::Relaxed)
        );
        let reply = self.replies.expect(&request_id);
        let frame = json!({
            "type": "control_request",
            "request_id": request_id,
            "request": request,
        });
        if outgoing.send(TurnMessage::Send(frame)).is_err() {
            self.replies.forget(&request_id);
            return Err("the turn ended".to_owned());
        }
        let answer = tokio::select! {
            answer = tokio::time::timeout(REPLY_TIMEOUT, reply) => answer,
            () = outgoing.closed() => {
                self.replies.forget(&request_id);
                return Err("the turn ended".to_owned());
            }
        };
        let message = match answer {
            Ok(Ok(message)) => message,
            Ok(Err(_)) => return Err("the turn ended".to_owned()),
            Err(_) => {
                self.replies.forget(&request_id);
                return Err("no answer from Claude Code".to_owned());
            }
        };
        if message.pointer("/response/subtype").and_then(Value::as_str) == Some("success") {
            return Ok(message
                .pointer("/response/response")
                .cloned()
                .unwrap_or(Value::Null));
        }
        Err(message
            .pointer("/response/error")
            .and_then(Value::as_str)
            .unwrap_or("refused")
            .to_owned())
    }
}

/// Claude's name for a TodeX permission mode in an implement turn.
fn native_permission_mode(mode: &str) -> Option<String> {
    crate::provider::types::resolve_execution_config(
        ProviderKind::ClaudeCode,
        Some(mode),
        Some("implement"),
        None,
        None,
        None,
    )
    .ok()?
    .provider_mode
}

/// What the switch to the execute settings achieved.
#[derive(Clone, Debug)]
pub(in crate::provider) struct HandoffOutcome {
    pub after_plan: AfterPlan,
    /// Claude's name for the requested permission mode.
    pub native_mode: String,
    pub model_applied: bool,
    /// The mode Claude reported running in, once it took the switch.
    pub mode_applied: Option<String>,
    pub effort_applied: bool,
    /// Why a switch did not take.
    pub failure: Option<String>,
}

impl HandoffOutcome {
    /// Whether the process now runs the execute model and permission mode,
    /// so the rest of the turn and later implement turns run on it.
    pub(in crate::provider) fn applied_now(&self) -> bool {
        self.model_applied && self.mode_applied.is_some()
    }

    /// The `provider.event` reporting the handoff to clients.
    pub(super) fn event(&self) -> Value {
        let now = self.applied_now();
        let reason = if now {
            "Plan approved; Claude Code continues this turn with the execute model and permission mode."
                .to_owned()
        } else {
            format!(
                "Plan approved, but Claude Code did not switch settings mid-turn ({}); they apply from the next prompt.",
                self.failure.as_deref().unwrap_or("unknown error")
            )
        };
        json!({
            "provider": "claude-code",
            "providerMethod": "plan_handoff",
            "metadata": {
                "workMode": "implement",
                "model": self.after_plan.model,
                "reasoningEffort": self.after_plan.reasoning_effort,
                "permissionMode": self.after_plan.permission_mode,
                "applied": if now { "now" } else { "next_turn" },
                "reason": reason,
            },
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn after_plan(mode: &str) -> AfterPlan {
        AfterPlan {
            model: "claude-opus-5-5".to_owned(),
            reasoning_effort: Some("medium".to_owned()),
            permission_mode: mode.to_owned(),
        }
    }

    #[test]
    fn only_a_plan_turn_with_after_plan_hands_off() {
        let replies = ControlReplies::default();
        assert!(PlanHandoff::new(None, "plan", replies.clone()).is_none());
        assert!(
            PlanHandoff::new(Some(&after_plan("auto")), "implement", replies.clone()).is_none()
        );
        for (mode, native) in [
            ("auto", "auto"),
            ("accept-edits", "acceptEdits"),
            ("full-access", "bypassPermissions"),
            ("ask", "default"),
        ] {
            let handoff =
                PlanHandoff::new(Some(&after_plan(mode)), "plan", replies.clone()).unwrap();
            assert_eq!(handoff.native_mode, native);
            assert!(handoff.claim());
            assert!(!handoff.claim());
        }
    }

    #[test]
    fn after_plan_reads_the_camel_case_wire_shape() {
        let parsed: AfterPlan = serde_json::from_value(json!({
            "model": "claude-opus-5-5",
            "reasoningEffort": "medium",
            "permissionMode": "auto",
        }))
        .unwrap();
        assert_eq!(parsed, after_plan("auto"));
        let parsed: AfterPlan = serde_json::from_value(json!({
            "model": "claude-opus-5-5",
            "reasoningEffort": null,
            "permissionMode": "auto",
        }))
        .unwrap();
        assert_eq!(parsed.reasoning_effort, None);
    }

    #[test]
    fn replies_reach_only_the_task_waiting_on_their_id() {
        let replies = ControlReplies::default();
        let mut waiting = replies.expect("todex-handoff-model-1");
        let other = json!({ "type": "control_response", "response": {
            "subtype": "success", "request_id": "todex-get-settings", "response": {},
        } });
        assert!(!replies.deliver(&other));
        let answer = json!({ "type": "control_response", "response": {
            "subtype": "success", "request_id": "todex-handoff-model-1", "response": {},
        } });
        assert!(replies.deliver(&answer));
        assert_eq!(waiting.try_recv().unwrap(), answer);
        assert!(!replies.deliver(&answer));
    }
}
