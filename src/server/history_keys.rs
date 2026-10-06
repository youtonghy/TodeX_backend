//! `history.*` WebSocket v2 commands (docs/history-encryption.md §7).
//!
//! Every registered device of the owner may call them; the caller's device
//! id comes from the authenticated connection, never from the payload. The
//! backend only moves public keys and wrapped DEKs: it checks that a key id
//! exists and that each wrap targets the granted recipient, and can never
//! read what it stores.

use std::collections::BTreeMap;

use serde::Deserialize;
use serde_json::{json, Value};

use crate::app_state::AppState;
use crate::config::HistoryEncryption;
use crate::error::AppError;
use crate::history_crypto::{RecipientPublicKey, WrappedKey, KID_LEN, RECIPIENT_ID_LEN};
use crate::history_keys::{decode_id, GrantRecord, RecipientsSnapshot, MAX_BATCH};

/// Every `history.*` command; `is_v2_native_command` lists the same names.
pub(super) const HISTORY_COMMANDS: [&str; 12] = [
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
            registry.set_mode(mode)?;
            encryption_state(state, device_id)
        }
        "history.recipient.register" => {
            let request = parse::<PublicKeyRequest>(payload)?;
            let key = RecipientPublicKey::from_base64url(&request.public_key)?;
            let rid = registry.register_device(device_id, &key)?;
            Ok(json!({ "rid": rid }))
        }
        "history.recipient.revoke" => {
            let request = parse::<RidRequest>(payload)?;
            decode_id::<RECIPIENT_ID_LEN>(&request.rid, "rid")?;
            registry.revoke(&request.rid)?;
            encryption_state(state, device_id)
        }
        "history.recovery.set" => {
            let request = parse::<PublicKeyRequest>(payload)?;
            let key = RecipientPublicKey::from_base64url(&request.public_key)?;
            Ok(json!({ "rid": registry.set_recovery(&key)? }))
        }
        "history.grant.request" => {
            parse::<Empty>(payload)?;
            Ok(json!({ "grantId": registry.request_grant(device_id)? }))
        }
        "history.grant.list" => {
            parse::<Empty>(payload)?;
            Ok(json!({ "grants": grants_json(&registry.snapshot()?) }))
        }
        "history.grant.dismiss" => {
            let request = parse::<GrantIdRequest>(payload)?;
            registry.dismiss_grant(&request.grant_id)?;
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
    for (conversation_id, wraps) in by_conversation {
        added += keys
            .keyrings()
            .append_wraps(&conversation_id, &target, wraps)
            .await?;
    }
    if request.complete {
        if let Some(grant_id) = &request.grant_id {
            registry.complete_grant(grant_id)?;
        }
    }
    Ok(json!({ "added": added }))
}

/// `{mode, epoch, recipients[], myRid?, grants[]}`.
fn encryption_state(state: &AppState, device_id: &str) -> Result<Value, AppError> {
    let registry = state.history_keys.recipients();
    let snapshot = registry.snapshot()?;
    let mut response = json!({
        "mode": snapshot.mode,
        "epoch": snapshot.epoch,
        "recipients": snapshot.recipients,
        "grants": grants_json(&snapshot),
    });
    if let Some(rid) = registry.device_rid(device_id)? {
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
