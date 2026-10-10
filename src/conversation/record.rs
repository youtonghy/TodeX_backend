//! Journal record encoding (history v3, `docs/history-encryption.md` §4.2).
//!
//! Every new journal line is a v3 record with short keys:
//!
//! ```json
//! {"s":42,"i":"evt_…","t":1759670000123456,"y":"message.delta","r":"rawType?",
//!  "p":"claude-code?","e":{…envelope…},"c":{…payload…}}
//! ```
//!
//! `conversationId` and `schemaVersion` are implied by the journal's
//! directory and the store, and `normalizedType` is recomputed from `y`; a
//! record only spells it out (`n`) when the original event carried a value
//! the recomputation would not reproduce (`""` stands for an absent one).
//! `t` is microseconds since the epoch; `tn` keeps the sub-microsecond
//! nanoseconds of legacy events so a migrated event decodes identically.
//! Reads accept v2 full-event lines, [`CompactedRecord`] markers and v3
//! records alike, and always return the same [`ConversationEvent`].
//!
//! # End-to-end encrypted records
//!
//! With history encryption on, [`seal_event`] turns an event into its
//! *stored form*: the [`envelope_fields`] plus `"$enc": {v, kid, c, n, s?,
//! f}` (§5.3), the content sealed under the conversation's current DEK.
//! [`encode_record`] writes such a payload as `e` + `x` (`x` is exactly the
//! `$enc` object) and decoding returns the same stored form, so the record
//! on disk, the event the hub publishes and every replay carry identical
//! ciphertext. The daemon does not decrypt the records it serves to
//! devices; sealing a segment (`super::segment`) opens the ciphertext of
//! keys still in memory to repack it into frames, and the external API
//! decrypts an API key's own conversations with that key
//! (`super::server_decrypt`).

use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

use super::model::normalized_event_type;
use super::{summarize_event, ConversationEvent, ProviderKind, CONVERSATION_SCHEMA_VERSION};
use crate::error::AppError;
use crate::history_crypto::{self, ContentStream, SegmentKey};
use crate::history_keys::FingerprintKey;

/// Payload key of encrypted content on the wire (§5.3). A record stores the
/// same object as `x`.
pub(crate) const ENCRYPTED_FIELD: &str = "$enc";
/// `$enc.v`: history crypto v1.
const ENCRYPTED_VERSION: u64 = 1;

/// Every v3 record line starts with this; v2 full events start with
/// `{"schemaVersion"` and [`CompactedRecord`] lines with `{"sequence"`.
pub(super) const V3_RECORD_PREFIX: &[u8] = b"{\"s\":";
/// Every [`CompactedRecord`] line starts with this; a full event serializes
/// `schemaVersion` first, so the prefix alone tells the two apart.
const COMPACTED_RECORD_PREFIX: &[u8] = b"{\"sequence\":";
/// Marker written in place of each sequence whose streaming record was
/// stripped (by the retired compaction pass or by seal-time slimming, see
/// [`super::segment`]): it keeps the original sequence, event id and time
/// so sequence `N` still exists and no client ever waits on a gap. Its
/// payload is `{reason, originalType, runStart, runLength}`; consecutive
/// markers sharing `runStart` describe one stripped run. Appends cannot
/// forge it: the store rejects the `journal.` prefix, and clients classify
/// the type as unknown so the markers render nothing.
pub(super) const JOURNAL_COMPACTED_EVENT: &str = "journal.compacted";

/// Identity fields the e2e envelope keeps in plaintext (§5.2).
const ENVELOPE_ID_KEYS: [&str; 10] = [
    "turnId",
    "clientRequestId",
    "requestId",
    "permissionId",
    "runtimeId",
    "operationId",
    "itemId",
    "messageId",
    "toolCallId",
    "subagentId",
];

