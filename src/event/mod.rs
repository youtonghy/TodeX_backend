pub mod audit;
pub mod bus;

pub use audit::AuditLog;
pub use bus::EventBus;

use std::ops::Deref;
use std::sync::{Arc, OnceLock};

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use uuid::Uuid;

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct EventRecord {
    pub time: DateTime<Utc>,
    pub event_id: String,
    #[serde(rename = "type")]
    pub event_type: String,
    pub workspace_id: Option<String>,
    pub window_id: Option<String>,
    pub pane_id: Option<String>,
    pub payload: Value,
}

impl EventRecord {
    pub fn new(
        event_type: impl Into<String>,
        workspace_id: Option<String>,
        window_id: Option<String>,
        pane_id: Option<String>,
        payload: Value,
    ) -> Self {
        Self {
            time: Utc::now(),
            event_id: format!("evt_{}", Uuid::new_v4().simple()),
            event_type: event_type.into(),
            workspace_id,
            window_id,
            pane_id,
            payload,
        }
    }
}

/// A record as every bus subscriber receives it: one shared copy, the scope
/// id connection filters match on (extracted once at publish), and a slot
/// for the single wire encoding all connections send.
#[derive(Debug)]
pub struct PublishedEvent {
    record: EventRecord,
    scope_id: Option<String>,
    wire: WireCache,
}

/// One wire encoding shared by every connection that sends a published
/// value: the first sender encodes, later ones reuse the text.
#[derive(Debug, Default)]
pub struct WireCache(OnceLock<Arc<str>>);

impl WireCache {
    pub fn get_or_encode<E>(
        &self,
        encode: impl FnOnce() -> Result<String, E>,
    ) -> Result<Arc<str>, E> {
        if let Some(text) = self.0.get() {
            return Ok(text.clone());
        }
        let text: Arc<str> = encode()?.into();
        Ok(self.0.get_or_init(|| text).clone())
    }
}

impl PublishedEvent {
    pub fn new(record: EventRecord) -> Self {
        Self {
            scope_id: scope_id(&record).map(str::to_owned),
            record,
            wire: WireCache::default(),
        }
    }

    /// The Codex session (`codex.*`) or terminal (`terminal.*`) the record
    /// belongs to; connections only see records of scopes they joined.
    pub fn scope_id(&self) -> Option<&str> {
        self.scope_id.as_deref()
    }

    /// The record's wire text, encoded by the first caller and shared after.
    pub fn wire_text(
        &self,
        encode: impl FnOnce(&EventRecord) -> Result<String, serde_json::Error>,
    ) -> Result<Arc<str>, serde_json::Error> {
        self.wire.get_or_encode(|| encode(&self.record))
    }
}

impl Deref for PublishedEvent {
    type Target = EventRecord;

    fn deref(&self) -> &EventRecord {
        &self.record
    }
}

fn scope_id(record: &EventRecord) -> Option<&str> {
    if record.event_type.starts_with("codex.") {
        return find_string_field(&record.payload, &["codexSessionId", "codex_session_id"], 0);
    }
    if record.event_type.starts_with("terminal.") {
        return find_string_field(&record.payload, &["terminalId", "terminal_id"], 0)
            .or(record.pane_id.as_deref());
    }
    None
}

fn find_string_field<'a>(value: &'a Value, keys: &[&str], depth: usize) -> Option<&'a str> {
    if depth > 8 {
        return None;
    }
    match value {
        Value::Object(object) => {
            for key in keys {
                if let Some(value) = object.get(*key).and_then(Value::as_str) {
                    return Some(value);
                }
            }
            object
                .values()
                .find_map(|value| find_string_field(value, keys, depth + 1))
        }
        Value::Array(values) => values
            .iter()
            .find_map(|value| find_string_field(value, keys, depth + 1)),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn scope_id_is_extracted_once_per_record() {
        let codex = PublishedEvent::new(EventRecord::new(
            "codex.turn.completed",
            None,
            None,
            None,
            json!({ "nested": { "codexSessionId": "cdxs_1" } }),
        ));
        assert_eq!(codex.scope_id(), Some("cdxs_1"));
        let terminal = PublishedEvent::new(EventRecord::new(
            "terminal.exit",
            None,
            None,
            Some("term_pane".to_owned()),
            json!({}),
        ));
        assert_eq!(terminal.scope_id(), Some("term_pane"));
        let other = PublishedEvent::new(EventRecord::new(
            "workspace.updated",
            None,
            None,
            None,
            json!({ "codexSessionId": "cdxs_1" }),
        ));
        assert_eq!(other.scope_id(), None);
    }

    #[test]
    fn wire_text_is_encoded_once() {
        let event = PublishedEvent::new(EventRecord::new(
            "terminal.output",
            None,
            None,
            None,
            json!({ "terminalId": "t" }),
        ));
        let mut encodings = 0;
        let first = event
            .wire_text(|record| {
                encodings += 1;
                serde_json::to_string(record)
            })
            .unwrap();
        let second = event
            .wire_text(|record| {
                encodings += 1;
                serde_json::to_string(record)
            })
            .unwrap();
        assert_eq!(encodings, 1);
        assert!(Arc::ptr_eq(&first, &second));
    }
}
