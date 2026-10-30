//! Private (RAM-only) conversations (`ephemeral: true`).
//!
//! A private conversation has no conversation folder: its manifest sits in
//! the store's manifest cache and everything else — the event list, the
//! journal digest, the provider state and the last request — in the table
//! below. Nothing is sealed or written: no journal, no keyring entry, no
//! DEK, no `titleEnc`. A daemon restart starts with an empty table, so the
//! conversation is gone.
//!
//! Every [`ConversationStore`] method that touches the conversation folder
//! branches here first (see [`ConversationStore::is_ephemeral`]). Writers
//! hold the conversation lock like they do for a journal, so sequences stay
//! contiguous and publication order matches append order.

use std::sync::Arc;

use dashmap::DashMap;
use serde_json::Value;

use super::digest::JournalDigest;
use super::store::{
    truncate_to_micros, validate_event, validate_id, CachedManifest, ConversationStore, PageBudget,
    MAX_REPLAY_LIMIT, MAX_REPLAY_PAGE_BYTES,
};
use super::{
    status_after_conversation_event, ConversationEvent, ConversationEventHub, ConversationManifest,
    ConversationReplay, ConversationStatus, ProviderState,
};
use crate::error::AppError;

/// What a private conversation holds besides its manifest.
pub(super) struct EphemeralConversation {
    events: Vec<ConversationEvent>,
    digest: Arc<JournalDigest>,
    provider_state: ProviderState,
    last_request: Option<Value>,
}

/// Private conversations by id.
pub(super) type EphemeralTable = Arc<DashMap<String, EphemeralConversation>>;

impl ConversationStore {
    /// Whether `conversation_id` is a private conversation held in memory.
    pub fn is_ephemeral(&self, conversation_id: &str) -> bool {
        self.ephemeral.contains_key(conversation_id)
    }

    /// [`Self::create_with_history`] for a private conversation: nothing
    /// reaches the data directory.
    pub(super) async fn create_ephemeral(
        &self,
        mut manifest: ConversationManifest,
        events: Vec<ConversationEvent>,
        provider_state: Option<ProviderState>,
        request: Option<Value>,
    ) -> Result<ConversationManifest, AppError> {
        validate_id(&manifest.id)?;
        let _guard = self.lock(&manifest.id).await;
        if self.ephemeral.contains_key(&manifest.id)
            || self.manifests.contains_key(&manifest.id)
            || tokio::fs::try_exists(self.directory(&manifest.id)?).await?
        {
            return Err(AppError::Conflict(format!(
                "conversation {} already exists",
                manifest.id
            )));
        }
        let mut digest = JournalDigest::default();
        for (index, event) in events.iter().enumerate() {
            validate_event(event, &manifest.id, index as u64 + 1)?;
            manifest.status = status_after_conversation_event(manifest.status, event);
            manifest.last_sequence = event.sequence;
            digest.apply(event);
        }
        // Plaintext, in memory only: there is no history to encrypt.
        manifest.storage_version = None;
        manifest.history_encrypted_at = None;
        manifest.title_enc = None;
        manifest.legacy_plaintext = false;
        manifest.ephemeral = true;
        let provider_state =
            provider_state.unwrap_or_else(|| ProviderState::new(manifest.provider));
        self.ephemeral.insert(
            manifest.id.clone(),
            EphemeralConversation {
                events,
                digest: Arc::new(digest),
                provider_state,
                last_request: request,
            },
        );
        self.cache_ephemeral_manifest(&manifest);
        Ok(manifest)
    }

    /// The authoritative copy of a private conversation's manifest: the
    /// manifest cache, never dirty (there is no file to flush it to).
    pub(super) fn cache_ephemeral_manifest(&self, manifest: &ConversationManifest) {
        self.manifests.insert(
            manifest.id.clone(),
            CachedManifest {
                manifest: manifest.clone(),
                dirty: false,
                flush_scheduled: false,
            },
        );
    }