/// The §5.2 allowlist: the payload fields that stay readable without the
/// content key — routing ids, `role`/`status`/`scope` for the event kinds
/// that drive state, the identity part of `block` and `control`, numeric
/// `usage` and `stopReason`. Only fields present in the payload are copied.
/// Off mode writes the envelope too, so digests, deduplication and the
/// future e2e codec can rely on it being there.
///
/// `requestFingerprint` and `textMac` are copied as they are; with history
/// encryption on, [`add_history_macs`] has already turned them into
/// `HMAC-SHA256(fingerprintKey, …)` values.
pub(super) fn envelope_fields(event_type: &str, payload: &Value) -> Map<String, Value> {
    let mut envelope = Map::new();
    let Some(payload) = payload.as_object() else {
        return envelope;
    };
    let scalar = |value: &Value| matches!(value, Value::String(_) | Value::Number(_));
    for key in ENVELOPE_ID_KEYS {
        if let Some(value) = payload.get(key).filter(|value| scalar(value)) {
            envelope.insert(key.to_owned(), value.clone());
        }
    }
    let keyed = |prefix: &str| event_type.starts_with(prefix);
    let copy = |key: &str, envelope: &mut Map<String, Value>| {
        if let Some(value) = payload.get(key).filter(|value| scalar(value)) {
            envelope.insert(key.to_owned(), value.clone());
        }
    };
    if keyed("message.") {
        copy("role", &mut envelope);
    }
    if event_type == "provider.runtime" || keyed("turn.") || keyed("subagent.") {
        copy("status", &mut envelope);
    }
    // `tool.awaitingApproval` opens the same dialogs as `permission.*`; its
    // `scope` decides whether the conversation status changes.
    if keyed("permission.") || event_type == "tool.awaitingApproval" {
        copy("scope", &mut envelope);
    }
    // A control's outcome code (never its message or result) lets a
    // retried control report how the first attempt ended.
    if keyed("control.") {
        copy("code", &mut envelope);
    }
    copy("stopReason", &mut envelope);
    copy("requestFingerprint", &mut envelope);
    copy("textMac", &mut envelope);
    if let Some(block) = payload.get("block").and_then(Value::as_object) {
        let slim: Map<String, Value> = ["category", "id", "turnId"]
            .into_iter()
            .filter_map(|key| {
                let value = block.get(key).filter(|value| scalar(value))?;
                Some((key.to_owned(), value.clone()))
            })
            .collect();
        if !slim.is_empty() {
            envelope.insert("block".to_owned(), Value::Object(slim));
        }
    }
    if let Some(control) = payload.get("control").and_then(Value::as_object) {
        let slim: Map<String, Value> = ["action", "itemId", "textMac"]
            .into_iter()
            .filter_map(|key| {
                let value = control.get(key).filter(|value| scalar(value))?;
                Some((key.to_owned(), value.clone()))
            })
            .collect();
        if !slim.is_empty() {
            envelope.insert("control".to_owned(), Value::Object(slim));
        }
    }
    if let Some(usage) = payload.get("usage").and_then(numeric_only) {
        envelope.insert("usage".to_owned(), usage);
    }
    envelope
}

/// The `$enc` object of a stored-form payload, if it is encrypted.
pub(crate) fn encrypted_content(payload: &Value) -> Option<&Map<String, Value>> {
    payload.get(ENCRYPTED_FIELD)?.as_object()
}

/// e2e (§5.2): replace what deduplication compares with MACs under the
/// fingerprint key, so it keeps working without plaintext. Applied exactly
/// once to a plaintext payload, right before it is encrypted (an append, or
/// migrating an old plaintext record):
///
/// - `requestFingerprint` (the SHA-256 of a prompt request) becomes its MAC;
/// - a `control.requested` gets `requestFingerprint` = MAC of its `control`
///   object's JSON, so a retried control is matched without its arguments;
/// - a control's `text` gets `control.textMac`.
pub(crate) fn add_history_macs(event_type: &str, payload: &mut Value, key: &FingerprintKey) {
    let Some(map) = payload.as_object_mut() else {
        return;
    };
    if let Some(Value::String(fingerprint)) = map.get("requestFingerprint") {
        let mac = key.mac(fingerprint);
        map.insert("requestFingerprint".to_owned(), Value::String(mac));
    } else if event_type == "control.requested" {
        if let Some(control) = map.get("control").filter(|control| control.is_object()) {
            let mac = control_mac(key, control);
            map.insert("requestFingerprint".to_owned(), Value::String(mac));
        }
    }
    if let Some(Value::Object(control)) = map.get_mut("control") {
        if let Some(Value::String(text)) = control.get("text") {
            let mac = key.mac(text);
            control.insert("textMac".to_owned(), Value::String(mac));
        }
    }
}

/// The e2e `requestFingerprint` of a control request: the MAC of its JSON
/// (object keys sorted, as `serde_json` serializes a `Value`).
pub(crate) fn control_mac(key: &FingerprintKey, control: &Value) -> String {
    key.mac(&control.to_string())
}

