//! `history.*` WebSocket v2 commands (docs/history-encryption.md §7).
//!
//! Every registered device of the owner may call them; the caller's device
//! id comes from the authenticated connection, never from the payload. The
//! backend only moves public keys and wrapped DEKs: it checks that a key id
//! exists and that each wrap targets the granted recipient, and can never
//! read what it stores.
//!
//! Every persisted change to recipients, grants, the mode or the device block
//! list is announced to every `/v2/ws` connection as a global
//! `history.encryption.updated` event (docs/history-encryption.md §7.1),
//! published only after the write succeeded and never carrying key
//! material. Blocked devices (`revokedDevices`) get `HISTORY_ACCESS_REVOKED`
//! for every command but `history.encryption.get`.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::app_state::AppState;
use crate::config::HistoryEncryption;
use crate::error::AppError;
use crate::event::EventRecord;
use crate::history_crypto::{RecipientPublicKey, WrappedKey, KID_LEN, RECIPIENT_ID_LEN};
use crate::history_keys::{
    decode_id, GrantRecord, RecipientKind, RecipientsSnapshot, Written, MAX_BATCH,
};

/// The global server event announcing a history key state change.
pub(super) const UPDATED_EVENT: &str = "history.encryption.updated";

/// Every `history.*` command; `is_v2_native_command` lists the same names.
pub(super) const HISTORY_COMMANDS: [&str; 13] = [
    "history.encryption.get",
    "history.encryption.enable",
    "history.encryption.disable",
    "history.recipient.register",
    "history.recipient.revoke",
    "history.recovery.set",
    "history.grant.request",
    "history.grant.list",
    "history.grant.dismiss",
    "history.grant.fulfill",
    "history.keys.list",
    "history.keys.wraps",
    "history.device.restore",
];

