//! Shared read positions (docs/API.md "已读位置"): the `conversation.read`
//! command and event. A conversation's `readSequence` is the newest event
//! any of the owner's devices has seen, so a reply read on one device stops
//! waiting on the others.

use serde::Deserialize;
use serde_json::{json, Value};

use crate::app_state::AppState;
use crate::error::AppError;
use crate::event::EventRecord;

/// `readSync` in `/v2/version`.
pub(super) const READ_SYNC_VERSION: u8 = 1;
pub(super) const READ_EVENT: &str = "conversation.read";

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ReadRequest {
    conversation_id: String,
    /// The newest event the device has seen; the conversation's newest when
    /// absent.
    #[serde(default)]
    sequence: Option<u64>,
}

/// `conversation.read` → `{ conversationId, readSequence }`.
pub(super) async fn command(
    state: &AppState,
    owner_id: &str,
    payload: &Value,
) -> Result<Value, AppError> {
    let request = ReadRequest::deserialize(payload)?;
    let read = mark(state, owner_id, &request.conversation_id, request.sequence).await?;
    Ok(json!({ "conversationId": request.conversation_id, "readSequence": read }))
}

/// Moves the read position forward and, when it moved, tells every
/// connected device with a `conversation.read` event.
async fn mark(
    state: &AppState,
    owner_id: &str,
    conversation_id: &str,
    sequence: Option<u64>,
) -> Result<u64, AppError> {
    let (read, moved) = state
        .conversations
        .mark_read_owned(owner_id, conversation_id, sequence)
        .await?;
    if moved {
        let payload = json!({ "conversationId": conversation_id, "readSequence": read });
        state
            .events
            .publish(EventRecord::new(READ_EVENT, None, None, None, payload))
            .await;
    }
    Ok(read)
}

/// The conversation's newest event before a message is sent: whoever sends
/// has seen what came before it. `None` when the conversation cannot be read
/// (the send itself then reports why).
pub(super) async fn sequence_before_send(
    state: &AppState,
    owner_id: &str,
    conversation_id: &str,
) -> Option<u64> {
    state
        .conversations
        .get_owned(owner_id, conversation_id)
        .await
        .ok()
        .map(|manifest| manifest.last_sequence)
}

/// After a message was accepted: marks the conversation read up to where it
/// stood before (`sequence_before_send`). Best effort: the message is sent
/// either way, so a failure is only logged.
pub(super) async fn mark_sent(
    state: &AppState,
    owner_id: &str,
    conversation_id: &str,
    sequence: Option<u64>,
) {
    let Some(sequence) = sequence else { return };
    if let Err(error) = mark(state, owner_id, conversation_id, Some(sequence)).await {
        tracing::warn!(conversation_id, error = %error, "read position after a send was not saved");
    }
}