/// The stored form of `event` sealed under `key` (§5.3): envelope fields
/// plus `$enc {v, kid, c, n, s?, f}`, stream 2 (full) and stream 1
/// (summary, only when [`summarize_event`] changes the payload), counter =
/// the event's sequence. The summary is computed here because only the
/// writer sees the plaintext.
pub(crate) fn seal_event(
    event: &ConversationEvent,
    kid: &str,
    key: &SegmentKey,
) -> Result<ConversationEvent, AppError> {
    let conversation_id = event.conversation_id.as_str();
    let sequence = event.sequence;
    let full = serde_json::to_vec(&event.payload)?;
    let mut summarized = event.clone();
    let summary = if summarize_event(&mut summarized) {
        Some(serde_json::to_vec(&summarized.payload)?)
    } else {
        None
    };
    let mut encrypted = Map::new();
    encrypted.insert("v".to_owned(), Value::from(ENCRYPTED_VERSION));
    encrypted.insert("kid".to_owned(), Value::String(kid.to_owned()));
    encrypted.insert("c".to_owned(), Value::String(conversation_id.to_owned()));
    encrypted.insert("n".to_owned(), Value::from(sequence));
    if let Some(summary) = summary {
        let sealed = history_crypto::seal(
            key,
            conversation_id,
            ContentStream::EventSummary,
            sequence,
            &summary,
        )?;
        encrypted.insert(
            "s".to_owned(),
            Value::String(URL_SAFE_NO_PAD.encode(sealed)),
        );
    }
    let sealed = history_crypto::seal(
        key,
        conversation_id,
        ContentStream::EventFull,
        sequence,
        &full,
    )?;
    encrypted.insert(
        "f".to_owned(),
        Value::String(URL_SAFE_NO_PAD.encode(sealed)),
    );
    let mut payload = envelope_fields(&event.event_type, &event.payload);
    payload.insert(ENCRYPTED_FIELD.to_owned(), Value::Object(encrypted));
    let mut stored = event.clone();
    stored.payload = Value::Object(payload);
    Ok(stored)
}

/// Opens an event-level `$enc` this conversation sealed under `key`:
/// `(full, summary)` payloads, the summary equal to the full payload when
/// `s` is absent. `None` when the object is not one [`seal_event`] wrote for
/// `(conversation_id, sequence)` — a fork's copy, a frame reference — or
/// does not authenticate; such content is kept as ciphertext.
pub(crate) fn open_event(
    encrypted: &Map<String, Value>,
    conversation_id: &str,
    sequence: u64,
    key: &SegmentKey,
) -> Option<(Value, Value)> {
    if encrypted.get("v").and_then(Value::as_u64) != Some(ENCRYPTED_VERSION)
        || encrypted.get("c").and_then(Value::as_str) != Some(conversation_id)
        || encrypted.get("n").and_then(Value::as_u64) != Some(sequence)
        || encrypted.contains_key("fr")
    {
        return None;
    }
    let open = |field: &str, stream: ContentStream| -> Option<Value> {
        let sealed = URL_SAFE_NO_PAD
            .decode(encrypted.get(field)?.as_str()?)
            .ok()?;
        let plaintext =
            history_crypto::open(key, conversation_id, stream, sequence, &sealed).ok()?;
        serde_json::from_slice(&plaintext).ok()
    };
    let full = open("f", ContentStream::EventFull)?;
    let summary = match encrypted.get("s") {
        Some(_) => open("s", ContentStream::EventSummary)?,
        None => full.clone(),
    };
    Some((full, summary))
}

/// The numeric skeleton of a usage object: numbers, and objects holding
/// numbers; `None` when nothing numeric remains.
fn numeric_only(value: &Value) -> Option<Value> {
    match value {
        Value::Number(_) => Some(value.clone()),
        Value::Object(map) => {
            let slim: Map<String, Value> = map
                .iter()
                .filter_map(|(key, value)| Some((key.clone(), numeric_only(value)?)))
                .collect();
            (!slim.is_empty()).then_some(Value::Object(slim))
        }
        _ => None,
    }
}

fn map_is_empty(map: &&Map<String, Value>) -> bool {
    map.is_empty()
}