    /// Manifests of every private conversation (for the list).
    pub(super) fn ephemeral_manifests(&self) -> Vec<ConversationManifest> {
        self.ephemeral
            .iter()
            .filter_map(|entry| {
                self.manifests
                    .get(entry.key())
                    .map(|cached| cached.manifest.clone())
            })
            .collect()
    }

    /// Drops a private conversation from memory. Callers hold the
    /// conversation lock and forget its cache entries afterwards.
    pub(super) fn remove_ephemeral_locked(&self, conversation_id: &str) -> bool {
        self.ephemeral.remove(conversation_id).is_some()
    }

    /// [`Self::append_batch_locked`] for a private conversation, after the
    /// payloads were redacted and bounded: the events get consecutive
    /// sequences, are folded into the digest, delivered to taps and
    /// published, all in memory. Callers hold the conversation lock.
    pub(super) async fn append_ephemeral_locked(
        &self,
        conversation_id: &str,
        prepared: Vec<(String, Value)>,
        hub: Option<&ConversationEventHub>,
    ) -> Result<Vec<ConversationEvent>, AppError> {
        let mut manifest = self.get_unlocked(conversation_id).await?;
        let events = prepared
            .into_iter()
            .zip(1_u64..)
            .map(|((event_type, payload), offset)| {
                let mut event = ConversationEvent::new(
                    conversation_id,
                    manifest.last_sequence.saturating_add(offset),
                    event_type,
                    payload,
                );
                event.provider = Some(manifest.provider);
                event.time = truncate_to_micros(event.time);
                event
            })
            .collect::<Vec<_>>();
        let tapped = self.taps.project(conversation_id, &events);
        {
            let mut entry = self
                .ephemeral
                .get_mut(conversation_id)
                .ok_or_else(|| AppError::NotFound(format!("conversation {conversation_id}")))?;
            let entry = &mut *entry;
            let digest = Arc::make_mut(&mut entry.digest);
            for event in &events {
                digest.apply(event);
            }
            entry.events.extend(events.iter().cloned());
        }
        if let Some(tapped) = tapped {
            self.taps.deliver(conversation_id, tapped);
        }
        let last_event = events.last().expect("a batch holds at least one event");
        manifest.last_sequence = last_event.sequence;
        for event in &events {
            manifest.status = status_after_conversation_event(manifest.status, event);
        }
        manifest.updated_at = last_event.time;
        self.cache_ephemeral_manifest(&manifest);
        if let Some(hub) = hub {
            for event in &events {
                hub.publish(event.clone());
            }
        }
        Ok(events)
    }

    fn ephemeral_entry<R>(
        &self,
        conversation_id: &str,
        read: impl FnOnce(&EphemeralConversation) -> R,
    ) -> Result<R, AppError> {
        self.ephemeral
            .get(conversation_id)
            .map(|entry| read(&entry))
            .ok_or_else(|| AppError::NotFound(format!("conversation {conversation_id}")))
    }

    pub(super) fn ephemeral_save_request(
        &self,
        conversation_id: &str,
        request: &Value,
    ) -> Result<(), AppError> {
        let mut entry = self
            .ephemeral
            .get_mut(conversation_id)
            .ok_or_else(|| AppError::NotFound(format!("conversation {conversation_id}")))?;
        entry.last_request = Some(request.clone());
        Ok(())
    }

    pub(super) fn ephemeral_last_request(
        &self,
        conversation_id: &str,
    ) -> Result<Option<Value>, AppError> {
        self.ephemeral_entry(conversation_id, |entry| entry.last_request.clone())
    }

    pub(super) fn ephemeral_provider_state(
        &self,
        conversation_id: &str,
    ) -> Result<ProviderState, AppError> {
        self.ephemeral_entry(conversation_id, |entry| entry.provider_state.clone())
    }

    pub(super) fn ephemeral_save_provider_state(
        &self,
        conversation_id: &str,
        state: ProviderState,
    ) -> Result<(), AppError> {
        let mut entry = self
            .ephemeral
            .get_mut(conversation_id)
            .ok_or_else(|| AppError::NotFound(format!("conversation {conversation_id}")))?;
        entry.provider_state = state;
        Ok(())
    }

