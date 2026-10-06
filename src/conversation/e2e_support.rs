//! Client-side decryption of end-to-end encrypted history for tests
//! (`docs/history-encryption.md` §5.3), using the test recipient seeds.
//! It plays the part of the web and iOS clients, which implement the same
//! steps.

use std::collections::HashMap;
use std::io::Read as _;
use std::path::Path;

use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine as _};
use serde_json::{Map, Value};

use super::record::encrypted_content;
use super::{present_event, ConversationEvent};
use crate::history_crypto::{open, unwrap_for_tests, ContentStream, SegmentKey};
use crate::history_keys::test_support::{recipient, seed};
use crate::history_keys::{decode_id, HistoryKeys};

/// Every DEK of `conversation_id`'s keyring, unwrapped as device `byte`
/// (`test_support::seed(byte)`) would.
pub(crate) async fn client_keys(
    keys: &HistoryKeys,
    conversation_id: &str,
    byte: u8,
) -> HashMap<String, SegmentKey> {
    keys.keyrings()
        .keys(conversation_id)
        .await
        .unwrap()
        .into_iter()
        .filter_map(|entry| {
            let wrap = entry
                .wraps
                .iter()
                .find(|wrap| wrap.rid == recipient(byte).rid())?;
            let kid = decode_id(&entry.kid, "kid").unwrap();
            Some((
                entry.kid.clone(),
                unwrap_for_tests(seed(byte), wrap, kid).unwrap(),
            ))
        })
        .collect()
}

/// The plaintext payload of one replayed or live event, or `None` for a
/// plaintext event. Checks the anti-replay rule iOS enforces.
pub(crate) fn decrypt(
    keys: &HashMap<String, SegmentKey>,
    event: &ConversationEvent,
    frames: &Map<String, Value>,
) -> Option<Value> {
    let encrypted = encrypted_content(&event.payload)?;
    let aad_conversation = encrypted["c"].as_str().unwrap();
    if aad_conversation == event.conversation_id {
        assert_eq!(encrypted["n"].as_u64(), Some(event.sequence));
    }
    if let Some(reference) = encrypted.get("fr") {
        let id = ["f", "s"]
            .into_iter()
            .filter_map(|key| reference.get(key)?.as_str())
            .find(|id| frames.contains_key(*id))
            .expect("the page carries the referenced frame");
        let items = open_frame(keys, &frames[id]);
        return Some(items[reference["i"].as_u64().unwrap() as usize].clone());
    }
    let key = &keys[encrypted["kid"].as_str().unwrap()];
    let (field, stream) = match encrypted.get("f") {
        Some(_) => ("f", ContentStream::EventFull),
        None => ("s", ContentStream::EventSummary),
    };
    let sealed = URL_SAFE_NO_PAD
        .decode(encrypted[field].as_str().unwrap())
        .unwrap();
    let plaintext = open(
        key,
        aad_conversation,
        stream,
        encrypted["n"].as_u64().unwrap(),
        &sealed,
    )
    .unwrap();
    Some(serde_json::from_slice(&plaintext).unwrap())
}

/// A `frames` entry opened and inflated into its payload array.
pub(crate) fn open_frame(keys: &HashMap<String, SegmentKey>, frame: &Value) -> Vec<Value> {
    let key = &keys[frame["kid"].as_str().unwrap()];
    let stream = ContentStream::try_from(frame["stream"].as_u64().unwrap() as u32).unwrap();
    let sealed = URL_SAFE_NO_PAD
        .decode(frame["ct"].as_str().unwrap())
        .unwrap();
    let compressed = open(
        key,
        frame["c"].as_str().unwrap(),
        stream,
        frame["counter"].as_u64().unwrap(),
        &sealed,
    )
    .unwrap();
    let mut raw = Vec::new();
    flate2::read::DeflateDecoder::new(compressed.as_slice())
        .read_to_end(&mut raw)
        .unwrap();
    serde_json::from_slice(&raw).unwrap()
}

/// The plaintext of `events` as a client sees it in `detail`.
pub(crate) fn client_view(
    keys: &HashMap<String, SegmentKey>,
    events: &[ConversationEvent],
    frames: &Map<String, Value>,
    summary: bool,
) -> Vec<ConversationEvent> {
    events
        .iter()
        .map(|event| {
            let mut event = event.clone();
            present_event(&mut event, summary);
            if let Some(plaintext) = decrypt(keys, &event, frames) {
                event.payload = plaintext;
            }
            event
        })
        .collect()
}

/// A decrypted title (`manifest.titleEnc`: stream 2, counter 0).
pub(crate) fn decrypt_title(
    keys: &HashMap<String, SegmentKey>,
    conversation_id: &str,
    title: &Value,
) -> String {
    let sealed = URL_SAFE_NO_PAD
        .decode(title["ct"].as_str().unwrap())
        .unwrap();
    let plaintext = open(
        &keys[title["kid"].as_str().unwrap()],
        conversation_id,
        ContentStream::EventFull,
        0,
        &sealed,
    )
    .unwrap();
    String::from_utf8(plaintext).unwrap()
}

/// Every byte under `directory`, recursively.
pub(crate) fn all_bytes(directory: &Path) -> Vec<u8> {
    let mut bytes = Vec::new();
    for entry in std::fs::read_dir(directory).unwrap() {
        let path = entry.unwrap().path();
        if path.is_dir() {
            bytes.extend(all_bytes(&path));
        } else {
            bytes.extend(std::fs::read(&path).unwrap());
        }
    }
    bytes
}

pub(crate) fn contains(haystack: &[u8], needle: &str) -> bool {
    haystack
        .windows(needle.len())
        .any(|window| window == needle.as_bytes())
}