/// Serialized form of a v3 record; with `c` absent it is the envelope line
/// of a sealed segment's envelope stream. Field order is the wire order, so
/// a full line is exactly `envelope line − "}" + ",\"c\":" + content + "}"`
/// ([`record_bytes`] relies on that).
#[derive(Serialize)]
struct RecordOut<'a> {
    s: u64,
    i: &'a str,
    t: i64,
    y: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    r: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    p: Option<ProviderKind>,
    #[serde(skip_serializing_if = "Option::is_none")]
    n: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    tn: Option<u32>,
    #[serde(skip_serializing_if = "map_is_empty")]
    e: &'a Map<String, Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    c: Option<&'a Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    x: Option<&'a Value>,
}

/// A parsed v3 record or envelope line.
#[derive(Deserialize)]
pub(super) struct RecordIn {
    s: u64,
    i: String,
    t: i64,
    y: String,
    #[serde(default)]
    r: Option<String>,
    #[serde(default)]
    p: Option<ProviderKind>,
    #[serde(default)]
    n: Option<String>,
    #[serde(default)]
    tn: Option<u32>,
    #[serde(default)]
    e: Map<String, Value>,
    #[serde(default)]
    c: Option<Value>,
    #[serde(default)]
    x: Option<Value>,
}

impl RecordIn {
    pub(super) fn sequence(&self) -> u64 {
        self.s
    }

    /// The plaintext envelope fields (`e`).
    pub(super) fn envelope(&self) -> &Map<String, Value> {
        &self.e
    }

    /// The event this record describes; `content` replaces `c` when the
    /// payload is stored elsewhere (a sealed segment's content stream).
    pub(super) fn into_event(
        self,
        conversation_id: &str,
        content: Option<Value>,
    ) -> Result<ConversationEvent, String> {
        let mut time = DateTime::<Utc>::from_timestamp_micros(self.t)
            .ok_or_else(|| format!("record {} has an invalid time", self.s))?;
        if let Some(nanos) = self.tn.filter(|nanos| *nanos > 0 && *nanos < 1000) {
            time += chrono::Duration::nanoseconds(i64::from(nanos));
        }
        let normalized_type = match self.n {
            None => Some(normalized_event_type(&self.y)),
            Some(value) if value.is_empty() => None,
            Some(value) => Some(value),
        };
        let payload = match (content.or(self.c), self.x) {
            (Some(payload), _) => payload,
            // Encrypted content: the stored form, envelope plus `$enc`.
            (None, Some(Value::Object(encrypted))) => {
                let mut payload = self.e;
                payload.insert(ENCRYPTED_FIELD.to_owned(), Value::Object(encrypted));
                Value::Object(payload)
            }
            (None, Some(_)) => return Err(format!("record {} has invalid ciphertext", self.s)),
            (None, None) => return Err(format!("record {} has no content", self.s)),
        };
        Ok(ConversationEvent {
            schema_version: CONVERSATION_SCHEMA_VERSION,
            sequence: self.s,
            event_id: self.i,
            conversation_id: conversation_id.to_owned(),
            time,
            event_type: self.y,
            normalized_type,
            raw_type: self.r,
            provider: self.p,
            payload,
        })
    }
}

fn record_out<'a>(
    event: &'a ConversationEvent,
    envelope: &'a Map<String, Value>,
    normalized: &'a str,
    content: Option<&'a Value>,
    encrypted: Option<&'a Value>,
) -> RecordOut<'a> {
    let nanos = event.time.timestamp_subsec_nanos() % 1000;
    RecordOut {
        s: event.sequence,
        i: &event.event_id,
        t: event.time.timestamp_micros(),
        y: &event.event_type,
        r: event.raw_type.as_deref(),
        p: event.provider,
        n: match event.normalized_type.as_deref() {
            Some(value) if value == normalized => None,
            Some(value) => Some(value),
            None => Some(""),
        },
        tn: (nanos > 0).then_some(nanos),
        e: envelope,
        c: content,
        x: encrypted,
    }
}

/// The v3 journal line of `event` (without its newline). A stored-form
/// payload (envelope plus `$enc`, see [`seal_event`]) is written as `e` and
/// `x`; any other payload as `c`.
pub(super) fn encode_record(event: &ConversationEvent) -> Result<Vec<u8>, serde_json::Error> {
    let envelope = envelope_fields(&event.event_type, &event.payload);
    let normalized = normalized_event_type(&event.event_type);
    let (content, encrypted) = match event.payload.get(ENCRYPTED_FIELD) {
        Some(encrypted) if encrypted.is_object() => (None, Some(encrypted)),
        _ => (Some(&event.payload), None),
    };
    serde_json::to_vec(&record_out(
        event,
        &envelope,
        &normalized,
        content,
        encrypted,
    ))
}

