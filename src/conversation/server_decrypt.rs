//! Server-side reading of an API key's own conversations
//! (`docs/history-encryption.md` §3.5).
//!
//! The DEKs of a conversation owned by an API key are also wrapped for the
//! key's history recipient, whose private seed derives from the key's
//! secret. While serving a request authenticated with that key the daemon
//! unwraps those DEKs in memory and decrypts the events it returns, the way
//! a client would (§5.3). Nothing derived here is written anywhere; the seed
//! and the unwrapped keys are zeroized when the reader is dropped.

use std::collections::{BTreeSet, HashMap};
use std::io::Read as _;
use std::sync::Arc;

use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine as _};
use serde_json::{json, Map, Value};
use zeroize::Zeroizing;

use super::record::encrypted_content;
use super::{present_event, ConversationEvent, ConversationManifest};
use crate::error::AppError;
use crate::history_crypto::{
    self, open, unwrap_with_seed, ContentStream, SegmentKey, RECIPIENT_ID_LEN,
};
use crate::history_keys::{decode_id, HistoryKeys};

/// Upper bound on decompressed frame bytes, against a malformed frame.
const MAX_FRAME_BYTES: u64 = 64 * 1024 * 1024;

/// Decrypts one conversation's events for the holder of an API key seed.
pub(crate) struct HistoryReader {
    history: HistoryKeys,
    conversation_id: String,
    seed: Arc<Zeroizing<[u8; 32]>>,
    rid: [u8; RECIPIENT_ID_LEN],
    /// Unwrapped keys by kid; `None` for a kid with no wrap for this seed.
    keys: HashMap<String, Option<SegmentKey>>,
}

impl HistoryReader {
    pub(crate) fn new(
        history: HistoryKeys,
        conversation_id: impl Into<String>,
        seed: Arc<Zeroizing<[u8; 32]>>,
    ) -> Self {
        let rid = history_crypto::recipient_from_seed(&seed).rid();
        Self {
            history,
            conversation_id: conversation_id.into(),
            seed,
            rid,
            keys: HashMap::new(),
        }
    }

    /// `event` as a client renders it in full detail, its payload replaced
    /// by the plaintext. Content this seed cannot open (no wrap for it, a
    /// damaged record) becomes `{"encrypted": true}` plus the envelope.
    pub(crate) async fn present(
        &mut self,
        mut event: ConversationEvent,
        frames: &Map<String, Value>,
    ) -> ConversationEvent {
        present_event(&mut event, false);
        let Some(encrypted) = encrypted_content(&event.payload) else {
            return event;
        };
        let kids = referenced_kids(encrypted, frames);
        if let Err(error) = self.load(&kids).await {
            tracing::warn!(
                conversation_id = %self.conversation_id,
                error = %error,
                "API history keys could not be read"
            );
        }
        let plaintext = self.decrypt(&event, frames);
        let mut payload = match plaintext {
            Some(Value::Object(plaintext)) => plaintext,
            Some(other) => {
                let mut map = Map::new();
                map.insert("value".to_owned(), other);
                map
            }
            None => {
                let mut map = Map::new();
                map.insert("encrypted".to_owned(), json!(true));
                map
            }
        };
        // Envelope fields stay in plaintext beside `$enc`; keep any the
        // sealed content does not repeat.
        if let Value::Object(stored) = &event.payload {
            for (key, value) in stored {
                if key != super::record::ENCRYPTED_FIELD && !payload.contains_key(key) {
                    payload.insert(key.clone(), value.clone());
                }
            }
        }
        event.payload = Value::Object(payload);
        event
    }

    /// The decrypted title of `manifest` (`titleEnc`), or its plaintext
    /// title.
    pub(crate) async fn title(&mut self, manifest: &ConversationManifest) -> Option<String> {
        let Some(title) = &manifest.title_enc else {
            return manifest.title.clone().filter(|title| !title.is_empty());
        };
        self.load(&BTreeSet::from([title.kid.clone()])).await.ok()?;
        let key = self.keys.get(&title.kid)?.as_ref()?;
        let sealed = URL_SAFE_NO_PAD.decode(&title.ct).ok()?;
        let plaintext = open(
            key,
            &self.conversation_id,
            ContentStream::EventFull,
            0,
            &sealed,
        )
        .ok()?;
        String::from_utf8(plaintext).ok()
    }

