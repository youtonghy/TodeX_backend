use std::{
    collections::{BTreeMap, HashMap},
    sync::Arc,
};

use base64::{engine::general_purpose::STANDARD, Engine};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use tokio::sync::Mutex;
use uuid::Uuid;

use crate::{codex_gateway::CodexGatewayCursor, error::AppError, server::protocol::ServerEvent};

pub const TRANSPORT_VERSION: u16 = 1;
pub const TRANSPORT_CHUNK_TARGET_BYTES: usize = 100 * 1024;
pub const TRANSPORT_REASSEMBLY_LIMIT_BYTES: usize = 100 * 1024 * 1024;

#[derive(Clone, Debug, Deserialize)]
#[serde(tag = "type", content = "payload", rename_all = "camelCase")]
pub enum TransportClientMessage {
    #[serde(rename = "transport.hello")]
    Hello(TransportHelloPayload),
    #[serde(rename = "transport.ack")]
    Ack(TransportAckPayload),
    #[serde(rename = "transport.event")]
    Event(TransportEventClientPayload),
    #[serde(rename = "transport.chunk")]
    Chunk(TransportChunkPayload),
}

#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TransportHelloPayload {
    pub transport_version: u16,
    pub client_id: String,
    #[serde(default)]
    pub capabilities: Vec<String>,
    #[serde(default)]
    pub session_cursors: BTreeMap<String, CodexGatewayCursor>,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TransportAckPayload {
    #[serde(default)]
    pub session_id: Option<String>,
    #[serde(default)]
    pub cursor: Option<CodexGatewayCursor>,
    #[serde(default)]
    pub stream_id: Option<String>,
    #[serde(default)]
    pub seq_id: Option<u64>,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TransportEventClientPayload {
    pub payload: Value,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TransportChunkPayload {
    pub chunk_id: String,
    pub index: usize,
    pub total: usize,
    pub encoding: String,
    pub total_bytes: usize,
    pub data: String,
}

#[derive(Clone, Debug, Serialize)]
#[serde(tag = "type", content = "payload", rename_all = "camelCase")]
pub enum TransportServerMessage {
    #[serde(rename = "transport.event")]
    Event(TransportEventServerPayload),
    #[serde(rename = "transport.chunk")]
    Chunk(TransportChunkPayload),
    #[serde(rename = "transport.error")]
    Error(TransportErrorPayload),
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TransportEventServerPayload {
    pub stream_id: String,
    pub seq_id: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cursor: Option<CodexGatewayCursor>,
    pub payload: ServerEvent,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TransportErrorPayload {
    pub code: String,
    pub message: String,
}

#[derive(Clone, Debug)]
pub struct TransportHelloState {
    pub client_id: String,
    pub capabilities: Vec<String>,
    pub session_cursors: BTreeMap<String, CodexGatewayCursor>,
}

#[derive(Clone)]
pub struct TransportAckStore {
    inner: Arc<Mutex<HashMap<String, TransportAckState>>>,
}

#[derive(Clone, Debug, Default)]
struct TransportAckState {
    session_cursors: BTreeMap<String, CodexGatewayCursor>,
    stream_seq_ids: BTreeMap<String, u64>,
}

impl TransportAckStore {
    pub fn new() -> Self {
        Self {
            inner: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    pub async fn apply_hello(&self, hello: &TransportHelloState) {
        let mut inner = self.inner.lock().await;
        let state = inner.entry(hello.client_id.clone()).or_default();
        for (session_id, cursor) in &hello.session_cursors {
            let entry = state.session_cursors.entry(session_id.clone()).or_default();
            *entry = (*entry).max(*cursor);
        }
    }

    pub async fn apply_ack(&self, client_id: &str, ack: &TransportAckPayload) {
        let mut inner = self.inner.lock().await;
        let state = inner.entry(client_id.to_owned()).or_default();
        if let (Some(session_id), Some(cursor)) = (&ack.session_id, ack.cursor) {
            let entry = state.session_cursors.entry(session_id.clone()).or_default();
            *entry = (*entry).max(cursor);
        }
        if let (Some(stream_id), Some(seq_id)) = (&ack.stream_id, ack.seq_id) {
            let entry = state.stream_seq_ids.entry(stream_id.clone()).or_default();
            *entry = (*entry).max(seq_id);
        }
    }

    pub async fn cursor_for(
        &self,
        client_id: &str,
        session_id: &str,
    ) -> Option<CodexGatewayCursor> {
        let inner = self.inner.lock().await;
        inner
            .get(client_id)
            .and_then(|state| state.session_cursors.get(session_id).copied())
    }
}

#[derive(Default)]
pub struct TransportChunkReassembler {
    chunks: HashMap<String, PartialChunk>,
}

#[derive(Clone, Debug)]
struct PartialChunk {
    total: usize,
    total_bytes: usize,
    parts: BTreeMap<usize, String>,
}

impl TransportChunkReassembler {
    pub fn push(&mut self, chunk: TransportChunkPayload) -> Result<Option<String>, AppError> {
        if chunk.encoding != "base64" {
            return Err(AppError::InvalidRequest(format!(
                "unsupported transport chunk encoding {}",
                chunk.encoding
            )));
        }
        if chunk.total == 0 || chunk.index >= chunk.total {
            return Err(AppError::InvalidRequest(
                "invalid transport chunk index".to_owned(),
            ));
        }
        if chunk.total_bytes > TRANSPORT_REASSEMBLY_LIMIT_BYTES {
            return Err(AppError::InvalidRequest(
                "transport chunk payload exceeds reassembly limit".to_owned(),
            ));
        }

        let entry = self
            .chunks
            .entry(chunk.chunk_id.clone())
            .or_insert_with(|| PartialChunk {
                total: chunk.total,
                total_bytes: chunk.total_bytes,
                parts: BTreeMap::new(),
            });
        if entry.total != chunk.total || entry.total_bytes != chunk.total_bytes {
            return Err(AppError::InvalidRequest(
                "transport chunk metadata changed mid-message".to_owned(),
            ));
        }
        entry.parts.insert(chunk.index, chunk.data);
        if entry.parts.len() != entry.total {
            return Ok(None);
        }

        let mut bytes = Vec::with_capacity(entry.total_bytes);
        for index in 0..entry.total {
            let data = entry.parts.get(&index).ok_or_else(|| {
                AppError::InvalidRequest("transport chunk is missing a part".to_owned())
            })?;
            let part = STANDARD.decode(data).map_err(|err| {
                AppError::InvalidRequest(format!("invalid base64 transport chunk: {err}"))
            })?;
            bytes.extend_from_slice(&part);
        }
        if bytes.len() != entry.total_bytes {
            return Err(AppError::InvalidRequest(
                "transport chunk decoded size mismatch".to_owned(),
            ));
        }
        self.chunks.remove(&chunk.chunk_id);
        String::from_utf8(bytes).map(Some).map_err(|err| {
            AppError::InvalidRequest(format!("invalid utf-8 transport chunk: {err}"))
        })
    }
}

pub fn parse_transport_message(text: &str) -> Result<Option<TransportClientMessage>, AppError> {
    let value: Value = serde_json::from_str(text).map_err(|err| {
        AppError::InvalidRequest(format!("failed to parse client message: {err}"))
    })?;
    let Some(message_type) = value.get("type").and_then(Value::as_str) else {
        return Ok(None);
    };
    if !message_type.starts_with("transport.") {
        return Ok(None);
    }
    serde_json::from_value(value).map(Some).map_err(|err| {
        AppError::InvalidRequest(format!("failed to parse transport message: {err}"))
    })
}

pub fn transport_hello_state(
    hello: TransportHelloPayload,
) -> Result<TransportHelloState, AppError> {
    if hello.transport_version != TRANSPORT_VERSION {
        return Err(AppError::InvalidRequest(format!(
            "unsupported transport version {}",
            hello.transport_version
        )));
    }
    if hello.client_id.trim().is_empty() {
        return Err(AppError::InvalidRequest(
            "transport hello requires clientId".to_owned(),
        ));
    }
    Ok(TransportHelloState {
        client_id: hello.client_id,
        capabilities: hello.capabilities,
        session_cursors: hello.session_cursors,
    })
}

pub fn encode_server_event(event: ServerEvent, stream_id: &str, seq_id: u64) -> Vec<String> {
    let session_id = event.codex_session_id.clone();
    let cursor = event.cursor;
    let transport_event = TransportServerMessage::Event(TransportEventServerPayload {
        stream_id: stream_id.to_owned(),
        seq_id,
        session_id,
        cursor,
        payload: event,
    });
    let json = match serde_json::to_string(&transport_event) {
        Ok(json) => json,
        Err(_) => return vec![],
    };
    if json.len() <= TRANSPORT_CHUNK_TARGET_BYTES {
        return vec![json];
    }

    chunk_text(json)
}

pub fn encode_transport_error(code: impl Into<String>, message: impl Into<String>) -> String {
    serde_json::to_string(&TransportServerMessage::Error(TransportErrorPayload {
        code: code.into(),
        message: message.into(),
    }))
    .unwrap_or_else(|_| {
        json!({
            "type": "transport.error",
            "payload": {
                "code": "SERIALIZATION_ERROR",
                "message": "failed to serialize transport error"
            }
        })
        .to_string()
    })
}

fn chunk_text(json: String) -> Vec<String> {
    let bytes = json.into_bytes();
    let chunk_id = format!("chunk_{}", Uuid::new_v4().simple());
    let total = bytes.len().div_ceil(TRANSPORT_CHUNK_TARGET_BYTES);
    let total_bytes = bytes.len();
    bytes
        .chunks(TRANSPORT_CHUNK_TARGET_BYTES)
        .enumerate()
        .filter_map(|(index, part)| {
            serde_json::to_string(&TransportServerMessage::Chunk(TransportChunkPayload {
                chunk_id: chunk_id.clone(),
                index,
                total,
                encoding: "base64".to_owned(),
                total_bytes,
                data: STANDARD.encode(part),
            }))
            .ok()
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn server_event_with_text(text: String) -> ServerEvent {
        ServerEvent {
            event_id: "evt_1".to_owned(),
            event_type: "codex.item.completed".to_owned(),
            cursor: Some(7),
            codex_session_id: Some("session-1".to_owned()),
            codex_thread_id: None,
            codex_turn_id: None,
            workspace_id: None,
            window_id: None,
            pane_id: None,
            payload: json!({ "text": text }),
        }
    }

    #[test]
    fn transport_event_wraps_server_event_with_cursor() {
        let frames = encode_server_event(server_event_with_text("hello".to_owned()), "stream", 3);
        assert_eq!(frames.len(), 1);
        let frame: Value = serde_json::from_str(&frames[0]).unwrap();
        assert_eq!(frame["type"], "transport.event");
        assert_eq!(frame["payload"]["streamId"], "stream");
        assert_eq!(frame["payload"]["seqId"], 3);
        assert_eq!(frame["payload"]["sessionId"], "session-1");
        assert_eq!(frame["payload"]["cursor"], 7);
        assert_eq!(frame["payload"]["payload"]["type"], "codex.item.completed");
    }

    #[test]
    fn large_transport_events_are_chunked_and_reassembled() {
        let frames = encode_server_event(
            server_event_with_text("x".repeat(TRANSPORT_CHUNK_TARGET_BYTES + 1)),
            "stream",
            1,
        );
        assert!(frames.len() > 1);

        let mut reassembler = TransportChunkReassembler::default();
        let mut completed = None;
        for frame in frames {
            let message = parse_transport_message(&frame).unwrap().unwrap();
            let TransportClientMessage::Chunk(chunk) = message else {
                let value: Value = serde_json::from_str(&frame).unwrap();
                let payload = value.get("payload").cloned().unwrap();
                let chunk: TransportChunkPayload = serde_json::from_value(payload).unwrap();
                completed = reassembler.push(chunk).unwrap();
                continue;
            };
            completed = reassembler.push(chunk).unwrap();
        }

        let completed = completed.expect("chunks should complete");
        let value: Value = serde_json::from_str(&completed).unwrap();
        assert_eq!(value["type"], "transport.event");
        assert_eq!(value["payload"]["payload"]["cursor"], 7);
    }

    #[tokio::test]
    async fn ack_store_keeps_highest_session_cursor() {
        let store = TransportAckStore::new();
        let hello = TransportHelloState {
            client_id: "client-1".to_owned(),
            capabilities: vec![],
            session_cursors: BTreeMap::from([("session-1".to_owned(), 5)]),
        };
        store.apply_hello(&hello).await;
        store
            .apply_ack(
                "client-1",
                &TransportAckPayload {
                    session_id: Some("session-1".to_owned()),
                    cursor: Some(3),
                    stream_id: Some("stream".to_owned()),
                    seq_id: Some(9),
                },
            )
            .await;
        assert_eq!(store.cursor_for("client-1", "session-1").await, Some(5));
        store
            .apply_ack(
                "client-1",
                &TransportAckPayload {
                    session_id: Some("session-1".to_owned()),
                    cursor: Some(8),
                    stream_id: None,
                    seq_id: None,
                },
            )
            .await;
        assert_eq!(store.cursor_for("client-1", "session-1").await, Some(8));
    }
}