/// The envelope-stream line of `event` for a sealed segment: the record
/// without its content.
pub(super) fn encode_envelope(event: &ConversationEvent) -> Result<Vec<u8>, serde_json::Error> {
    let envelope = envelope_fields(&event.event_type, &event.payload);
    let normalized = normalized_event_type(&event.event_type);
    serde_json::to_vec(&record_out(event, &envelope, &normalized, None, None))
}

/// Journal bytes of one record for replay page budgets: the length of its
/// v3 line, whether it is stored as such or split into an envelope line and
/// a content item of a sealed segment.
pub(super) fn record_bytes(envelope_bytes: usize, content_bytes: usize) -> u64 {
    (envelope_bytes + content_bytes + ",\"c\":".len()) as u64
}

/// Parse an envelope-stream line or v3 record.
pub(super) fn parse_record(line: &[u8]) -> Result<RecordIn, serde_json::Error> {
    serde_json::from_slice(line)
}

/// Parse one journal line: a v3 record, a v2 full event, or a
/// [`CompactedRecord`] expanded into its marker event. Every journal read
/// goes through here.
pub(super) fn decode_journal_record(
    line: &[u8],
    conversation_id: &str,
) -> Result<ConversationEvent, serde_json::Error> {
    if line.starts_with(V3_RECORD_PREFIX) {
        let record: RecordIn = serde_json::from_slice(line)?;
        record
            .into_event(conversation_id, None)
            .map_err(serde::de::Error::custom)
    } else if line.starts_with(COMPACTED_RECORD_PREFIX) {
        serde_json::from_slice::<CompactedRecord>(line)
            .map(|record| record.into_event(conversation_id))
    } else {
        serde_json::from_slice(line)
    }
}

/// The `journal.compacted` marker standing in for `original`.
pub(super) fn compacted_marker(
    original: &ConversationEvent,
    run_start: u64,
    run_length: u64,
) -> ConversationEvent {
    CompactedRecord {
        sequence: original.sequence,
        compacted: CompactedRun {
            event_id: original.event_id.clone(),
            time_us: original.time,
            original_type: original.event_type.clone(),
            run_start,
            run_length,
        },
    }
    .into_event(&original.conversation_id)
}

/// On-disk form of a [`JOURNAL_COMPACTED_EVENT`] marker written by the
/// retired v2 compaction pass. Still decoded so those journals replay
/// unchanged; new markers are ordinary v3 records.
#[derive(Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(super) struct CompactedRecord {
    sequence: u64,
    compacted: CompactedRun,
}

#[derive(Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct CompactedRun {
    event_id: String,
    #[serde(with = "chrono::serde::ts_microseconds")]
    time_us: DateTime<Utc>,
    original_type: String,
    run_start: u64,
    run_length: u64,
}

impl CompactedRecord {
    fn into_event(self, conversation_id: &str) -> ConversationEvent {
        let run = self.compacted;
        let mut event = ConversationEvent::new(
            conversation_id,
            self.sequence,
            JOURNAL_COMPACTED_EVENT,
            serde_json::json!({
                "reason": "compacted",
                "originalType": run.original_type,
                "runStart": run.run_start,
                "runLength": run.run_length,
            }),
        );
        event.event_id = run.event_id;
        event.time = run.time_us;
        event
    }
}

#[cfg(test)]
mod tests {
    use chrono::SubsecRound;
    use serde_json::json;

    use super::*;

    fn same(left: &ConversationEvent, right: &ConversationEvent) {
        assert_eq!(
            serde_json::to_value(left).unwrap(),
            serde_json::to_value(right).unwrap()
        );
    }