    pub(super) fn ephemeral_digest(
        &self,
        conversation_id: &str,
    ) -> Result<Arc<JournalDigest>, AppError> {
        self.ephemeral_entry(conversation_id, |entry| Arc::clone(&entry.digest))
    }

    pub(super) fn ephemeral_last_event(
        &self,
        conversation_id: &str,
    ) -> Result<Option<ConversationEvent>, AppError> {
        self.ephemeral_entry(conversation_id, |entry| entry.events.last().cloned())
    }

    #[cfg(test)]
    pub(super) fn ephemeral_events(
        &self,
        conversation_id: &str,
    ) -> Result<Vec<ConversationEvent>, AppError> {
        self.ephemeral_entry(conversation_id, |entry| entry.events.clone())
    }

    /// Forward replay page, with the same record and byte limits as a
    /// journal page. Events are plaintext, which is what replay returns for
    /// plaintext records in every detail.
    pub(super) fn ephemeral_replay(
        &self,
        conversation_id: &str,
        after_sequence: u64,
        limit: usize,
    ) -> Result<ConversationReplay, AppError> {
        let limit = limit.clamp(1, MAX_REPLAY_LIMIT);
        self.ephemeral_entry(conversation_id, |entry| {
            let total = entry.events.len();
            let from = usize::try_from(after_sequence)
                .unwrap_or(usize::MAX)
                .min(total);
            let to = from.saturating_add(limit).min(total);
            let mut budget = PageBudget::new(MAX_REPLAY_PAGE_BYTES);
            let mut events = Vec::new();
            for event in &entry.events[from..to] {
                if !budget.admit(event_bytes(event)) {
                    break;
                }
                events.push(event.clone());
            }
            let read_to = from + events.len();
            ConversationReplay {
                conversation_id: conversation_id.to_owned(),
                from_sequence: after_sequence,
                next_sequence: events.last().map_or(after_sequence, |event| event.sequence),
                has_more: read_to < total,
                events,
                frames: serde_json::Map::new(),
            }
        })
    }

    /// Reverse replay page; see [`Self::replay_before_detail`].
    pub(super) fn ephemeral_replay_before(
        &self,
        conversation_id: &str,
        before_sequence: u64,
        limit: usize,
    ) -> Result<ConversationReplay, AppError> {
        let limit = limit.clamp(1, MAX_REPLAY_LIMIT);
        self.ephemeral_entry(conversation_id, |entry| {
            let total = entry.events.len();
            let to = usize::try_from(before_sequence)
                .unwrap_or(usize::MAX)
                .min(total);
            let from = to.saturating_sub(limit);
            // The budget drops the oldest records of the window first.
            let mut budget = PageBudget::new(MAX_REPLAY_PAGE_BYTES);
            let mut events = Vec::new();
            for event in entry.events[from..to].iter().rev() {
                if !budget.admit(event_bytes(event)) {
                    break;
                }
                events.push(event.clone());
            }
            events.reverse();
            let read_from = to - events.len();
            ConversationReplay {
                conversation_id: conversation_id.to_owned(),
                from_sequence: read_from as u64,
                next_sequence: events
                    .last()
                    .map_or(before_sequence, |event| event.sequence),
                has_more: read_from > 0,
                events,
                frames: serde_json::Map::new(),
            }
        })
    }

    /// [`Self::recover`] for a private conversation: an unfinished turn is
    /// `interrupted`. Only reachable within the daemon's lifetime (a restart
    /// forgets private conversations).
    pub(super) fn ephemeral_recover_locked(
        &self,
        conversation_id: &str,
        mut manifest: ConversationManifest,
    ) -> Result<ConversationManifest, AppError> {
        let digest = self.ephemeral_digest(conversation_id)?;
        let mut status = digest.status_from(ConversationStatus::Idle);
        if matches!(
            status,
            ConversationStatus::Running | ConversationStatus::WaitingPermission
        ) {
            status = ConversationStatus::Interrupted;
        }
        manifest.last_sequence = digest.last_sequence();
        manifest.updated_at = digest.last_time().unwrap_or(manifest.updated_at);
        manifest.status = status;
        self.cache_ephemeral_manifest(&manifest);
        Ok(manifest)
    }
}