    /// Unwraps every kid in `kids` not tried yet. One keyring read covers
    /// all of them.
    async fn load(&mut self, kids: &BTreeSet<String>) -> Result<(), AppError> {
        if kids.iter().all(|kid| self.keys.contains_key(kid)) {
            return Ok(());
        }
        let entries = self.history.keyrings().keys(&self.conversation_id).await?;
        for entry in entries {
            if self.keys.contains_key(&entry.kid) {
                continue;
            }
            let key = entry
                .wraps
                .iter()
                .find(|wrap| wrap.rid == self.rid)
                .and_then(|wrap| {
                    let kid = decode_id(&entry.kid, "kid").ok()?;
                    unwrap_with_seed(&self.seed, wrap, kid).ok()
                });
            self.keys.insert(entry.kid, key);
        }
        for kid in kids {
            self.keys.entry(kid.clone()).or_insert(None);
        }
        Ok(())
    }

    fn key(&self, kid: &str) -> Option<&SegmentKey> {
        self.keys.get(kid)?.as_ref()
    }

    fn decrypt(&self, event: &ConversationEvent, frames: &Map<String, Value>) -> Option<Value> {
        let encrypted = encrypted_content(&event.payload)?;
        if let Some(reference) = encrypted.get("fr") {
            let frame = ["f", "s"]
                .into_iter()
                .filter_map(|key| reference.get(key)?.as_str())
                .find_map(|id| frames.get(id))?;
            let index = usize::try_from(reference.get("i")?.as_u64()?).ok()?;
            return self.open_frame(frame)?.into_iter().nth(index);
        }
        let aad_conversation = encrypted.get("c")?.as_str()?;
        let key = self.key(encrypted.get("kid")?.as_str()?)?;
        let (field, stream) = if encrypted.contains_key("f") {
            ("f", ContentStream::EventFull)
        } else {
            ("s", ContentStream::EventSummary)
        };
        let sealed = URL_SAFE_NO_PAD
            .decode(encrypted.get(field)?.as_str()?)
            .ok()?;
        let plaintext = open(
            key,
            aad_conversation,
            stream,
            encrypted.get("n")?.as_u64()?,
            &sealed,
        )
        .ok()?;
        serde_json::from_slice(&plaintext).ok()
    }

    /// A `frames` entry opened and inflated into its payload array.
    fn open_frame(&self, frame: &Value) -> Option<Vec<Value>> {
        let key = self.key(frame.get("kid")?.as_str()?)?;
        let stream =
            ContentStream::try_from(u32::try_from(frame.get("stream")?.as_u64()?).ok()?).ok()?;
        let sealed = URL_SAFE_NO_PAD.decode(frame.get("ct")?.as_str()?).ok()?;
        let compressed = open(
            key,
            frame.get("c")?.as_str()?,
            stream,
            frame.get("counter")?.as_u64()?,
            &sealed,
        )
        .ok()?;
        let mut raw = Vec::new();
        flate2::read::DeflateDecoder::new(compressed.as_slice())
            .take(MAX_FRAME_BYTES)
            .read_to_end(&mut raw)
            .ok()?;
        serde_json::from_slice(&raw).ok()
    }
}

/// The kids an encrypted payload needs: its own, or those of the frames it
/// refers to.
fn referenced_kids(
    encrypted: &Map<String, Value>,
    frames: &Map<String, Value>,
) -> BTreeSet<String> {
    let mut kids = BTreeSet::new();
    if let Some(kid) = encrypted.get("kid").and_then(Value::as_str) {
        kids.insert(kid.to_owned());
    }
    if let Some(reference) = encrypted.get("fr") {
        for id in ["f", "s"]
            .into_iter()
            .filter_map(|key| reference.get(key)?.as_str())
        {
            if let Some(kid) = frames
                .get(id)
                .and_then(|frame| frame.get("kid"))
                .and_then(Value::as_str)
            {
                kids.insert(kid.to_owned());
            }
        }
    }
    kids
}