    #[test]
    fn v2_events_round_trip_through_v3_records_exactly() {
        let conversation = "8f1c2a4e-1b2c-4d3e-8f00-112233445566";
        let mut cases = Vec::new();
        let mut plain = ConversationEvent::new(
            conversation,
            7,
            "message.delta",
            json!({"turnId": "t1", "role": "assistant", "delta": {"text": "hi"},
                   "block": {"category": "assistant_final", "id": "b1", "phase": "delta"}}),
        );
        plain.provider = Some(ProviderKind::ClaudeCode);
        cases.push(plain.clone());
        // Nanosecond time (Linux clocks), a raw type, a legacy normalized
        // value and a missing one must all survive.
        let mut legacy = plain.clone();
        legacy.time = DateTime::from_timestamp(1_759_670_000, 123_456_789).unwrap();
        legacy.raw_type = Some("item/started".to_owned());
        legacy.normalized_type = Some("assistant.legacy".to_owned());
        cases.push(legacy.clone());
        legacy.normalized_type = None;
        legacy.provider = None;
        cases.push(legacy);
        cases.push(ConversationEvent::new(
            conversation,
            8,
            "turn.started",
            json!([1, 2]),
        ));
        for original in cases {
            let v2_line = serde_json::to_vec(&original).unwrap();
            let v2 = decode_journal_record(&v2_line, conversation).unwrap();
            let v3_line = encode_record(&v2).unwrap();
            assert!(v3_line.starts_with(V3_RECORD_PREFIX));
            assert!(v3_line.len() < v2_line.len() + 64);
            let decoded = decode_journal_record(&v3_line, conversation).unwrap();
            same(&decoded, &original);
            // The split form (sealed segments) measures and decodes alike.
            let envelope = encode_envelope(&original).unwrap();
            let content = serde_json::to_vec(&original.payload).unwrap();
            assert_eq!(
                record_bytes(envelope.len(), content.len()),
                v3_line.len() as u64
            );
            let split = parse_record(&envelope)
                .unwrap()
                .into_event(conversation, Some(original.payload.clone()))
                .unwrap();
            same(&split, &original);
        }
    }

    #[test]
    fn compacted_record_lines_still_decode() {
        let mut original = ConversationEvent::new("c", 3, "message.delta", json!({"x": 1}));
        // Markers store microseconds; Linux clocks carry nanoseconds.
        original.time = original.time.trunc_subsecs(6);
        let line = serde_json::to_vec(&CompactedRecord {
            sequence: 3,
            compacted: CompactedRun {
                event_id: original.event_id.clone(),
                time_us: original.time,
                original_type: "message.delta".to_owned(),
                run_start: 2,
                run_length: 4,
            },
        })
        .unwrap();
        let marker = decode_journal_record(&line, "c").unwrap();
        same(&marker, &compacted_marker(&original, 2, 4));
        assert_eq!(marker.event_type, JOURNAL_COMPACTED_EVENT);
        assert_eq!(marker.payload["runLength"], 4);
    }

    #[test]
    fn envelope_keeps_only_the_allowlisted_fields() {
        let payload = json!({
            "turnId": "t", "role": "assistant", "status": "running", "scope": "turn",
            "text": "secret prose", "stopReason": "toolUse",
            "block": {"category": "tool", "id": "b", "turnId": "t", "phase": "delta", "title": "x"},
            "control": {"action": "steer", "itemId": "i", "text": "secret"},
            "usage": {"inputTokens": 3, "model": "m", "detail": {"cached": 1, "note": "n"}},
            "requestFingerprint": "fp",
        });
        let envelope = envelope_fields("message.delta", &payload);
        assert_eq!(
            Value::Object(envelope),
            json!({
                "turnId": "t", "role": "assistant", "stopReason": "toolUse",
                "requestFingerprint": "fp",
                "block": {"category": "tool", "id": "b", "turnId": "t"},
                "control": {"action": "steer", "itemId": "i"},
                "usage": {"inputTokens": 3, "detail": {"cached": 1}},
            })
        );
        // `status` and `scope` belong to the event kinds that use them.
        assert_eq!(
            envelope_fields("turn.completed", &payload)["status"],
            "running"
        );
        assert_eq!(
            envelope_fields("permission.requested", &payload)["scope"],
            "turn"
        );
        assert!(!envelope_fields("turn.completed", &payload).contains_key("role"));
    }

    #[test]
    fn encrypted_records_decode_to_their_stored_form_and_back() {
        let line = br#"{"s":1,"i":"evt_1","t":1,"y":"message.delta","e":{"turnId":"t"},"x":{"v":1,"kid":"k","c":"c","n":1,"f":"AA"}}"#;
        let event = decode_journal_record(line, "c").unwrap();
        assert_eq!(
            event.payload,
            json!({"turnId": "t", "$enc": {"v": 1, "kid": "k", "c": "c", "n": 1, "f": "AA"}})
        );
        // The stored form is written back as `e` + `x`, byte for byte.
        assert_eq!(encode_record(&event).unwrap(), line.to_vec());
        let invalid = br#"{"s":1,"i":"evt_1","t":1,"y":"message.delta","x":"nope"}"#;
        assert!(decode_journal_record(invalid, "c").is_err());
    }
}