/// What a record would weigh in a journal page.
fn event_bytes(event: &ConversationEvent) -> u64 {
    serde_json::to_vec(event).map_or(0, |bytes| bytes.len() as u64)
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;
    use crate::conversation::{ConversationEventHub, DeltaFragment, ProviderKind};

    #[tokio::test]
    async fn private_history_pages_coalesces_and_leaves_no_trace() {
        let root =
            std::env::temp_dir().join(format!("todex-ephemeral-store-{}", uuid::Uuid::new_v4()));
        let keys = crate::history_keys::HistoryKeys::load(&root.join("data"), None).unwrap();
        keys.recipients()
            .register_device("dev_a", &crate::history_keys::test_support::recipient(1))
            .unwrap();
        let store = ConversationStore::new(root.join("data"))
            .await
            .unwrap()
            .with_history_keys(keys);
        let listing = |path: &std::path::Path| {
            let mut names = std::fs::read_dir(path)
                .unwrap()
                .map(|entry| entry.unwrap().file_name())
                .collect::<Vec<_>>();
            names.sort();
            names
        };
        let before = listing(&root.join("data"));
        let conversations = listing(&root.join("data").join("conversations"));
        let mut manifest =
            ConversationManifest::new(ProviderKind::Pi, root.clone(), Some("t".into()), None);
        manifest.ephemeral = true;
        let manifest = store.create(manifest).await.unwrap();
        assert!(store.is_ephemeral(&manifest.id));
        let hub = ConversationEventHub::default();
        for index in 0..5 {
            store
                .append_and_publish(
                    &manifest.id,
                    "message.created",
                    json!({"role": "user", "content": format!("m{index}"), "turnId": "t1"}),
                    &hub,
                )
                .await
                .unwrap();
        }
        for part in ["a", "b"] {
            store
                .append_delta_and_publish(
                    &manifest.id,
                    "message.delta",
                    json!({"turnId": "t1", "delta": {"text": part}}),
                    DeltaFragment::keyed("k".to_owned(), &["/delta/text"]),
                    &hub,
                )
                .await
                .unwrap();
        }
        let page = store.replay(&manifest.id, 0, 3).await.unwrap();
        assert_eq!(
            page.events.iter().map(|e| e.sequence).collect::<Vec<_>>(),
            [1, 2, 3]
        );
        assert!(page.has_more);
        let back = store.replay_before(&manifest.id, 6, 2).await.unwrap();
        assert_eq!(
            back.events.iter().map(|e| e.sequence).collect::<Vec<_>>(),
            [5, 6]
        );
        assert_eq!(back.events[1].payload["delta"]["text"], "ab");
        assert!(back.has_more);
        assert_eq!(
            store
                .digest(&manifest.id, |d| d.last_sequence())
                .await
                .unwrap(),
            6
        );
        store
            .save_request(&manifest.id, &json!({"text": "secret"}))
            .await
            .unwrap();
        assert_eq!(
            store.last_request(&manifest.id).await.unwrap().unwrap()["text"],
            "secret"
        );
        assert_eq!(store.list().await.unwrap().len(), 1);
        assert_eq!(listing(&root.join("data")), before);
        assert_eq!(
            listing(&root.join("data").join("conversations")),
            conversations
        );
        assert!(store
            .create_from_journal(
                &manifest.id,
                ConversationManifest::new(ProviderKind::Pi, root.clone(), None, None),
                None,
                None,
                |e, _| e,
                |_| Vec::new()
            )
            .await
            .is_err());
        store.delete(&manifest.id).await.unwrap();
        assert!(store.list().await.unwrap().is_empty());
        assert!(store.get(&manifest.id).await.is_err());
        let _ = std::fs::remove_dir_all(root);
    }
}