pub(super) fn is_history_command(command_type: &str) -> bool {
    HISTORY_COMMANDS.contains(&command_type)
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Empty {}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct PublicKeyRequest {
    public_key: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct RidRequest {
    rid: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct DeviceIdRequest {
    device_id: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct GrantIdRequest {
    grant_id: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct KeysListRequest {
    #[serde(default)]
    conversation_id: Option<String>,
    #[serde(default)]
    cursor: Option<String>,
    #[serde(default)]
    limit: Option<usize>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct KeysWrapsRequest {
    conversation_id: String,
    kids: Vec<String>,
    #[serde(default)]
    rid: Option<String>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct FulfillRequest {
    #[serde(default)]
    grant_id: Option<String>,
    rid: String,
    #[serde(default)]
    wraps: Vec<FulfillWrap>,
    /// Marks the grant fulfilled after this batch (the last one).
    #[serde(default)]
    complete: bool,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct FulfillWrap {
    conversation_id: String,
    kid: String,
    wrapped: WrappedKey,
}

pub(super) async fn dispatch(
    state: &AppState,
    owner_id: &str,
    device_id: &str,
    command_type: &str,
    payload: &Value,
) -> Result<Value, AppError> {
    let keys = &state.history_keys;
    let registry = keys.recipients();
    registry.ensure_device(device_id)?;
    publish_external_changes(state).await?;
    if command_type != "history.encryption.get" {
        registry.ensure_access(device_id)?;
    }
    match command_type {
        "history.encryption.get" => {
            parse::<Empty>(payload)?;
            encryption_state(state, device_id)
        }
        "history.encryption.enable" | "history.encryption.disable" => {
            parse::<Empty>(payload)?;
            let mode = if command_type == "history.encryption.enable" {
                HistoryEncryption::E2e
            } else {
                HistoryEncryption::Off
            };
            let applied = registry.set_mode(mode)?;
            publish(state, applied.written, Updated::new("mode")).await;
            if mode == HistoryEncryption::E2e {
                state.conversations.request_history_migration();
            }
            encryption_state(state, device_id)
        }
        "history.recipient.register" => {
            let request = parse::<PublicKeyRequest>(payload)?;
            let key = RecipientPublicKey::from_base64url(&request.public_key)?;
            let applied = registry.register_device(device_id, &key)?;
            let rid = applied.value;
            let update = Updated::new("recipient.registered")
                .rid(&rid)
                .device(device_id);
            publish(state, applied.written, update).await;
            Ok(json!({ "rid": rid }))
        }
        "history.recipient.revoke" => {
            let request = parse::<RidRequest>(payload)?;
            decode_id::<RECIPIENT_ID_LEN>(&request.rid, "rid")?;
            let applied = registry.revoke(&request.rid, device_id)?;
            let mut update = Updated::new("recipient.revoked").rid(&request.rid);
            if let Some(revoked_device) = &applied.value {
                update = update.device(revoked_device);
            }
            publish(state, applied.written, update).await;
            encryption_state(state, device_id)
        }
        "history.device.restore" => {
            let request = parse::<DeviceIdRequest>(payload)?;
            let applied = registry.restore_device(&request.device_id)?;
            let update = Updated::new("device.restored").device(&request.device_id);
            publish(state, applied.written, update).await;
            encryption_state(state, device_id)
        }
        "history.recovery.set" => {
            let request = parse::<PublicKeyRequest>(payload)?;
            let key = RecipientPublicKey::from_base64url(&request.public_key)?;
            let applied = registry.set_recovery(&key)?;
            let rid = applied.value;
            publish(
                state,
                applied.written,
                Updated::new("recovery.set").rid(&rid),
            )
            .await;
            Ok(json!({ "rid": rid }))
        }
        "history.grant.request" => {
            parse::<Empty>(payload)?;
            let applied = registry.request_grant(device_id)?;
            let grant = applied.value;
            let update = Updated::new("grant.requested")
                .grant(&grant.grant_id)
                .rid(&grant.rid)
                .device(device_id);
            publish(state, applied.written, update).await;
            Ok(json!({ "grantId": grant.grant_id }))
        }
        "history.grant.list" => {
            parse::<Empty>(payload)?;
            Ok(json!({ "grants": grants_json(&registry.snapshot()?) }))
        }
        "history.grant.dismiss" => {
            let request = parse::<GrantIdRequest>(payload)?;
            let applied = registry.dismiss_grant(&request.grant_id)?;
            let update = Updated::new("grant.dismissed").grant(&request.grant_id);
            publish(state, applied.written, update).await;
            Ok(json!({}))
        }
        "history.keys.list" => {
            let request = parse::<KeysListRequest>(payload)?;
            let conversation_ids = match request.conversation_id {
                Some(id) => vec![state.conversations.get_owned(owner_id, &id).await?.id],
                None => state
                    .conversations
                    .list_owned(owner_id)
                    .await?
                    .into_iter()
                    .map(|manifest| manifest.id)
                    .collect(),
            };
            let page = keys
                .keyrings()
                .page(
                    &conversation_ids,
                    request.cursor.as_deref(),
                    request.limit.unwrap_or(MAX_BATCH),
                )
                .await?;
            let items = page
                .items
                .into_iter()
                .map(|(conversation_id, kid)| json!({ "conversationId": conversation_id, "kid": kid }))
                .collect::<Vec<_>>();
            let mut response = json!({ "items": items });
            if let Some(cursor) = page.next_cursor {
                response["nextCursor"] = json!(cursor);
            }
            Ok(response)
        }
        "history.keys.wraps" => {
            let request = parse::<KeysWrapsRequest>(payload)?;
            check_batch(request.kids.len(), "kids")?;
            for kid in &request.kids {
                decode_id::<KID_LEN>(kid, "kid")?;
            }
            let rid = match request.rid {
                Some(rid) => rid,
                None => registry.device_rid(device_id)?.ok_or_else(not_registered)?,
            };
            let rid = decode_id::<RECIPIENT_ID_LEN>(&rid, "rid")?;
            state
                .conversations
                .get_owned(owner_id, &request.conversation_id)
                .await?;
            let wraps = keys
                .keyrings()
                .wraps_for(&request.conversation_id, &request.kids, &rid)
                .await?;
            Ok(json!({ "wraps": wraps }))
        }
        "history.grant.fulfill" => fulfill(state, owner_id, device_id, payload).await,
        other => Err(AppError::Unsupported(format!(
            "v2 websocket command {other}"
        ))),
    }
}

/// Uploads re-wrapped DEKs for one recipient. With `grantId` the target is
/// that pending grant's recipient; without it (recovery import) the target
/// must be the caller's own recipient. Everything is validated before the
/// first keyring is written; duplicates are skipped so batches can be retried.
async fn fulfill(
    state: &AppState,
    owner_id: &str,
    device_id: &str,
    payload: &Value,
) -> Result<Value, AppError> {
    let keys = &state.history_keys;
    let registry = keys.recipients();
    let request = parse::<FulfillRequest>(payload)?;
    check_batch(request.wraps.len(), "wraps")?;
    let target = decode_id::<RECIPIENT_ID_LEN>(&request.rid, "rid")?;
    match &request.grant_id {
        Some(grant_id) => {
            if registry.pending_grant_rid(grant_id)? != request.rid {
                return Err(AppError::Unauthorized(
                    "wraps must target the grant's recipient".to_owned(),
                ));
            }
        }
        None => {
            if request.complete {
                return Err(AppError::InvalidRequest(
                    "complete requires a grantId".to_owned(),
                ));
            }
            if registry.device_rid(device_id)?.as_deref() != Some(request.rid.as_str()) {
                return Err(AppError::Unauthorized(
                    "without a grantId, wraps may only target the caller's own recipient"
                        .to_owned(),
                ));
            }
        }
    }
    if !registry.is_active(&request.rid)? {
        return Err(AppError::Conflict(format!(
            "history recipient {} is revoked",
            request.rid
        )));
    }

    let mut by_conversation = BTreeMap::<String, Vec<(String, WrappedKey)>>::new();
    for wrap in request.wraps {
        decode_id::<KID_LEN>(&wrap.kid, "kid")?;
        if wrap.wrapped.rid != target {
            return Err(AppError::InvalidRequest(
                "every wrapped key must be for the target recipient".to_owned(),
            ));
        }
        by_conversation
            .entry(wrap.conversation_id)
            .or_default()
            .push((wrap.kid, wrap.wrapped));
    }
    for (conversation_id, wraps) in &by_conversation {
        state
            .conversations
            .get_owned(owner_id, conversation_id)
            .await?;
        let existing = keys.keyrings().keys(conversation_id).await?;
        if let Some((kid, _)) = wraps
            .iter()
            .find(|(kid, _)| !existing.iter().any(|key| key.kid == *kid))
        {
            return Err(AppError::NotFound(format!("history key {kid}")));
        }
    }
    let mut added = 0;
    let mut updated = Vec::new();
    let mut failure = None;
    for (conversation_id, wraps) in by_conversation {
        match keys
            .keyrings()
            .append_wraps(&conversation_id, &target, wraps)
            .await
        {
            Ok(0) => {}
            Ok(count) => {
                added += count;
                updated.push(conversation_id);
            }
            Err(error) => {
                failure = Some(error);
                break;
            }
        }
    }
    // Wraps already written stay usable, so they are announced even when a
    // later conversation failed.
    if !updated.is_empty() {
        let mut update = Updated::new("grant.progress").rid(&request.rid);
        update.conversation_ids = Some(updated);
        if let Some(grant_id) = &request.grant_id {
            update = update.grant(grant_id);
        }
        match registry.state() {
            Ok(current) => publish(state, Some(current), update).await,
            Err(error) => tracing::warn!(
                error = %error,
                "history grant progress was not announced"
            ),
        }
    }
    if let Some(error) = failure {
        return Err(error);
    }
    if request.complete {
        if let Some(grant_id) = &request.grant_id {
            let applied = registry.complete_grant(grant_id)?;
            let update = Updated::new("grant.fulfilled")
                .grant(grant_id)
                .rid(&request.rid);
            publish(state, applied.written, update).await;
        }
    }
    Ok(json!({ "added": added }))
}

/// Announces device blocks the registry noticed outside `history.*`
/// commands: the TUI revoking devices through `devices.json` from its own
/// process, or the daemon blocking devices that vanished from it. Called
/// before every `history.*` command and by [`spawn_history_watch`].
pub(super) async fn publish_external_changes(state: &AppState) -> Result<(), AppError> {
    for change in state.history_keys.recipients().take_external_changes()? {
        let update = Updated::new("device.revoked").device(&change.device_id);
        publish(state, Some(change.state), update).await;
    }
    Ok(())
}

/// How often the daemon checks `recipients.json` for changes made by other
/// processes, so TUI device revocations are pushed without a client request.
const EXTERNAL_CHANGE_POLL: std::time::Duration = std::time::Duration::from_secs(2);

/// Polls for [`publish_external_changes`] until aborted.
pub(crate) fn spawn_history_watch(state: AppState) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(EXTERNAL_CHANGE_POLL);
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        let mut failing = false;
        loop {
            interval.tick().await;
            match publish_external_changes(&state).await {
                Ok(()) => failing = false,
                // Log once per failure streak, not every two seconds.
                Err(error) if !failing => {
                    failing = true;
                    tracing::warn!(
                        error = %error,
                        "history recipient registry check failed"
                    );
                }
                Err(_) => {}
            }
        }
    })
}

/// `history.encryption.updated` payload; absent fields are omitted.
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct Updated {
    epoch: u64,
    mode: HistoryEncryption,
    reason: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    rid: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    device_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    grant_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    conversation_ids: Option<Vec<String>>,
}

impl Updated {
    fn new(reason: &'static str) -> Self {
        Self {
            epoch: 0,
            mode: HistoryEncryption::Off,
            reason,
            rid: None,
            device_id: None,
            grant_id: None,
            conversation_ids: None,
        }
    }

    fn rid(mut self, rid: &str) -> Self {
        self.rid = Some(rid.to_owned());
        self
    }

    fn device(mut self, device_id: &str) -> Self {
        self.device_id = Some(device_id.to_owned());
        self
    }

    fn grant(mut self, grant_id: &str) -> Self {
        self.grant_id = Some(grant_id.to_owned());
        self
    }
}

/// Publishes `update` with the state `written` left behind; nothing when the
/// command changed nothing (`written` is `None`).
async fn publish(state: &AppState, written: Option<Written>, mut update: Updated) {
    let Some(written) = written else {
        return;
    };
    update.epoch = written.epoch;
    update.mode = written.mode;
    match serde_json::to_value(&update) {
        Ok(payload) => {
            state
                .events
                .publish(EventRecord::new(UPDATED_EVENT, None, None, None, payload))
                .await;
        }
        Err(error) => tracing::warn!(error = %error, "history update event was not published"),
    }
}

/// `{mode, epoch, recipients[], myRid?, myAccess, grants[], revokedDevices[]}`.
fn encryption_state(state: &AppState, device_id: &str) -> Result<Value, AppError> {
    let snapshot = state.history_keys.recipients().snapshot()?;
    let my_rid = snapshot
        .recipients
        .iter()
        .find(|record| {
            record.revoked_at.is_none()
                && record.kind == RecipientKind::Device
                && record.device_id.as_deref() == Some(device_id)
        })
        .map(|record| record.rid.clone());
    let blocked = snapshot
        .revoked_devices
        .iter()
        .any(|entry| entry.device_id == device_id);
    let my_access = match (blocked, &my_rid) {
        (true, _) => "revoked",
        (false, Some(_)) => "active",
        (false, None) => "unregistered",
    };
    let revoked_devices = snapshot
        .revoked_devices
        .iter()
        .map(|entry| json!({ "deviceId": entry.device_id, "revokedAt": entry.revoked_at }))
        .collect::<Vec<_>>();
    let mut response = json!({
        "mode": snapshot.mode,
        "epoch": snapshot.epoch,
        "recipients": snapshot.recipients,
        "grants": grants_json(&snapshot),
        "myAccess": my_access,
        "revokedDevices": revoked_devices,
    });
    if let Some(rid) = my_rid {
        response["myRid"] = json!(rid);
    }
    Ok(response)
}

/// Grants plus the target's public key, so a fulfilling device can wrap
/// without a separate lookup.
fn grants_json(snapshot: &RecipientsSnapshot) -> Vec<Value> {
    snapshot
        .grants
        .iter()
        .map(|grant: &GrantRecord| {
            let mut value = json!(grant);
            if let Some(record) = snapshot.recipients.iter().find(|r| r.rid == grant.rid) {
                value["publicKey"] = json!(record.public_key);
            }
            value
        })
        .collect()
}

/// A missing or `null` payload counts as `{}`.
fn parse<T: serde::de::DeserializeOwned>(payload: &Value) -> Result<T, AppError> {
    let payload = if payload.is_null() {
        &Value::Object(Default::default())
    } else {
        payload
    };
    T::deserialize(payload).map_err(|error| AppError::InvalidRequest(error.to_string()))
}

fn check_batch(len: usize, field: &str) -> Result<(), AppError> {
    if len > MAX_BATCH {
        return Err(AppError::InvalidRequest(format!(
            "{field} may hold at most {MAX_BATCH} items"
        )));
    }
    Ok(())
}

fn not_registered() -> AppError {
    AppError::InvalidRequest(
        "this device has no history recipient; call history.recipient.register first".to_owned(),
    )
}
