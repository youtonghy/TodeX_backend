use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use chrono::{DateTime, Utc};
use dashmap::DashMap;
use serde::de::DeserializeOwned;
use serde::Serialize;
use serde_json::Value;
use tokio::io::{AsyncReadExt, AsyncSeekExt, AsyncWriteExt};
use tokio::sync::{Mutex, OwnedMutexGuard};
use uuid::Uuid;

use crate::error::AppError;

use super::coalesce::{DeltaFragment, PendingDelta};
use super::{
    redact_secrets, status_after_conversation_event, ConversationEvent, ConversationEventHub,
    ConversationManifest, ConversationReplay, ConversationSnapshot, ProviderState,
    CONVERSATION_SCHEMA_VERSION, MAX_EVENT_PAYLOAD_BYTES,
};

const MANIFEST_FILE: &str = "manifest.json";
const EVENTS_FILE: &str = "events.jsonl";
const SNAPSHOT_FILE: &str = "snapshot.json";
const PROVIDER_STATE_FILE: &str = "provider-state.json";
const MAX_REPLAY_LIMIT: usize = 1000;
pub(crate) const MAX_EVENTS_JOURNAL_BYTES: u64 = 64 * 1024 * 1024;
/// New prompts are refused above this size so a started turn always has
/// headroom to append its events, including the terminal one, before the
/// hard cap.
const JOURNAL_PROMPT_LIMIT_BYTES: u64 = MAX_EVENTS_JOURNAL_BYTES - 1024 * 1024;
/// On overflow the journal is rewritten below this target so the next bursts
/// of events fit without compacting again on every append.
const JOURNAL_COMPACT_TARGET_BYTES: u64 = MAX_EVENTS_JOURNAL_BYTES * 3 / 4;
/// The newest events keep byte-identical payloads; only older history is
/// truncated so recent tool output stays intact for replay.
const JOURNAL_COMPACT_PROTECTED_BYTES: u64 = MAX_EVENTS_JOURNAL_BYTES / 4;
const JOURNAL_COMPACT_STRING_MAX: usize = 4 * 1024;
const JOURNAL_COMPACT_STRING_KEEP: usize = 1024;
/// Read buffer for the cold-index newline scan.
const JOURNAL_SCAN_BUFFER_BYTES: usize = 256 * 1024;
/// Placeholder written in place of each sequence a corrupt journal region
/// lost. Its payload is `{reason, runStart, runLength, backup}`; clients show
/// consecutive placeholders sharing `runStart` as one notice. Appends cannot
/// forge it: [`validate_event_type`] rejects its upper-case letter.
pub(crate) const JOURNAL_RECORD_LOST_EVENT: &str = "journal.recordLost";
/// Lower bound on the serialized size of any journal record (each carries a
/// 36-byte event id, a 36-byte conversation id and an RFC 3339 time). Salvage
/// uses it to bound how many sequences a corrupt region can have swallowed,
/// so a record with a damaged sequence number cannot conjure a huge run of
/// placeholders.
const MIN_JOURNAL_RECORD_BYTES: usize = 128;
/// Replay pages stop adding records once their journal bytes would exceed
/// this (records reach ~1 MiB, so 1000 of them could otherwise approach the
/// whole journal). A page always holds at least one record.
const MAX_REPLAY_PAGE_BYTES: u64 = 8 * 1024 * 1024;
/// Appends that leave the status unchanged only mark the cached manifest
/// dirty; it reaches `manifest.json` at most this often per conversation.
const MANIFEST_FLUSH_INTERVAL: std::time::Duration = std::time::Duration::from_secs(2);

#[derive(Clone)]
pub struct ConversationStore {
    root: PathBuf,
    locks: Arc<DashMap<String, Arc<Mutex<()>>>>,
    indexes: Arc<DashMap<String, JournalIndex>>,
    tails: Arc<DashMap<String, JournalTail>>,
    /// Open streaming-text merge window per conversation. Only touched while
    /// the conversation lock is held, so any other write flushes it first and
    /// journal order matches emission order.
    pending_deltas: Arc<DashMap<String, StoreDelta>>,
    delta_generation: Arc<AtomicU64>,
    /// Authoritative in-memory manifests. Every writer holds the
    /// conversation lock; loads on a cache miss take it too, so a stale disk
    /// read can never overwrite a newer entry or resurrect a deleted one.
    manifests: Arc<DashMap<String, CachedManifest>>,
    /// `(bytes, modified)` of journals that compaction could not shrink, so
    /// an unchanged full journal is not re-parsed on every overflowing append.
    incompressible: Arc<DashMap<String, (u64, Option<std::time::SystemTime>)>>,
}

struct CachedManifest {
    manifest: ConversationManifest,
    /// The entry is newer than `manifest.json`. Only appends that keep the
    /// status set this; see [`ConversationStore::append_locked`].
    dirty: bool,
    /// A debounced flush timer is pending for this conversation.
    flush_scheduled: bool,
}

struct StoreDelta {
    generation: u64,
    delta: PendingDelta<Option<ConversationEventHub>>,
}

#[derive(Clone)]
struct JournalIndex {
    bytes: u64,
    modified: Option<std::time::SystemTime>,
    offsets: Vec<(u64, u64)>,
    /// `false` when built by the cold newline scan, which parsed only the
    /// first and last records. Pages still validate every record they return;
    /// one that fails triggers the full validating scan (see
    /// [`ConversationStore::read_indexed_page`]).
    fully_validated: bool,
}

#[derive(Clone)]
struct JournalTail {
    bytes: u64,
    modified: Option<std::time::SystemTime>,
    event: ConversationEvent,
}

impl ConversationStore {
    pub async fn new(data_dir: PathBuf) -> Result<Self, AppError> {
        let root = data_dir.join("conversations");
        tokio::fs::create_dir_all(&root).await?;
        set_owner_only(&root, true).await?;
        Ok(Self {
            root,
            locks: Arc::new(DashMap::new()),
            indexes: Arc::new(DashMap::new()),
            tails: Arc::new(DashMap::new()),
            pending_deltas: Arc::new(DashMap::new()),
            delta_generation: Arc::new(AtomicU64::new(0)),
            manifests: Arc::new(DashMap::new()),
            incompressible: Arc::new(DashMap::new()),
        })
    }

    pub async fn create(
        &self,
        manifest: ConversationManifest,
    ) -> Result<ConversationManifest, AppError> {
        self.create_with_history(manifest, Vec::new(), None, None)
            .await
    }

    /// Publish a fork directory only after history, provider state and snapshot are complete.
    pub async fn create_with_history(
        &self,
        mut manifest: ConversationManifest,
        events: Vec<ConversationEvent>,
        provider_state: Option<ProviderState>,
        request: Option<Value>,
    ) -> Result<ConversationManifest, AppError> {
        validate_id(&manifest.id)?;
        let mut journal = Vec::new();
        for (index, event) in events.iter().enumerate() {
            validate_event(event, &manifest.id, index as u64 + 1)?;
            serde_json::to_writer(&mut journal, event)?;
            journal.push(b'\n');
            if journal.len() as u64 > MAX_EVENTS_JOURNAL_BYTES {
                return Err(AppError::ResourceExhausted(
                    "Fork history exceeds the journal storage limit".to_owned(),
                ));
            }
            manifest.status = status_after_conversation_event(manifest.status, event);
            manifest.last_sequence = event.sequence;
        }
        let _guard = self.lock(&manifest.id).await;
        let directory = self.directory(&manifest.id)?;
        if tokio::fs::try_exists(&directory).await? {
            return Err(AppError::Conflict(format!(
                "conversation {} already exists",
                manifest.id
            )));
        }
        let temporary = self.root.join(format!(
            ".conversation.{}.{}.tmp",
            manifest.id,
            Uuid::new_v4().simple()
        ));
        tokio::fs::create_dir(&temporary).await?;
        let create_result = async {
            set_owner_only(&temporary, true).await?;
            let event_file = temporary.join(EVENTS_FILE);
            let mut file = tokio::fs::OpenOptions::new()
                .create_new(true)
                .write(true)
                .open(&event_file)
                .await?;
            file.write_all(&journal).await?;
            file.flush().await?;
            file.sync_all().await?;
            drop(file);
            set_owner_only(&event_file, false).await?;

            write_atomic_json(&temporary.join(MANIFEST_FILE), &manifest).await?;
            write_atomic_json(
                &temporary.join(SNAPSHOT_FILE),
                &ConversationSnapshot::from_manifest(&manifest),
            )
            .await?;
            write_atomic_json(
                &temporary.join(PROVIDER_STATE_FILE),
                &provider_state.unwrap_or_else(|| ProviderState::new(manifest.provider)),
            )
            .await?;
            if let Some(request) = request {
                write_atomic_json(&temporary.join("last-request.json"), &request).await?;
            }
            sync_directory(&temporary).await?;
            tokio::fs::rename(&temporary, &directory).await?;
            sync_directory(&self.root).await
        }
        .await;
        if let Err(error) = create_result {
            let _ = tokio::fs::remove_dir_all(&temporary).await;
            return Err(error);
        }
        self.manifests.insert(
            manifest.id.clone(),
            CachedManifest {
                manifest: manifest.clone(),
                dirty: false,
                flush_scheduled: false,
            },
        );
        Ok(manifest)
    }

    pub async fn get(&self, conversation_id: &str) -> Result<ConversationManifest, AppError> {
        validate_id(conversation_id)?;
        if let Some(cached) = self.manifests.get(conversation_id) {
            return Ok(cached.manifest.clone());
        }
        let _guard = self.lock(conversation_id).await;
        self.get_unlocked(conversation_id).await
    }

    /// The directory listing decides which conversations exist; only
    /// manifests missing from the cache are read from disk, so a list costs
    /// one `read_dir` instead of parsing every manifest.
    pub async fn list(&self) -> Result<Vec<ConversationManifest>, AppError> {
        let mut directory = tokio::fs::read_dir(&self.root).await?;
        let mut manifests = Vec::new();
        while let Some(entry) = directory.next_entry().await? {
            if !entry.file_type().await?.is_dir() {
                continue;
            }
            let id = entry.file_name().to_string_lossy().to_string();
            if validate_id(&id).is_err() {
                continue;
            }
            match self.get(&id).await {
                Ok(manifest) => manifests.push(manifest),
                Err(error) => {
                    tracing::warn!(conversation_id = %id, error = %error, "skipping unreadable conversation")
                }
            }
        }
        manifests.sort_by_key(|manifest| std::cmp::Reverse(manifest.updated_at));
        Ok(manifests)
    }

    pub async fn update_metadata(
        &self,
        conversation_id: &str,
        title: Option<Option<String>>,
        archived: Option<bool>,
    ) -> Result<ConversationManifest, AppError> {
        let _guard = self.lock(conversation_id).await;
        let mut manifest = self.get_unlocked(conversation_id).await?;
        if let Some(title) = title {
            manifest.title = title
                .map(|value| value.trim().chars().take(200).collect::<String>())
                .filter(|value| !value.is_empty());
        }
        if let Some(archived) = archived {
            manifest.archived_at = archived.then(Utc::now);
        }
        manifest.updated_at = Utc::now();
        self.persist_manifest_locked(&manifest).await?;
        Ok(manifest)
    }

    /// Force the stored status without a journal event. Only for the case
    /// where the journal itself refused the terminal event: the manifest stops
    /// claiming a turn that is no longer running, and restart recovery still
    /// closes the turn in the journal.
    pub async fn set_status(
        &self,
        conversation_id: &str,
        status: super::ConversationStatus,
    ) -> Result<ConversationManifest, AppError> {
        let _guard = self.lock(conversation_id).await;
        let mut manifest = self.get_unlocked(conversation_id).await?;
        if manifest.status == status {
            return Ok(manifest);
        }
        manifest.status = status;
        manifest.updated_at = Utc::now();
        self.persist_manifest_locked(&manifest).await?;
        Ok(manifest)
    }

    pub async fn delete(&self, conversation_id: &str) -> Result<(), AppError> {
        let _guard = self.lock(conversation_id).await;
        let directory = self.directory(conversation_id)?;
        if !tokio::fs::try_exists(&directory).await? {
            return Err(AppError::NotFound(format!(
                "conversation {conversation_id}"
            )));
        }
        tokio::fs::remove_dir_all(directory).await?;
        self.forget_locked(conversation_id);
        Ok(())
    }

    /// Drop every cache entry of a removed conversation. Callers hold the
    /// conversation lock.
    fn forget_locked(&self, conversation_id: &str) {
        self.indexes.remove(conversation_id);
        self.tails.remove(conversation_id);
        self.pending_deltas.remove(conversation_id);
        self.manifests.remove(conversation_id);
        self.incompressible.remove(conversation_id);
    }

    pub async fn cleanup_before(
        &self,
        cutoff: DateTime<Utc>,
        protected: &std::collections::HashSet<String>,
    ) -> Result<Vec<ConversationManifest>, AppError> {
        let manifests = self.list().await?;
        let mut removed = Vec::new();
        for manifest in manifests {
            if protected.contains(&manifest.id)
                || manifest.updated_at >= cutoff
                || matches!(
                    manifest.status,
                    super::ConversationStatus::Running
                        | super::ConversationStatus::WaitingPermission
                )
            {
                continue;
            }
            let id = manifest.id.clone();
            let _guard = self.lock(&id).await;
            let current = match self.get_unlocked(&id).await {
                Ok(value) => value,
                Err(AppError::NotFound(_)) => continue,
                Err(error) => return Err(error),
            };
            if current.updated_at < cutoff
                && !matches!(
                    current.status,
                    super::ConversationStatus::Running
                        | super::ConversationStatus::WaitingPermission
                )
            {
                tokio::fs::remove_dir_all(self.directory(&id)?).await?;
                self.forget_locked(&id);
                removed.push(current);
            }
        }
        Ok(removed)
    }

    pub async fn append(
        &self,
        conversation_id: &str,
        event_type: impl Into<String>,
        payload: Value,
    ) -> Result<ConversationEvent, AppError> {
        self.append_inner(conversation_id, event_type.into(), payload, None)
            .await
    }

    /// Serialize durable writes and publication with the same conversation lock.
    pub async fn append_and_publish(
        &self,
        conversation_id: &str,
        event_type: impl Into<String>,
        payload: Value,
        hub: &ConversationEventHub,
    ) -> Result<ConversationEvent, AppError> {
        self.append_inner(conversation_id, event_type.into(), payload, Some(hub))
            .await
    }

    /// Streaming text fragment: merged with the open window of the same stream
    /// (see [`super::coalesce`]) and journalled when the window expires, fills,
    /// or any other event of this conversation is written or read. The merged
    /// event is published like any other append, so live latency grows by at
    /// most the coalescing window.
    pub async fn append_delta_and_publish(
        &self,
        conversation_id: &str,
        event_type: impl Into<String>,
        payload: Value,
        fragment: DeltaFragment,
        hub: &ConversationEventHub,
    ) -> Result<(), AppError> {
        let event_type = event_type.into();
        validate_event_type(&event_type)?;
        // Reject an unknown conversation now instead of from the flush timer.
        self.directory(conversation_id)?;
        let _guard = self.lock(conversation_id).await;
        let payload = match self.pending_deltas.get_mut(conversation_id) {
            Some(mut pending) => match pending.delta.try_append(&event_type, &fragment, payload) {
                Ok(()) => return Ok(()),
                Err(payload) => payload,
            },
            None => payload,
        };
        self.flush_pending_delta_locked(conversation_id).await?;
        let generation = self.delta_generation.fetch_add(1, Ordering::Relaxed);
        let delta = PendingDelta::new(Some(hub.clone()), event_type, fragment, payload);
        let deadline = delta.deadline();
        self.pending_deltas
            .insert(conversation_id.to_owned(), StoreDelta { generation, delta });
        let store = self.clone();
        let conversation_id = conversation_id.to_owned();
        tokio::spawn(async move {
            tokio::time::sleep_until(deadline).await;
            store
                .flush_expired_delta(&conversation_id, generation)
                .await;
        });
        Ok(())
    }

    /// Journals every open merge window, then writes every dirty cached
    /// manifest; called on shutdown so nothing waiting on a flush timer is
    /// lost with the runtime.
    pub async fn flush_pending_deltas(&self) {
        let conversation_ids: Vec<String> = self
            .pending_deltas
            .iter()
            .map(|entry| entry.key().clone())
            .collect();
        for conversation_id in conversation_ids {
            let _guard = self.lock(&conversation_id).await;
            self.flush_pending_delta_logged(&conversation_id).await;
        }
        let dirty: Vec<String> = self
            .manifests
            .iter()
            .filter(|entry| entry.dirty)
            .map(|entry| entry.key().clone())
            .collect();
        for conversation_id in dirty {
            let _guard = self.lock(&conversation_id).await;
            self.flush_manifest_logged(&conversation_id).await;
        }
    }

    async fn flush_expired_delta(&self, conversation_id: &str, generation: u64) {
        let _guard = self.lock(conversation_id).await;
        let current = self
            .pending_deltas
            .get(conversation_id)
            .is_some_and(|pending| pending.generation == generation);
        if current {
            self.flush_pending_delta_logged(conversation_id).await;
        }
    }

    /// For flushes without a caller to report to (timer, shutdown, reads):
    /// the fragment is lost, so the failure is logged loudly.
    async fn flush_pending_delta_logged(&self, conversation_id: &str) {
        if let Err(error) = self.flush_pending_delta_locked(conversation_id).await {
            tracing::error!(conversation_id, error = %error, "failed to journal coalesced stream text");
        }
    }

    /// Callers hold the conversation lock.
    async fn flush_pending_delta_locked(&self, conversation_id: &str) -> Result<(), AppError> {
        let Some((_, pending)) = self.pending_deltas.remove(conversation_id) else {
            return Ok(());
        };
        let (hub, event_type, payload) = pending.delta.into_parts();
        self.append_locked(conversation_id, event_type, payload, hub.as_ref())
            .await
            .map(|_| ())
    }

    async fn append_inner(
        &self,
        conversation_id: &str,
        event_type: String,
        payload: Value,
        hub: Option<&ConversationEventHub>,
    ) -> Result<ConversationEvent, AppError> {
        validate_event_type(&event_type)?;
        let _guard = self.lock(conversation_id).await;
        // Buffered stream text precedes this event in emission order.
        self.flush_pending_delta_locked(conversation_id).await?;
        self.append_locked(conversation_id, event_type, payload, hub)
            .await
    }

    /// Callers hold the conversation lock.
    async fn append_locked(
        &self,
        conversation_id: &str,
        event_type: String,
        mut payload: Value,
        hub: Option<&ConversationEventHub>,
    ) -> Result<ConversationEvent, AppError> {
        redact_secrets(&mut payload);
        let bounded = bound_event_payload(&mut payload)?;
        if let Some(original_bytes) = bounded.original_bytes {
            tracing::warn!(
                conversation_id,
                event_type,
                original_bytes,
                bounded_bytes = bounded.bytes,
                "oversized conversation event payload truncated"
            );
        }
        // Safety net: bounding always lands under the budget, so this only
        // trips if that invariant is ever broken.
        if bounded.bytes > MAX_EVENT_PAYLOAD_BYTES {
            return Err(AppError::InvalidRequest(format!(
                "conversation event payload exceeds {MAX_EVENT_PAYLOAD_BYTES} bytes"
            )));
        }
        let directory = self.directory(conversation_id)?;
        let mut manifest = self.get_unlocked(conversation_id).await?;
        let persisted_status = manifest.status;
        let (last_event, terminated) = self.read_last_event(conversation_id).await?;
        let journal_sequence = last_event.as_ref().map_or(0, |event| event.sequence);
        if manifest.last_sequence != journal_sequence {
            // The journal is the commit point, so it wins in both directions.
            // Behind: appends that keep the status only reach manifest.json
            // through the debounced flush, so a crash can leave it any number
            // of events behind. Its status is still right, because every
            // status change is written before its append returns; only the
            // newest event can have lost that write to a crash, so it alone
            // is replayed onto the status. Ahead: tail recovery cut records
            // off the journal.
            tracing::warn!(
                conversation_id,
                manifest_sequence = manifest.last_sequence,
                journal_sequence,
                "conversation manifest disagrees with its journal; following the journal"
            );
            if let Some(last_event) = last_event
                .as_ref()
                .filter(|_| journal_sequence > manifest.last_sequence)
            {
                manifest.status = status_after_conversation_event(manifest.status, last_event);
                manifest.updated_at = last_event.time;
            }
            manifest.last_sequence = journal_sequence;
        }
        let mut event = ConversationEvent::new(
            conversation_id,
            manifest.last_sequence.saturating_add(1),
            event_type,
            payload,
        );
        event.provider = Some(manifest.provider);
        // An unterminated final record (a write interrupted before its
        // newline) must not absorb this one; start a fresh line instead.
        let separator = u64::from(!terminated);
        let mut line = Vec::new();
        if !terminated {
            line.push(b'\n');
        }
        serde_json::to_writer(&mut line, &event)?;
        line.push(b'\n');
        let event_path = directory.join(EVENTS_FILE);
        let (mut journal_bytes, mut journal_modified) = journal_metadata(&event_path).await?;
        if journal_bytes.saturating_add(line.len() as u64) > MAX_EVENTS_JOURNAL_BYTES {
            self.compact_journal(conversation_id).await?;
            (journal_bytes, journal_modified) = journal_metadata(&event_path).await?;
            if journal_bytes.saturating_add(line.len() as u64) > MAX_EVENTS_JOURNAL_BYTES {
                return Err(AppError::ResourceExhausted(format!(
                    "conversation {conversation_id} journal reached its storage limit"
                )));
            }
        }
        // `create` normally made the journal; only a missing one is created
        // here, and only then do its permissions and directory entry need work.
        let created = journal_bytes == 0 && journal_modified.is_none();
        let mut file = tokio::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&event_path)
            .await?;
        if created {
            set_owner_only(&event_path, false).await?;
            sync_directory(&directory).await?;
        }
        file.write_all(&line).await?;
        file.flush().await?;
        file.sync_data().await?;
        let metadata = file.metadata().await?;
        if let Some(mut index) = self.indexes.get_mut(conversation_id) {
            if index.bytes == journal_bytes && index.modified == journal_modified {
                index.offsets.push((
                    journal_bytes + separator,
                    journal_bytes + line.len() as u64 - 1,
                ));
                index.bytes = metadata.len();
                index.modified = metadata.modified().ok();
            }
        }
        self.tails.insert(
            conversation_id.to_owned(),
            JournalTail {
                bytes: metadata.len(),
                modified: metadata.modified().ok(),
                event: event.clone(),
            },
        );

        // The synced journal line above is the commit point. The manifest only
        // mirrors it: status changes are written now (recovery and clients
        // key off them), everything else is flushed by the debounce timer.
        manifest.last_sequence = event.sequence;
        manifest.status = status_after_conversation_event(manifest.status, &event);
        manifest.updated_at = event.time;
        if manifest.status == persisted_status {
            self.mark_manifest_dirty_locked(manifest);
        } else {
            self.persist_manifest_locked(&manifest).await?;
        }
        if let Some(hub) = hub {
            hub.publish(event.clone());
        }
        Ok(event)
    }

    pub async fn save_request(
        &self,
        conversation_id: &str,
        request: &Value,
    ) -> Result<(), AppError> {
        let _guard = self.lock(conversation_id).await;
        let directory = self.directory(conversation_id)?;
        self.get_unlocked(conversation_id).await?;
        // Every prompt saves its request before the turn starts, so this is
        // where a nearly full history refuses new turns.
        self.ensure_prompt_capacity_locked(conversation_id).await?;
        write_atomic_json(&directory.join("last-request.json"), request).await
    }

    /// Refuse a new turn once the journal is within
    /// `MAX_EVENTS_JOURNAL_BYTES - JOURNAL_PROMPT_LIMIT_BYTES` of the hard
    /// cap and compaction cannot free space. Running turns keep appending up
    /// to the cap. Callers hold the conversation lock.
    async fn ensure_prompt_capacity_locked(&self, conversation_id: &str) -> Result<(), AppError> {
        let event_path = self.directory(conversation_id)?.join(EVENTS_FILE);
        if journal_metadata(&event_path).await?.0 <= JOURNAL_PROMPT_LIMIT_BYTES {
            return Ok(());
        }
        self.compact_journal(conversation_id).await?;
        let (bytes, _) = journal_metadata(&event_path).await?;
        if bytes <= JOURNAL_PROMPT_LIMIT_BYTES {
            return Ok(());
        }
        Err(AppError::JournalFull(format!(
            "conversation {conversation_id} holds {bytes} journal bytes, above the \
             {JOURNAL_PROMPT_LIMIT_BYTES} byte limit for new turns; start a new conversation"
        )))
    }

    pub async fn last_request(&self, conversation_id: &str) -> Result<Option<Value>, AppError> {
        let _guard = self.lock(conversation_id).await;
        let path = self.directory(conversation_id)?.join("last-request.json");
        if !tokio::fs::try_exists(&path).await? {
            return Ok(None);
        }
        Ok(Some(
            read_json(&path, "conversation request snapshot").await?,
        ))
    }

    pub async fn complete_history(
        &self,
        conversation_id: &str,
    ) -> Result<Vec<ConversationEvent>, AppError> {
        let _guard = self.lock(conversation_id).await;
        self.flush_pending_delta_logged(conversation_id).await;
        self.read_and_recover_events(conversation_id).await
    }

    /// Internal complete scan: unlike the public paginated API this never truncates.
    pub async fn last_user_message(
        &self,
        conversation_id: &str,
    ) -> Result<Option<ConversationEvent>, AppError> {
        let _guard = self.lock(conversation_id).await;
        self.flush_pending_delta_logged(conversation_id).await;
        let events = self.read_and_recover_events(conversation_id).await?;
        Ok(events.into_iter().rev().find(|event| {
            event.event_type == "message.created"
                && event.payload.get("role").and_then(Value::as_str) == Some("user")
        }))
    }

    /// Forward replay of events after `after_sequence`, at most `limit`
    /// records and [`MAX_REPLAY_PAGE_BYTES`] of journal (never fewer than one
    /// record). `has_more` reports whether later events remain; clients
    /// continue from `next_sequence`.
    pub async fn replay(
        &self,
        conversation_id: &str,
        after_sequence: u64,
        limit: usize,
    ) -> Result<ConversationReplay, AppError> {
        let _guard = self.lock(conversation_id).await;
        // Readers see every fragment emitted so far.
        self.flush_pending_delta_logged(conversation_id).await;
        let limit = limit.clamp(1, MAX_REPLAY_LIMIT);
        let event_path = self.replay_journal(conversation_id).await?;
        let (_, to, total, events) = self
            .read_indexed_page(conversation_id, &event_path, PageAnchor::Start, |total| {
                let from = usize::try_from(after_sequence)
                    .unwrap_or(usize::MAX)
                    .min(total);
                (from, from.saturating_add(limit).min(total))
            })
            .await?;
        let next_sequence = events.last().map_or(after_sequence, |event| event.sequence);
        Ok(ConversationReplay {
            conversation_id: conversation_id.to_owned(),
            from_sequence: after_sequence,
            next_sequence,
            has_more: to < total,
            events,
        })
    }

    /// Reverse replay for lazy history loading: returns the newest events with
    /// `sequence <= before_sequence` in ascending order. `has_more` reports
    /// whether earlier events remain, so clients page back with
    /// `before_sequence = first_returned_sequence - 1` (`from_sequence`). The
    /// same byte budget as [`Self::replay`] applies, dropping the oldest
    /// records of the window first.
    pub async fn replay_before(
        &self,
        conversation_id: &str,
        before_sequence: u64,
        limit: usize,
    ) -> Result<ConversationReplay, AppError> {
        let _guard = self.lock(conversation_id).await;
        // Readers see every fragment emitted so far.
        self.flush_pending_delta_logged(conversation_id).await;
        let limit = limit.clamp(1, MAX_REPLAY_LIMIT);
        let event_path = self.replay_journal(conversation_id).await?;
        let (from, _, _, events) = self
            .read_indexed_page(conversation_id, &event_path, PageAnchor::End, |total| {
                let to = usize::try_from(before_sequence)
                    .unwrap_or(usize::MAX)
                    .min(total);
                (to.saturating_sub(limit), to)
            })
            .await?;
        let next_sequence = events
            .last()
            .map_or(before_sequence, |event| event.sequence);
        Ok(ConversationReplay {
            conversation_id: conversation_id.to_owned(),
            from_sequence: from as u64,
            next_sequence,
            has_more: from > 0,
            events,
        })
    }

    /// Shared replay prelude: the manifest must exist and the byte-offset
    /// index — a rebuildable cache — must match the journal file. Callers hold
    /// the conversation lock.
    async fn replay_journal(&self, conversation_id: &str) -> Result<PathBuf, AppError> {
        let directory = self.directory(conversation_id)?;
        if !tokio::fs::try_exists(directory.join(MANIFEST_FILE)).await? {
            return Err(AppError::NotFound(format!(
                "conversation {conversation_id}"
            )));
        }
        let event_path = directory.join(EVENTS_FILE);
        let metadata = tokio::fs::metadata(&event_path).await?;
        let valid_index = self.indexes.get(conversation_id).is_some_and(|index| {
            index.bytes == metadata.len() && index.modified == metadata.modified().ok()
        });
        if !valid_index
            && !self
                .index_journal_fast(conversation_id, &event_path)
                .await?
        {
            // Validate and repair once; the byte index is only a rebuildable cache.
            self.read_and_recover_events(conversation_id).await?;
        }
        Ok(event_path)
    }

    /// Cold index build without deserializing every record: a newline scan
    /// plus validation of the first and last records. Returns `false` when
    /// the journal is not in the clean shape appends leave behind (see
    /// [`scan_journal_offsets`]) so the caller runs the full validating scan
    /// with its tail repair.
    async fn index_journal_fast(
        &self,
        conversation_id: &str,
        event_path: &Path,
    ) -> Result<bool, AppError> {
        let path = event_path.to_path_buf();
        let id = conversation_id.to_owned();
        let scanned = tokio::task::spawn_blocking(move || scan_journal_offsets(&path, &id))
            .await
            .map_err(|error| AppError::Anyhow(error.into()))??;
        let Some(scanned) = scanned else {
            tracing::debug!(
                conversation_id,
                "journal needs a full scan to build its replay index"
            );
            return Ok(false);
        };
        self.indexes.insert(
            conversation_id.to_owned(),
            JournalIndex {
                bytes: scanned.bytes,
                modified: scanned.modified,
                offsets: scanned.offsets,
                fully_validated: false,
            },
        );
        self.tails.insert(
            conversation_id.to_owned(),
            JournalTail {
                bytes: scanned.bytes,
                modified: scanned.modified,
                event: scanned.last,
            },
        );
        Ok(true)
    }

    /// Read the page `window(total)` selects from the current index, cut to
    /// [`MAX_REPLAY_PAGE_BYTES`] from its `anchor` end before any record is
    /// read. When a fast-built index meets a record that does not parse or
    /// validate, the full scan runs so corruption is reported (or salvaged)
    /// exactly as an eagerly validated index would, then the page is read
    /// again. Returns `(from, to, total, events)`.
    async fn read_indexed_page(
        &self,
        conversation_id: &str,
        event_path: &Path,
        anchor: PageAnchor,
        window: impl Fn(usize) -> (usize, usize),
    ) -> Result<(usize, usize, usize, Vec<ConversationEvent>), AppError> {
        let total = self.journal_len(conversation_id);
        let (from, to) = self.budgeted_window(conversation_id, window(total), anchor);
        let error = match self
            .read_replay_window(conversation_id, event_path, from, to)
            .await
        {
            Ok(events) => return Ok((from, to, total, events)),
            Err(error) => error,
        };
        let fully_validated = self
            .indexes
            .get(conversation_id)
            .is_none_or(|index| index.fully_validated);
        if fully_validated {
            return Err(error);
        }
        tracing::warn!(conversation_id, error = %error, "unreadable record behind the fast journal index; running full validation");
        self.read_and_recover_events(conversation_id).await?;
        let total = self.journal_len(conversation_id);
        let (from, to) = self.budgeted_window(conversation_id, window(total), anchor);
        let events = self
            .read_replay_window(conversation_id, event_path, from, to)
            .await?;
        Ok((from, to, total, events))
    }

    fn budgeted_window(
        &self,
        conversation_id: &str,
        window: (usize, usize),
        anchor: PageAnchor,
    ) -> (usize, usize) {
        match self.indexes.get(conversation_id) {
            Some(index) => budget_window(&index.offsets, window, anchor, MAX_REPLAY_PAGE_BYTES),
            None => window,
        }
    }

    fn journal_len(&self, conversation_id: &str) -> usize {
        self.indexes
            .get(conversation_id)
            .map(|index| index.offsets.len())
            .unwrap_or_default()
    }

    /// Read `offsets[from..to]` as one contiguous journal page. `from` is the
    /// absolute index so sequence validation still matches journal positions.
    async fn read_replay_window(
        &self,
        conversation_id: &str,
        event_path: &Path,
        from: usize,
        to: usize,
    ) -> Result<Vec<ConversationEvent>, AppError> {
        let offsets = {
            let index = self.indexes.get(conversation_id);
            index
                .map(|entry| {
                    let len = entry.offsets.len();
                    entry.offsets[from.min(len)..to.min(len)].to_vec()
                })
                .unwrap_or_default()
        };
        let mut events = Vec::with_capacity(offsets.len());
        if let (Some(first), Some(last)) = (offsets.first(), offsets.last()) {
            let start = first.0;
            let end = last.1;
            let mut file = tokio::fs::File::open(event_path).await?;
            file.seek(std::io::SeekFrom::Start(start)).await?;
            let mut page = vec![0u8; (end - start) as usize];
            file.read_exact(&mut page).await?;
            for (index, (start_offset, end_offset)) in offsets.iter().enumerate() {
                let event: ConversationEvent = serde_json::from_slice(
                    &page[(start_offset - start) as usize..(end_offset - start) as usize],
                )?;
                validate_event(&event, conversation_id, (from + index + 1) as u64)?;
                events.push(event);
            }
        }
        Ok(events)
    }

    #[cfg_attr(not(test), allow(dead_code))]
    pub async fn recover(&self, conversation_id: &str) -> Result<ConversationManifest, AppError> {
        self.recover_with_history(conversation_id)
            .await
            .map(|(manifest, _)| manifest)
    }

    pub async fn recover_with_history(
        &self,
        conversation_id: &str,
    ) -> Result<(ConversationManifest, Vec<ConversationEvent>), AppError> {
        let _guard = self.lock(conversation_id).await;
        self.flush_pending_delta_logged(conversation_id).await;
        let mut manifest = self.get_unlocked(conversation_id).await?;
        let events = self.read_and_recover_events(conversation_id).await?;
        let last_sequence = events.last().map_or(0, |event| event.sequence);
        let mut status = super::ConversationStatus::Idle;
        for event in &events {
            status = status_after_conversation_event(status, event);
        }
        if status == super::ConversationStatus::Running
            || status == super::ConversationStatus::WaitingPermission
        {
            status = super::ConversationStatus::Interrupted;
        }
        manifest.last_sequence = last_sequence;
        manifest.status = status;
        manifest.updated_at = events
            .last()
            .map_or(manifest.updated_at, |event| event.time);
        self.persist_manifest_locked(&manifest).await?;
        Ok((manifest, events))
    }

    pub async fn provider_state(&self, conversation_id: &str) -> Result<ProviderState, AppError> {
        let directory = self.directory(conversation_id)?;
        read_json(&directory.join(PROVIDER_STATE_FILE), "provider state").await
    }

    pub async fn save_provider_state(
        &self,
        conversation_id: &str,
        mut state: ProviderState,
    ) -> Result<(), AppError> {
        let _guard = self.lock(conversation_id).await;
        let manifest = self.get_unlocked(conversation_id).await?;
        if manifest.provider != state.provider {
            return Err(AppError::InvalidRequest(
                "provider state does not match conversation provider".to_owned(),
            ));
        }
        state.updated_at = Utc::now();
        let directory = self.directory(conversation_id)?;
        write_atomic_json(&directory.join(PROVIDER_STATE_FILE), &state).await
    }

    /// Cached manifest, loaded from disk on a miss. Callers hold the
    /// conversation lock.
    async fn get_unlocked(&self, conversation_id: &str) -> Result<ConversationManifest, AppError> {
        if let Some(cached) = self.manifests.get(conversation_id) {
            return Ok(cached.manifest.clone());
        }
        let directory = self.directory(conversation_id)?;
        let manifest: ConversationManifest =
            read_json(&directory.join(MANIFEST_FILE), "conversation manifest").await?;
        validate_manifest(&manifest, conversation_id)?;
        self.manifests.insert(
            conversation_id.to_owned(),
            CachedManifest {
                manifest: manifest.clone(),
                dirty: false,
                flush_scheduled: false,
            },
        );
        Ok(manifest)
    }

    /// Write the manifest and its snapshot now. A failed write leaves the
    /// cache authoritative and dirty so the debounce timer retries it.
    /// Callers hold the conversation lock.
    async fn persist_manifest_locked(
        &self,
        manifest: &ConversationManifest,
    ) -> Result<(), AppError> {
        let directory = self.directory(&manifest.id)?;
        let written = async {
            write_atomic_json(&directory.join(MANIFEST_FILE), manifest).await?;
            write_atomic_json(
                &directory.join(SNAPSHOT_FILE),
                &ConversationSnapshot::from_manifest(manifest),
            )
            .await
        }
        .await;
        if let Err(error) = written {
            self.mark_manifest_dirty_locked(manifest.clone());
            return Err(error);
        }
        let mut cached = self
            .manifests
            .entry(manifest.id.clone())
            .or_insert_with(|| CachedManifest {
                manifest: manifest.clone(),
                dirty: false,
                flush_scheduled: false,
            });
        cached.manifest = manifest.clone();
        cached.dirty = false;
        Ok(())
    }

    /// Cache a manifest newer than `manifest.json` and make sure a flush is
    /// pending. Callers hold the conversation lock.
    fn mark_manifest_dirty_locked(&self, manifest: ConversationManifest) {
        let conversation_id = manifest.id.clone();
        let mut cached = self
            .manifests
            .entry(conversation_id.clone())
            .or_insert_with(|| CachedManifest {
                manifest: manifest.clone(),
                dirty: true,
                flush_scheduled: false,
            });
        cached.manifest = manifest;
        cached.dirty = true;
        if cached.flush_scheduled {
            return;
        }
        cached.flush_scheduled = true;
        // Release the shard lock before spawning.
        drop(cached);
        let store = self.clone();
        tokio::spawn(async move {
            tokio::time::sleep(MANIFEST_FLUSH_INTERVAL).await;
            let _guard = store.lock(&conversation_id).await;
            if let Some(mut cached) = store.manifests.get_mut(&conversation_id) {
                cached.flush_scheduled = false;
            }
            store.flush_manifest_logged(&conversation_id).await;
        });
    }

    /// For flushes without a caller to report to (timer, shutdown). The
    /// entry stays dirty, so the next status change or flush retries.
    async fn flush_manifest_logged(&self, conversation_id: &str) {
        if let Err(error) = self.flush_manifest_locked(conversation_id).await {
            tracing::error!(conversation_id, error = %error, "failed to persist conversation manifest");
        }
    }

    /// Write a dirty cached manifest. Callers hold the conversation lock.
    async fn flush_manifest_locked(&self, conversation_id: &str) -> Result<(), AppError> {
        let manifest = match self.manifests.get(conversation_id) {
            Some(cached) if cached.dirty => cached.manifest.clone(),
            _ => return Ok(()),
        };
        let directory = self.directory(conversation_id)?;
        write_atomic_json(&directory.join(MANIFEST_FILE), &manifest).await?;
        if let Some(mut cached) = self.manifests.get_mut(conversation_id) {
            cached.dirty = false;
        }
        Ok(())
    }

    async fn read_and_recover_events(
        &self,
        conversation_id: &str,
    ) -> Result<Vec<ConversationEvent>, AppError> {
        let path = self.directory(conversation_id)?.join(EVENTS_FILE);
        let metadata = match tokio::fs::metadata(&path).await {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(error) => return Err(error.into()),
        };
        if metadata.len() > MAX_EVENTS_JOURNAL_BYTES {
            return Err(AppError::InvalidRequest(format!(
                "conversation {conversation_id} journal exceeds {MAX_EVENTS_JOURNAL_BYTES} bytes"
            )));
        }
        let raw = match tokio::fs::read(&path).await {
            Ok(raw) => raw,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(error) => return Err(error.into()),
        };
        let salvaged = salvage_journal(&raw, conversation_id);
        let repaired = salvaged.interior_damage || salvaged.corrupt_tail.is_some();
        let (events, offsets) = if salvaged.interior_damage {
            // Lines before the last valid record are damaged: back the whole
            // journal up and rewrite it with placeholders for lost sequences.
            // A corrupt tail after that record is dropped by the rewrite.
            rewrite_salvaged_journal(conversation_id, &path, &raw, salvaged).await?
        } else {
            let mut events = Vec::with_capacity(salvaged.entries.len());
            let mut offsets = Vec::with_capacity(salvaged.entries.len());
            for entry in salvaged.entries {
                if let SalvagedEntry::Record { event, start, end } = entry {
                    offsets.push((start as u64, end as u64));
                    events.push(event);
                }
            }
            if let Some(start) = salvaged.corrupt_tail {
                // Nothing valid follows these lines, so they are an
                // interrupted write rather than lost history: quarantine
                // them and cut the journal back to its last valid record.
                quarantine_tail(&path, &raw[start..]).await?;
                let file = tokio::fs::OpenOptions::new()
                    .write(true)
                    .open(&path)
                    .await?;
                file.set_len(start as u64).await?;
                file.sync_all().await?;
                tracing::warn!(
                    conversation_id,
                    quarantined_bytes = raw.len() - start,
                    "recovered invalid conversation journal tail"
                );
            } else if raw.last().is_some_and(|byte| *byte != b'\n') {
                // A complete record whose trailing newline never reached the
                // disk parses fine, but the next O_APPEND write would glue its
                // record onto it and a later scan would quarantine both.
                // Terminate it now; the recorded offsets already end where the
                // newline lands.
                terminate_journal(&path).await?;
                tracing::warn!(
                    conversation_id,
                    "terminated conversation journal missing its final newline"
                );
            }
            (events, offsets)
        };
        if repaired {
            self.follow_repaired_journal_locked(
                conversation_id,
                events.last().map_or(0, |event| event.sequence),
            )
            .await?;
        }
        let metadata = tokio::fs::metadata(&path).await?;
        self.indexes.insert(
            conversation_id.to_owned(),
            JournalIndex {
                bytes: metadata.len(),
                modified: metadata.modified().ok(),
                offsets,
                fully_validated: true,
            },
        );
        if let Some(event) = events.last() {
            self.tails.insert(
                conversation_id.to_owned(),
                JournalTail {
                    bytes: metadata.len(),
                    modified: metadata.modified().ok(),
                    event: event.clone(),
                },
            );
        } else {
            self.tails.remove(conversation_id);
        }
        Ok(events)
    }

    /// A repair can leave the journal ending below the cached manifest, which
    /// would make subscribers wait for sequences that no longer exist. Pull
    /// `last_sequence` back to the journal; a manifest that is merely behind
    /// is left to [`Self::append_locked`], which also replays the status.
    /// Callers hold the conversation lock.
    async fn follow_repaired_journal_locked(
        &self,
        conversation_id: &str,
        journal_sequence: u64,
    ) -> Result<(), AppError> {
        let mut manifest = self.get_unlocked(conversation_id).await?;
        if manifest.last_sequence > journal_sequence {
            manifest.last_sequence = journal_sequence;
            self.mark_manifest_dirty_locked(manifest);
        }
        Ok(())
    }

    /// The newest journal record plus whether the journal ends with a
    /// newline. `false` means an interrupted write left the final record
    /// unterminated, so the next append must start a new line first.
    async fn read_last_event(
        &self,
        conversation_id: &str,
    ) -> Result<(Option<ConversationEvent>, bool), AppError> {
        let path = self.directory(conversation_id)?.join(EVENTS_FILE);
        let metadata = match tokio::fs::metadata(&path).await {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok((None, true)),
            Err(error) => return Err(error.into()),
        };
        // Every writer that fills the tail cache leaves the journal
        // newline-terminated (appends, recovery, the cold scan, compaction).
        if let Some(tail) = self.tails.get(conversation_id) {
            if tail.bytes == metadata.len() && tail.modified == metadata.modified().ok() {
                return Ok((Some(tail.event.clone()), true));
            }
        }
        if metadata.len() > MAX_EVENTS_JOURNAL_BYTES {
            return Err(AppError::InvalidRequest(format!(
                "conversation {conversation_id} journal exceeds {MAX_EVENTS_JOURNAL_BYTES} bytes"
            )));
        }
        if metadata.len() == 0 {
            return Ok((None, true));
        }

        let window = (MAX_EVENT_PAYLOAD_BYTES as u64 + 64 * 1024).min(metadata.len());
        let mut file = tokio::fs::File::open(&path).await?;
        file.seek(std::io::SeekFrom::End(-(window as i64))).await?;
        let mut raw = Vec::with_capacity(window as usize);
        file.read_to_end(&mut raw).await?;
        let terminated = raw.last().is_none_or(|byte| *byte == b'\n');
        if window < metadata.len() {
            let Some(first_newline) = raw.iter().position(|byte| *byte == b'\n') else {
                return Err(AppError::InvalidRequest(
                    "conversation journal event exceeds the payload limit".to_owned(),
                ));
            };
            raw.drain(..=first_newline);
        }
        let line = raw
            .split(|byte| *byte == b'\n')
            .rev()
            .find(|line| !trim_ascii(line).is_empty())
            .map(trim_ascii);
        let Some(line) = line else {
            return Ok((None, terminated));
        };
        let event: ConversationEvent = serde_json::from_slice(line).map_err(|error| {
            AppError::InvalidRequest(format!(
                "conversation {conversation_id} journal tail is invalid: {error}"
            ))
        })?;
        if event.schema_version != CONVERSATION_SCHEMA_VERSION
            || event.conversation_id != conversation_id
        {
            return Err(AppError::InvalidRequest(format!(
                "conversation {conversation_id} journal tail does not match its manifest"
            )));
        }
        Ok((Some(event), terminated))
    }

    /// Free journal space without disturbing replay cursors: oversized payload
    /// strings in older events are truncated in place while every event keeps
    /// its sequence. The newest `JOURNAL_COMPACT_PROTECTED_BYTES` of events
    /// stay byte-identical so recent history retains full fidelity. Callers
    /// must hold the conversation lock.
    async fn compact_journal(&self, conversation_id: &str) -> Result<(), AppError> {
        let directory = self.directory(conversation_id)?;
        let path = directory.join(EVENTS_FILE);
        let journal = journal_metadata(&path).await?;
        if self
            .incompressible
            .get(conversation_id)
            .is_some_and(|known| *known == journal)
        {
            return Ok(());
        }
        let raw = match tokio::fs::read(&path).await {
            Ok(raw) => raw,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
            Err(error) => return Err(error.into()),
        };
        let mut events = Vec::new();
        for (start, end) in line_ranges(&raw) {
            let line = trim_ascii(&raw[start..end]);
            if line.is_empty() {
                continue;
            }
            let event: ConversationEvent = serde_json::from_slice(line).map_err(|error| {
                AppError::InvalidRequest(format!(
                    "conversation {conversation_id} journal is corrupt at sequence {}: {error}",
                    events.len() + 1
                ))
            })?;
            validate_event(&event, conversation_id, events.len() as u64 + 1)?;
            events.push(event);
        }
        // Records without their newline; each costs one more journal byte.
        let mut lines = Vec::with_capacity(events.len());
        let mut total = 0u64;
        for event in &events {
            let line = serde_json::to_vec(event)?;
            total += line.len() as u64 + 1;
            lines.push(line);
        }
        if total <= JOURNAL_COMPACT_TARGET_BYTES {
            self.incompressible
                .insert(conversation_id.to_owned(), journal);
            return Ok(());
        }
        let mut protected = 0u64;
        let mut protected_from = lines.len();
        while protected_from > 0 && protected < JOURNAL_COMPACT_PROTECTED_BYTES {
            protected_from -= 1;
            protected += lines[protected_from].len() as u64 + 1;
        }
        // Truncate the bulkiest unprotected events first: the fewest possible
        // events lose payload fidelity before the journal fits again.
        let mut candidates: Vec<usize> = (0..protected_from).collect();
        candidates.sort_by_key(|index| std::cmp::Reverse(lines[*index].len()));
        let mut truncated = 0usize;
        for index in candidates {
            if total <= JOURNAL_COMPACT_TARGET_BYTES {
                break;
            }
            if !truncate_journal_strings(&mut events[index].payload) {
                continue;
            }
            let line = serde_json::to_vec(&events[index])?;
            total = total - lines[index].len() as u64 + line.len() as u64;
            lines[index] = line;
            truncated += 1;
        }
        if truncated == 0 {
            self.incompressible
                .insert(conversation_id.to_owned(), journal);
            return Ok(());
        }
        self.incompressible.remove(conversation_id);
        let offsets = replace_journal(&path, &lines).await?;
        let metadata = tokio::fs::metadata(&path).await?;
        self.indexes.insert(
            conversation_id.to_owned(),
            JournalIndex {
                bytes: metadata.len(),
                modified: metadata.modified().ok(),
                offsets,
                fully_validated: true,
            },
        );
        if let Some(event) = events.last() {
            self.tails.insert(
                conversation_id.to_owned(),
                JournalTail {
                    bytes: metadata.len(),
                    modified: metadata.modified().ok(),
                    event: event.clone(),
                },
            );
        }
        tracing::warn!(
            conversation_id,
            truncated_events = truncated,
            journal_bytes = metadata.len(),
            "conversation journal compacted: oversized payloads truncated"
        );
        Ok(())
    }

    fn directory(&self, conversation_id: &str) -> Result<PathBuf, AppError> {
        validate_id(conversation_id)?;
        Ok(self.root.join(conversation_id))
    }

    async fn lock(&self, conversation_id: &str) -> OwnedMutexGuard<()> {
        // Drop the DashMap shard guard before awaiting the async mutex. Holding
        // it across await can block every executor thread under concurrent writes.
        let lock = self
            .locks
            .entry(conversation_id.to_owned())
            .or_insert_with(|| Arc::new(Mutex::new(())))
            .clone();
        lock.lock_owned().await
    }
}

async fn journal_metadata(path: &Path) -> Result<(u64, Option<std::time::SystemTime>), AppError> {
    match tokio::fs::metadata(path).await {
        Ok(metadata) => Ok((metadata.len(), metadata.modified().ok())),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok((0, None)),
        Err(error) => Err(error.into()),
    }
}

/// Serialized size an oversized payload is cut down to. The headroom below
/// `MAX_EVENT_PAYLOAD_BYTES` absorbs the truncation bookkeeping, so the hard
/// limit only rejects payloads that could not be bounded at all.
const EVENT_PAYLOAD_BUDGET_BYTES: usize = MAX_EVENT_PAYLOAD_BYTES - 16 * 1024;
/// Strings at or below this serialized size are never cut; trimming them
/// frees too little to be worth losing their content.
const BOUND_STRING_MIN_BYTES: usize = 512;
/// Every cut string keeps at least this much serialized prefix.
const BOUND_STRING_KEEP_BYTES: usize = 256;
/// Short top-level scalars (ids such as `turnId`) survive a payload that has
/// to be replaced wholesale, up to this many serialized bytes in total.
const BOUND_PRESERVED_SCALAR_BYTES: usize = 4 * 1024;

struct BoundedPayload {
    /// Serialized size after bounding.
    bytes: usize,
    /// Serialized size before bounding, when the payload had to change.
    original_bytes: Option<usize>,
}

/// Fit an oversized payload under [`EVENT_PAYLOAD_BUDGET_BYTES`] instead of
/// rejecting the event: the largest strings are cut at a character boundary
/// and end with `…[truncated N bytes]`, and the original byte length of each
/// cut string is recorded by JSON pointer under a top-level `truncated` map
/// (`_truncated` when the payload already uses that key; no map when the
/// payload is not an object). When strings alone cannot make it fit (bulk
/// made of many small values) the payload is replaced by
/// `{"truncated": true, "originalBytes": N}` plus its short top-level scalars.
/// Payloads within the budget are left byte-for-byte untouched.
fn bound_event_payload(payload: &mut Value) -> Result<BoundedPayload, AppError> {
    let original = serde_json::to_vec(payload)?.len();
    if original <= EVENT_PAYLOAD_BUDGET_BYTES {
        return Ok(BoundedPayload {
            bytes: original,
            original_bytes: None,
        });
    }
    let record_key = payload.as_object().and_then(|map| {
        ["truncated", "_truncated"]
            .into_iter()
            .find(|key| !map.contains_key(*key))
    });
    let mut leaves = Vec::new();
    collect_string_leaves(payload, &mut String::new(), &mut leaves);
    leaves.sort_by_key(|(_, bytes)| std::cmp::Reverse(*bytes));
    let mut leaves = leaves.into_iter().peekable();
    let mut records = serde_json::Map::new();
    let mut size = original;
    while size > EVENT_PAYLOAD_BUDGET_BYTES {
        let mut excess = size - EVENT_PAYLOAD_BUDGET_BYTES;
        let mut progressed = false;
        while excess > 0 {
            let Some((pointer, serialized)) =
                leaves.next_if(|(_, bytes)| *bytes > BOUND_STRING_MIN_BYTES)
            else {
                break;
            };
            let Some(Value::String(text)) = payload.pointer_mut(&pointer) else {
                continue;
            };
            // `"<pointer>":<bytes>,` in the record map plus the marker.
            let overhead = if record_key.is_some() {
                serialized_string_bytes(&pointer) + 24
            } else {
                0
            } + 48;
            let keep = serialized
                .saturating_sub(excess + overhead)
                .max(BOUND_STRING_KEEP_BYTES);
            let original_bytes = text.len();
            let cut = serialized_prefix_len(text, keep - 2);
            text.truncate(cut);
            text.push_str(&format!("…[truncated {} bytes]", original_bytes - cut));
            let freed = serialized.saturating_sub(serialized_string_bytes(text) + overhead);
            excess = excess.saturating_sub(freed);
            records.insert(pointer, Value::from(original_bytes));
            progressed = true;
        }
        if !progressed {
            *payload = replacement_payload(payload, original);
            return Ok(BoundedPayload {
                bytes: serde_json::to_vec(payload)?.len(),
                original_bytes: Some(original),
            });
        }
        if let (Some(key), Value::Object(map)) = (record_key, &mut *payload) {
            map.insert(key.to_owned(), Value::Object(records.clone()));
        }
        size = serde_json::to_vec(payload)?.len();
    }
    Ok(BoundedPayload {
        bytes: size,
        original_bytes: Some(original),
    })
}

/// `(JSON pointer, serialized bytes)` of every string in `value`.
fn collect_string_leaves(value: &Value, pointer: &mut String, leaves: &mut Vec<(String, usize)>) {
    let parent = pointer.len();
    match value {
        Value::String(text) => leaves.push((pointer.clone(), serialized_string_bytes(text))),
        Value::Array(items) => {
            for (index, item) in items.iter().enumerate() {
                pointer.push('/');
                pointer.push_str(&index.to_string());
                collect_string_leaves(item, pointer, leaves);
                pointer.truncate(parent);
            }
        }
        Value::Object(map) => {
            for (key, item) in map {
                pointer.push('/');
                pointer.push_str(&key.replace('~', "~0").replace('/', "~1"));
                collect_string_leaves(item, pointer, leaves);
                pointer.truncate(parent);
            }
        }
        _ => {}
    }
}

fn replacement_payload(payload: &Value, original_bytes: usize) -> Value {
    let mut replacement = serde_json::Map::new();
    if let Value::Object(map) = payload {
        let mut preserved = 0usize;
        for (key, value) in map {
            let bytes = match value {
                Value::String(text) => serialized_string_bytes(text),
                Value::Number(_) | Value::Bool(_) | Value::Null => 24,
                Value::Array(_) | Value::Object(_) => continue,
            };
            preserved += bytes + serialized_string_bytes(key) + 2;
            if bytes > BOUND_STRING_KEEP_BYTES || preserved > BOUND_PRESERVED_SCALAR_BYTES {
                continue;
            }
            replacement.insert(key.clone(), value.clone());
        }
    }
    replacement.insert("truncated".to_owned(), Value::Bool(true));
    replacement.insert("originalBytes".to_owned(), Value::from(original_bytes));
    Value::Object(replacement)
}

/// Bytes serde_json writes for one character inside a string literal.
fn escaped_char_bytes(ch: char) -> usize {
    match ch {
        '"' | '\\' | '\n' | '\r' | '\t' | '\u{08}' | '\u{0c}' => 2,
        '\u{00}'..='\u{1f}' => 6,
        _ => ch.len_utf8(),
    }
}

/// Serialized size of `text` as a JSON string literal, quotes included.
fn serialized_string_bytes(text: &str) -> usize {
    2 + text.chars().map(escaped_char_bytes).sum::<usize>()
}

/// Longest prefix of `text`, ending on a character boundary, whose escaped
/// form fits in `limit` bytes. Returns its byte length.
fn serialized_prefix_len(text: &str, limit: usize) -> usize {
    let mut used = 0usize;
    for (index, ch) in text.char_indices() {
        used += escaped_char_bytes(ch);
        if used > limit {
            return index;
        }
    }
    text.len()
}

/// Truncate oversized strings inside a journal payload. Returns whether any
/// string changed so callers can skip reserializing untouched events. Every
/// branch is visited deliberately — `any` would short-circuit and leave later
/// strings untouched.
fn truncate_journal_strings(value: &mut Value) -> bool {
    match value {
        Value::String(text) if text.len() > JOURNAL_COMPACT_STRING_MAX => {
            let original = text.len();
            let kept: String = text.chars().take(JOURNAL_COMPACT_STRING_KEEP).collect();
            *text = format!("{kept}…[truncated from {original} bytes]");
            true
        }
        Value::Array(items) => {
            let mut changed = false;
            for item in items {
                changed |= truncate_journal_strings(item);
            }
            changed
        }
        Value::Object(map) => {
            let mut changed = false;
            for item in map.values_mut() {
                changed |= truncate_journal_strings(item);
            }
            changed
        }
        _ => false,
    }
}

fn validate_id(id: &str) -> Result<(), AppError> {
    match Uuid::parse_str(id) {
        Ok(parsed) if parsed.get_version_num() == 4 => Ok(()),
        _ => Err(AppError::InvalidRequest(
            "conversation id must be a UUID v4".to_owned(),
        )),
    }
}

fn validate_manifest(manifest: &ConversationManifest, expected_id: &str) -> Result<(), AppError> {
    if manifest.schema_version != CONVERSATION_SCHEMA_VERSION {
        return Err(AppError::InvalidRequest(format!(
            "conversation schema version {} is not supported",
            manifest.schema_version
        )));
    }
    if manifest.id != expected_id {
        return Err(AppError::InvalidRequest(
            "conversation manifest id does not match its directory".to_owned(),
        ));
    }
    if manifest.owner_id.trim().is_empty() || manifest.owner_id.len() > 256 {
        return Err(AppError::InvalidRequest(
            "conversation owner id is invalid".to_owned(),
        ));
    }
    Ok(())
}

fn validate_event(
    event: &ConversationEvent,
    conversation_id: &str,
    expected_sequence: u64,
) -> Result<(), AppError> {
    if event.schema_version != CONVERSATION_SCHEMA_VERSION
        || event.conversation_id != conversation_id
        || event.sequence != expected_sequence
    {
        return Err(AppError::InvalidRequest(format!(
            "conversation {conversation_id} journal continuity check failed at sequence {expected_sequence}"
        )));
    }
    if event.event_type == JOURNAL_RECORD_LOST_EVENT {
        return Ok(());
    }
    validate_event_type(&event.event_type)
}

fn validate_event_type(event_type: &str) -> Result<(), AppError> {
    let valid = !event_type.is_empty()
        && event_type.len() <= 96
        && event_type.bytes().all(|byte| {
            byte.is_ascii_lowercase() || byte.is_ascii_digit() || matches!(byte, b'.' | b'_' | b'-')
        });
    if valid {
        Ok(())
    } else {
        Err(AppError::InvalidRequest(
            "conversation event type is invalid".to_owned(),
        ))
    }
}

async fn read_json<T: DeserializeOwned>(path: &Path, label: &str) -> Result<T, AppError> {
    match tokio::fs::read(path).await {
        Ok(raw) => serde_json::from_slice(&raw)
            .map_err(|error| AppError::InvalidRequest(format!("invalid {label}: {error}"))),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            Err(AppError::NotFound(format!("{label} at {}", path.display())))
        }
        Err(error) => Err(error.into()),
    }
}

/// Replace `path` atomically. The parent directory must already exist, so a
/// late manifest flush can never resurrect a deleted conversation directory.
async fn write_atomic_json<T: Serialize>(path: &Path, value: &T) -> Result<(), AppError> {
    let parent = path
        .parent()
        .ok_or_else(|| AppError::InvalidRequest("persisted file has no parent".to_owned()))?;
    let file_name = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("state.json");
    let temporary = parent.join(format!(".{file_name}.{}.tmp", Uuid::new_v4().simple()));
    let mut bytes = serde_json::to_vec_pretty(value)?;
    bytes.push(b'\n');
    let mut file = tokio::fs::OpenOptions::new()
        .create_new(true)
        .write(true)
        .open(&temporary)
        .await?;
    set_owner_only(&temporary, false).await?;
    file.write_all(&bytes).await?;
    file.flush().await?;
    file.sync_all().await?;
    drop(file);
    #[cfg(windows)]
    if tokio::fs::try_exists(path).await? {
        tokio::fs::remove_file(path).await?;
    }
    tokio::fs::rename(&temporary, path).await?;
    set_owner_only(path, false).await?;
    sync_directory(parent).await
}

/// Append the newline an interrupted write left off the final record.
async fn terminate_journal(path: &Path) -> Result<(), AppError> {
    let mut file = tokio::fs::OpenOptions::new()
        .append(true)
        .open(path)
        .await?;
    file.write_all(b"\n").await?;
    file.flush().await?;
    file.sync_data().await?;
    Ok(())
}

async fn quarantine_tail(path: &Path, tail: &[u8]) -> Result<(), AppError> {
    if tail.is_empty() {
        return Ok(());
    }
    write_corrupt_copy(&corrupt_copy_path(path), tail).await
}

/// `events.corrupt.<UTC timestamp>.jsonl` next to the journal: where damaged
/// journal bytes are kept before recovery cuts or rewrites them.
fn corrupt_copy_path(path: &Path) -> PathBuf {
    path.with_file_name(format!(
        "events.corrupt.{}.jsonl",
        Utc::now().format("%Y%m%dT%H%M%S%.3fZ")
    ))
}

async fn write_corrupt_copy(copy: &Path, bytes: &[u8]) -> Result<(), AppError> {
    let mut file = tokio::fs::OpenOptions::new()
        .create_new(true)
        .write(true)
        .open(copy)
        .await?;
    set_owner_only(copy, false).await?;
    file.write_all(bytes).await?;
    file.flush().await?;
    file.sync_all().await?;
    Ok(())
}

/// Atomically replace the journal at `path` with `lines` (records without
/// their newline): temp file, fsync, rename, directory fsync. Returns the
/// replay index offsets of the new file.
async fn replace_journal<L: AsRef<[u8]>>(
    path: &Path,
    lines: &[L],
) -> Result<Vec<(u64, u64)>, AppError> {
    let directory = path
        .parent()
        .ok_or_else(|| AppError::InvalidRequest("journal has no parent directory".to_owned()))?;
    let temporary = directory.join(format!(".events.{}.tmp", Uuid::new_v4().simple()));
    let file = tokio::fs::OpenOptions::new()
        .create_new(true)
        .write(true)
        .open(&temporary)
        .await?;
    let written = async {
        set_owner_only(&temporary, false).await?;
        let mut writer = tokio::io::BufWriter::with_capacity(JOURNAL_SCAN_BUFFER_BYTES, file);
        let mut offsets = Vec::with_capacity(lines.len());
        let mut cursor = 0u64;
        for line in lines {
            let line = line.as_ref();
            writer.write_all(line).await?;
            writer.write_all(b"\n").await?;
            offsets.push((cursor, cursor + line.len() as u64));
            cursor += line.len() as u64 + 1;
        }
        writer.flush().await?;
        writer.into_inner().sync_all().await?;
        #[cfg(windows)]
        if tokio::fs::try_exists(path).await? {
            tokio::fs::remove_file(path).await?;
        }
        tokio::fs::rename(&temporary, path).await?;
        Ok::<_, AppError>(offsets)
    }
    .await;
    let offsets = match written {
        Ok(offsets) => offsets,
        Err(error) => {
            let _ = tokio::fs::remove_file(&temporary).await;
            return Err(error);
        }
    };
    set_owner_only(path, false).await?;
    sync_directory(directory).await?;
    Ok(offsets)
}

/// One entry of a salvaged journal, in sequence order.
// Records are the common variant; boxing them would cost an allocation per
// record on every full read to shrink the rare `Lost` entries.
#[allow(clippy::large_enum_variant)]
enum SalvagedEntry {
    /// A valid record and its line range in the original bytes.
    Record {
        event: ConversationEvent,
        start: usize,
        end: usize,
    },
    /// Sequences `run_start..run_start + run_length` that no valid line holds.
    Lost { run_start: u64, run_length: u64 },
}

/// Result of [`salvage_journal`].
struct SalvagedJournal {
    entries: Vec<SalvagedEntry>,
    /// Lines before the last valid record were dropped or sequences lost, so
    /// the journal must be rewritten.
    interior_damage: bool,
    /// Byte offset of corrupt lines no valid record follows.
    corrupt_tail: Option<usize>,
}

/// Classify every journal line. A line is corrupt when it does not parse as
/// one record of this conversation (garbage, a torn write, two records glued
/// onto one line) or its sequence does not continue the previous valid
/// record. Each corrupt run is bounded by the valid records around it: after
/// valid sequence `p`, the next valid record `q > p` resumes the journal and
/// `p + 1..q` are lost. `q` is only accepted while the lost count fits in the
/// corrupt lines seen (one record per line plus one per
/// [`MIN_JOURNAL_RECORD_BYTES`]); otherwise the record is itself treated as
/// corrupt, so a damaged sequence number cannot swallow the rest of the
/// journal. Corrupt lines with no valid record after them are a tail.
fn salvage_journal(raw: &[u8], conversation_id: &str) -> SalvagedJournal {
    let mut entries = Vec::new();
    let mut interior_damage = false;
    let mut expected = 1u64;
    // Corrupt lines since the last valid record: (first byte, lines, bytes).
    let mut pending: Option<(usize, u64, usize)> = None;
    for (start, end) in line_ranges(raw) {
        let line = trim_ascii(&raw[start..end]);
        if line.is_empty() {
            continue;
        }
        let record = serde_json::from_slice::<ConversationEvent>(line)
            .ok()
            .filter(|event| {
                event.sequence >= expected
                    && validate_event(event, conversation_id, event.sequence).is_ok()
            });
        if let Some(event) = record {
            let lost = event.sequence - expected;
            let capacity = pending.map_or(0, |(_, lines, bytes)| {
                lines.saturating_add((bytes / MIN_JOURNAL_RECORD_BYTES) as u64)
            });
            if lost <= capacity {
                if pending.take().is_some() {
                    interior_damage = true;
                }
                if lost > 0 {
                    entries.push(SalvagedEntry::Lost {
                        run_start: expected,
                        run_length: lost,
                    });
                }
                expected = event.sequence.saturating_add(1);
                entries.push(SalvagedEntry::Record { event, start, end });
                continue;
            }
        }
        let run = pending.get_or_insert((start, 0, 0));
        run.1 += 1;
        run.2 += end - start + 1;
    }
    SalvagedJournal {
        entries,
        interior_damage,
        corrupt_tail: pending.map(|(start, _, _)| start),
    }
}

/// Back up the original journal, then atomically rewrite it with every valid
/// record byte-for-byte and a [`JOURNAL_RECORD_LOST_EVENT`] placeholder for
/// each lost sequence. Corrupt lines after the last valid record are dropped
/// (the backup keeps them). Returns the new events and their offsets. Callers
/// hold the conversation lock.
async fn rewrite_salvaged_journal(
    conversation_id: &str,
    path: &Path,
    raw: &[u8],
    salvaged: SalvagedJournal,
) -> Result<(Vec<ConversationEvent>, Vec<(u64, u64)>), AppError> {
    use std::borrow::Cow;

    let too_large = || {
        AppError::InvalidRequest(format!(
            "conversation {conversation_id} journal is corrupt and its salvaged copy would \
             exceed {MAX_EVENTS_JOURNAL_BYTES} bytes"
        ))
    };
    let backup = corrupt_copy_path(path);
    let backup_name = backup
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_default();
    let mut events: Vec<ConversationEvent> = Vec::with_capacity(salvaged.entries.len());
    let mut lines: Vec<Cow<'_, [u8]>> = Vec::with_capacity(salvaged.entries.len());
    let mut total = 0u64;
    let mut lost_records = 0u64;
    for entry in salvaged.entries {
        match entry {
            SalvagedEntry::Record { event, start, end } => {
                let line = trim_ascii(&raw[start..end]);
                total += line.len() as u64 + 1;
                lines.push(Cow::Borrowed(line));
                events.push(event);
            }
            SalvagedEntry::Lost {
                run_start,
                run_length,
            } => {
                let previous = events.last();
                let time = previous.map_or_else(Utc::now, |event| event.time);
                let provider = previous.and_then(|event| event.provider);
                for sequence in run_start..run_start + run_length {
                    let mut placeholder = ConversationEvent::new(
                        conversation_id,
                        sequence,
                        JOURNAL_RECORD_LOST_EVENT,
                        serde_json::json!({
                            "reason": "corrupt",
                            "runStart": run_start,
                            "runLength": run_length,
                            "backup": backup_name,
                        }),
                    );
                    placeholder.time = time;
                    placeholder.provider = provider;
                    let line = serde_json::to_vec(&placeholder)?;
                    total += line.len() as u64 + 1;
                    // Checked per placeholder so a huge run fails before it
                    // is materialized.
                    if total > MAX_EVENTS_JOURNAL_BYTES {
                        return Err(too_large());
                    }
                    lines.push(Cow::Owned(line));
                    events.push(placeholder);
                }
                lost_records += run_length;
            }
        }
    }
    if total > MAX_EVENTS_JOURNAL_BYTES {
        return Err(too_large());
    }
    // The backup must be durable before the rewrite replaces the original.
    write_corrupt_copy(&backup, raw).await?;
    if let Some(directory) = path.parent() {
        sync_directory(directory).await?;
    }
    let offsets = replace_journal(path, &lines).await?;
    tracing::warn!(
        conversation_id,
        lost_records,
        backup = %backup_name,
        "salvaged corrupt conversation journal; lost records replaced with placeholders"
    );
    Ok((events, offsets))
}

/// Which end of a replay window the byte budget keeps.
#[derive(Clone, Copy)]
enum PageAnchor {
    /// Forward replay: keep the oldest records, drop newer ones.
    Start,
    /// Reverse replay: keep the newest records, drop older ones.
    End,
}

/// Shrink `from..to` from the end opposite `anchor` until its records span
/// at most `budget` journal bytes, keeping at least one record.
fn budget_window(
    offsets: &[(u64, u64)],
    (from, to): (usize, usize),
    anchor: PageAnchor,
    budget: u64,
) -> (usize, usize) {
    let to = to.min(offsets.len());
    let from = from.min(to);
    let size = |index: usize| offsets[index].1.saturating_sub(offsets[index].0);
    let mut bytes = 0u64;
    match anchor {
        PageAnchor::Start => {
            for index in from..to {
                bytes = bytes.saturating_add(size(index));
                if bytes > budget && index > from {
                    return (from, index);
                }
            }
        }
        PageAnchor::End => {
            for index in (from..to).rev() {
                bytes = bytes.saturating_add(size(index));
                if bytes > budget && index + 1 < to {
                    return (index + 1, to);
                }
            }
        }
    }
    (from, to)
}

/// Offset index produced by [`scan_journal_offsets`].
struct ScannedJournal {
    bytes: u64,
    modified: Option<std::time::SystemTime>,
    offsets: Vec<(u64, u64)>,
    last: ConversationEvent,
}

/// Blocking newline scan for a cold replay index; memory stays bounded by the
/// read buffer plus the first and last records. Sequence `N` must be line `N`,
/// so only the first and last records are parsed. Returns `None` whenever the
/// journal differs from what appends produce — empty, over the size limit,
/// lacking the final newline of an interrupted write, or with a first/last
/// record that fails to parse or validate (including a last sequence that
/// differs from the line count). Callers then run the full validating scan,
/// which owns tail repair and corruption reporting.
fn scan_journal_offsets(
    path: &Path,
    conversation_id: &str,
) -> Result<Option<ScannedJournal>, AppError> {
    use std::io::{BufRead, Read, Seek, SeekFrom};

    let mut file = std::fs::File::open(path)?;
    let metadata = file.metadata()?;
    let bytes = metadata.len();
    if bytes == 0 || bytes > MAX_EVENTS_JOURNAL_BYTES {
        return Ok(None);
    }
    let mut final_byte = [0u8; 1];
    file.seek(SeekFrom::End(-1))?;
    file.read_exact(&mut final_byte)?;
    if final_byte[0] != b'\n' {
        return Ok(None);
    }
    file.seek(SeekFrom::Start(0))?;
    let mut reader = std::io::BufReader::with_capacity(JOURNAL_SCAN_BUFFER_BYTES, file);
    let mut offsets = Vec::new();
    let mut cursor = 0u64;
    loop {
        let read = reader.skip_until(b'\n')? as u64;
        if read == 0 {
            break;
        }
        // `end` excludes the newline, matching `line_ranges`.
        offsets.push((cursor, cursor + read - 1));
        cursor += read;
    }
    if cursor != bytes {
        return Ok(None);
    }
    let mut file = reader.into_inner();
    let mut read_record =
        |(start, end): (u64, u64)| -> Result<Option<ConversationEvent>, AppError> {
            let mut line = vec![0u8; (end - start) as usize];
            file.seek(SeekFrom::Start(start))?;
            file.read_exact(&mut line)?;
            Ok(serde_json::from_slice(&line).ok())
        };
    let count = offsets.len() as u64;
    if count > 1 {
        let first_valid = read_record(offsets[0])?
            .is_some_and(|event| validate_event(&event, conversation_id, 1).is_ok());
        if !first_valid {
            return Ok(None);
        }
    }
    let Some(last) = read_record(offsets[offsets.len() - 1])?
        .filter(|event| validate_event(event, conversation_id, count).is_ok())
    else {
        return Ok(None);
    };
    Ok(Some(ScannedJournal {
        bytes,
        modified: metadata.modified().ok(),
        offsets,
        last,
    }))
}

fn line_ranges(raw: &[u8]) -> Vec<(usize, usize)> {
    let mut ranges = Vec::new();
    let mut start = 0;
    for (index, byte) in raw.iter().enumerate() {
        if *byte == b'\n' {
            ranges.push((start, index));
            start = index + 1;
        }
    }
    if start < raw.len() {
        ranges.push((start, raw.len()));
    }
    ranges
}

fn trim_ascii(mut value: &[u8]) -> &[u8] {
    while value.first().is_some_and(u8::is_ascii_whitespace) {
        value = &value[1..];
    }
    while value.last().is_some_and(u8::is_ascii_whitespace) {
        value = &value[..value.len() - 1];
    }
    value
}

async fn sync_directory(path: &Path) -> Result<(), AppError> {
    #[cfg(unix)]
    {
        tokio::fs::File::open(path).await?.sync_all().await?;
    }
    #[cfg(not(unix))]
    let _ = path;
    Ok(())
}

async fn set_owner_only(path: &Path, directory: bool) -> Result<(), AppError> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = if directory { 0o700 } else { 0o600 };
        tokio::fs::set_permissions(path, std::fs::Permissions::from_mode(mode)).await?;
    }
    #[cfg(not(unix))]
    let _ = (path, directory);
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::time::Duration;

    use serde_json::json;
    use tokio::io::AsyncWriteExt;

    use super::*;
    use crate::conversation::{ConversationStatus, ProviderKind};

    #[tokio::test]
    #[ignore = "opt-in 1k/10k journal replay performance measurement"]
    async fn measure_journal_replay_1k_and_10k() {
        for count in [1_000u64, 10_000] {
            let root = temp_dir("todex-replay-measurement");
            let store = ConversationStore::new(root.clone()).await.unwrap();
            let manifest = ConversationManifest::new(ProviderKind::Codex, root.clone(), None, None);
            let history = (1..=count)
                .map(|sequence| {
                    ConversationEvent::new(
                        &manifest.id,
                        sequence,
                        "message.delta",
                        json!({ "turnId": "t", "content": "representative delta ".repeat(24) }),
                    )
                })
                .collect();
            store
                .create_with_history(manifest.clone(), history, None, None)
                .await
                .unwrap();
            // The previous replay algorithm reparsed the complete journal for every page.
            let baseline_start = std::time::Instant::now();
            let mut baseline_first_page = std::time::Duration::ZERO;
            let mut restored = 0;
            for cursor in (0..count as usize).step_by(200) {
                restored += store
                    .complete_history(&manifest.id)
                    .await
                    .unwrap()
                    .into_iter()
                    .skip(cursor)
                    .take(200)
                    .count();
                if cursor == 0 {
                    baseline_first_page = baseline_start.elapsed();
                }
            }
            assert_eq!(restored, count as usize);
            let baseline = baseline_start.elapsed();
            store.indexes.clear();
            let cold_start = std::time::Instant::now();
            let mut cold_first_page = std::time::Duration::ZERO;
            let mut cursor = 0;
            loop {
                let page = store.replay(&manifest.id, cursor, 200).await.unwrap();
                if cursor == 0 {
                    cold_first_page = cold_start.elapsed();
                }
                cursor = page.next_sequence;
                if !page.has_more {
                    break;
                }
            }
            assert_eq!(cursor, count);
            let cold = cold_start.elapsed();
            let warm_start = std::time::Instant::now();
            let mut cursor = 0;
            loop {
                let page = store.replay(&manifest.id, cursor, 200).await.unwrap();
                cursor = page.next_sequence;
                if !page.has_more {
                    break;
                }
            }
            assert_eq!(cursor, count);
            let warm = warm_start.elapsed();
            let offset_capacity_bytes = {
                let index = store.indexes.get(&manifest.id).unwrap();
                index.offsets.capacity() * std::mem::size_of::<(u64, u64)>()
            };
            // Measure durable append queueing while the same journal is repeatedly replayed.
            let done = Arc::new(std::sync::atomic::AtomicBool::new(false));
            let reader_done = done.clone();
            let reader_store = store.clone();
            let reader_id = manifest.id.clone();
            let reader = tokio::spawn(async move {
                let mut cursor = 0;
                loop {
                    let page = reader_store.replay(&reader_id, cursor, 200).await.unwrap();
                    cursor = page.next_sequence;
                    if cursor >= count {
                        if reader_done.load(std::sync::atomic::Ordering::SeqCst) {
                            break;
                        }
                        cursor = 0;
                    }
                }
            });
            let mut append_ms = Vec::new();
            for index in 0..20 {
                let start = std::time::Instant::now();
                store
                    .append(&manifest.id, "fixture.append", json!({ "index": index }))
                    .await
                    .unwrap();
                append_ms.push(start.elapsed().as_secs_f64() * 1000.);
            }
            done.store(true, std::sync::atomic::Ordering::SeqCst);
            reader.await.unwrap();
            append_ms.sort_by(f64::total_cmp);
            eprintln!("replay_measurement events={count} page=200 baseline_full_scan_ms={:.2} indexed_cold_ms={:.2} indexed_warm_ms={:.2} baseline_first_page_ms={:.2} indexed_first_page_ms={:.2} concurrent_append_samples=20 append_p50_ms={:.2} append_p95_ms={:.2} append_max_ms={:.2} index_offset_capacity_bytes={offset_capacity_bytes}", baseline.as_secs_f64()*1000., cold.as_secs_f64()*1000., warm.as_secs_f64()*1000., baseline_first_page.as_secs_f64()*1000., cold_first_page.as_secs_f64()*1000., append_ms[9], append_ms[18], append_ms[19]);
            fs::remove_dir_all(root).unwrap();
        }
    }

    /// Cold first read after a daemon restart: a fresh store has no offset
    /// index, so the first tail page pays for building it. Run with
    /// `cargo test --release --locked -- --ignored measure_cold_first_page --nocapture`.
    #[tokio::test]
    #[ignore = "opt-in 10k/100k cold first-page latency measurement"]
    async fn measure_cold_first_page_10k_and_100k() {
        for count in [10_000u64, 100_000] {
            let root = temp_dir("todex-cold-first-page");
            let store = ConversationStore::new(root.clone()).await.unwrap();
            let manifest = ConversationManifest::new(ProviderKind::Codex, root.clone(), None, None);
            let history = (1..=count)
                .map(|sequence| {
                    ConversationEvent::new(
                        &manifest.id,
                        sequence,
                        "message.delta",
                        json!({ "turnId": "t", "content": "representative delta ".repeat(12) }),
                    )
                })
                .collect();
            store
                .create_with_history(manifest.clone(), history, None, None)
                .await
                .unwrap();
            let journal_bytes = fs::metadata(
                root.join("conversations")
                    .join(&manifest.id)
                    .join(EVENTS_FILE),
            )
            .unwrap()
            .len();
            let mut tail_ms = Vec::new();
            let mut head_ms = Vec::new();
            for _ in 0..5 {
                // A new store instance has no cached index, like a restarted daemon.
                let cold = ConversationStore::new(root.clone()).await.unwrap();
                let start = std::time::Instant::now();
                let page = cold
                    .replay_before(&manifest.id, u64::MAX, 50)
                    .await
                    .unwrap();
                tail_ms.push(start.elapsed().as_secs_f64() * 1000.);
                assert_eq!(page.events.last().unwrap().sequence, count);
                let cold = ConversationStore::new(root.clone()).await.unwrap();
                let start = std::time::Instant::now();
                let page = cold.replay(&manifest.id, 0, 200).await.unwrap();
                head_ms.push(start.elapsed().as_secs_f64() * 1000.);
                assert_eq!(page.events.len(), 200);
            }
            tail_ms.sort_by(f64::total_cmp);
            head_ms.sort_by(f64::total_cmp);
            // Daemon startup runs full recovery per conversation before
            // readiness, which leaves a validated index behind.
            let recovered = ConversationStore::new(root.clone()).await.unwrap();
            let start = std::time::Instant::now();
            recovered.recover_with_history(&manifest.id).await.unwrap();
            let recovery_ms = start.elapsed().as_secs_f64() * 1000.;
            let start = std::time::Instant::now();
            recovered
                .replay_before(&manifest.id, u64::MAX, 50)
                .await
                .unwrap();
            let after_recovery_tail_ms = start.elapsed().as_secs_f64() * 1000.;
            eprintln!(
                "cold_first_page events={count} journal_bytes={journal_bytes} runs=5 tail_page50_median_ms={:.2} tail_page50_min_ms={:.2} head_page200_median_ms={:.2} head_page200_min_ms={:.2} startup_recovery_ms={recovery_ms:.2} tail_page50_after_recovery_ms={after_recovery_tail_ms:.2}",
                tail_ms[2], tail_ms[0], head_ms[2], head_ms[0]
            );
            fs::remove_dir_all(root).unwrap();
        }
    }

    /// Per-append durable latency on a quiet conversation. Run with
    /// `cargo test --release --locked -- --ignored measure_append_latency --nocapture`.
    #[tokio::test]
    #[ignore = "opt-in append latency measurement"]
    async fn measure_append_latency_200() {
        let root = temp_dir("todex-append-latency");
        let store = ConversationStore::new(root.clone()).await.unwrap();
        let conversation = store
            .create(ConversationManifest::new(
                ProviderKind::Codex,
                root.clone(),
                None,
                None,
            ))
            .await
            .unwrap();
        store
            .append(&conversation.id, "turn.started", json!({ "turnId": "t" }))
            .await
            .unwrap();
        let mut append_ms = Vec::with_capacity(200);
        for index in 0..200 {
            let start = std::time::Instant::now();
            store
                .append(
                    &conversation.id,
                    "message.delta",
                    json!({ "turnId": "t", "index": index, "delta": "representative delta ".repeat(8) }),
                )
                .await
                .unwrap();
            append_ms.push(start.elapsed().as_secs_f64() * 1000.);
        }
        append_ms.sort_by(f64::total_cmp);
        eprintln!(
            "append_latency samples=200 p50_ms={:.2} p95_ms={:.2} max_ms={:.2}",
            append_ms[99], append_ms[189], append_ms[199]
        );
        fs::remove_dir_all(root).unwrap();
    }

    #[tokio::test]
    async fn live_and_recovered_status_preserve_approval_and_interruption_semantics() {
        let root = temp_dir("todex-status-contract");
        let store = ConversationStore::new(root.clone()).await.unwrap();
        let conversation = store
            .create(ConversationManifest::new(
                ProviderKind::Codex,
                root.clone(),
                None,
                None,
            ))
            .await
            .unwrap();
        store
            .append(
                &conversation.id,
                "codex.turn.started",
                json!({ "turnId": "t" }),
            )
            .await
            .unwrap();
        store
            .append(
                &conversation.id,
                "permission.requested",
                json!({ "permissionId": "p" }),
            )
            .await
            .unwrap();
        assert_eq!(
            store.get(&conversation.id).await.unwrap().status,
            ConversationStatus::WaitingPermission
        );
        store
            .append(
                &conversation.id,
                "permission.resolved",
                json!({ "permissionId": "p" }),
            )
            .await
            .unwrap();
        store
            .append(
                &conversation.id,
                "message.completed",
                json!({ "role": "assistant" }),
            )
            .await
            .unwrap();
        assert_eq!(
            store.get(&conversation.id).await.unwrap().status,
            ConversationStatus::Running
        );
        for activity in [
            "compaction.started",
            "compaction.completed",
            "compaction.failed",
        ] {
            store
                .append(
                    &conversation.id,
                    activity,
                    json!({ "turnId": "t", "source": "provider" }),
                )
                .await
                .unwrap();
            assert_eq!(
                store.get(&conversation.id).await.unwrap().status,
                ConversationStatus::Running
            );
        }
        store
            .append(
                &conversation.id,
                "conversation.interrupted",
                json!({ "reason": "daemon_restarted" }),
            )
            .await
            .unwrap();
        assert_eq!(
            store.get(&conversation.id).await.unwrap().status,
            ConversationStatus::Interrupted
        );
        assert_eq!(
            store.recover(&conversation.id).await.unwrap().status,
            ConversationStatus::Interrupted
        );
        // Repair a journal record created by the older lossy canonical mapping.
        let path = root
            .join("conversations")
            .join(&conversation.id)
            .join(EVENTS_FILE);
        let original = fs::read_to_string(&path).unwrap();
        fs::write(
            &path,
            original.replace(
                "\"normalizedType\":\"conversation.interrupted\"",
                "\"normalizedType\":\"turn.cancelled\"",
            ),
        )
        .unwrap();
        assert_eq!(
            store.recover(&conversation.id).await.unwrap().status,
            ConversationStatus::Interrupted
        );
        fs::remove_dir_all(root).unwrap();
    }

    #[tokio::test]
    async fn recovery_returns_validated_history_and_repaired_manifest() {
        let root = temp_dir("todex-recovery-history");
        let store = ConversationStore::new(root.clone()).await.unwrap();
        let manifest = store
            .create(ConversationManifest::new(
                ProviderKind::Codex,
                root.clone(),
                None,
                None,
            ))
            .await
            .unwrap();
        store
            .append(&manifest.id, "codex.turn.started", json!({ "turnId": "t" }))
            .await
            .unwrap();
        store
            .append(
                &manifest.id,
                "permission.requested",
                json!({ "permissionId": "p" }),
            )
            .await
            .unwrap();
        let (recovered, history) = store.recover_with_history(&manifest.id).await.unwrap();
        assert_eq!(history.len(), 2);
        assert_eq!(history[0].event_type, "codex.turn.started");
        assert_eq!(history[1].sequence, 2);
        assert_eq!(recovered.last_sequence, 2);
        assert_eq!(recovered.status, ConversationStatus::Interrupted);
        assert_eq!(
            store.get(&manifest.id).await.unwrap().status,
            ConversationStatus::Interrupted
        );
        assert_eq!(store.recover(&manifest.id).await.unwrap().last_sequence, 2);
        fs::remove_dir_all(root).unwrap();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn concurrent_durable_publish_is_contiguous() {
        let root = temp_dir("todex-publish-order");
        let store = ConversationStore::new(root.clone()).await.unwrap();
        let conversation = store
            .create(ConversationManifest::new(
                ProviderKind::Codex,
                root.clone(),
                None,
                None,
            ))
            .await
            .unwrap();
        let hub = ConversationEventHub::default();
        let mut receiver = hub.subscribe(&conversation.id);
        let barrier = Arc::new(tokio::sync::Barrier::new(24));
        let mut tasks = Vec::new();
        for _ in 0..24 {
            let (store, hub, id, barrier) = (
                store.clone(),
                hub.clone(),
                conversation.id.clone(),
                barrier.clone(),
            );
            tasks.push(tokio::spawn(async move {
                barrier.wait().await;
                store
                    .append_and_publish(&id, "fixture.delta", json!({}), &hub)
                    .await
                    .unwrap();
            }));
        }
        for task in tasks {
            task.await.unwrap();
        }
        for expected in 1..=24 {
            let received = receiver.recv().await.unwrap();
            assert_eq!(received.sequence, expected);
            assert!(store.get(&conversation.id).await.unwrap().last_sequence >= received.sequence);
        }
        fs::remove_dir_all(root).unwrap();
    }

    #[tokio::test]
    async fn seeded_history_and_indexed_pages_are_complete_and_rebuildable() {
        let root = temp_dir("todex-indexed-history");
        let store = ConversationStore::new(root.clone()).await.unwrap();
        let conversation = ConversationManifest::new(ProviderKind::Codex, root.clone(), None, None);
        let events = (1..=1201).map(|sequence| ConversationEvent::new(&conversation.id, sequence,
            "message.created", json!({ "role": "user", "content": format!("message-{sequence}"), "turnId": format!("t-{sequence}") }))).collect();
        let mut native = ProviderState::new(ProviderKind::Codex);
        native.native_session_id = Some("native-fork".to_owned());
        let snapshot = json!({ "turnId": "t-1201", "request": { "text": "last" } });
        store
            .create_with_history(
                conversation.clone(),
                events,
                Some(native),
                Some(snapshot.clone()),
            )
            .await
            .unwrap();
        let first = store.replay(&conversation.id, 0, usize::MAX).await.unwrap();
        assert_eq!(first.events.len(), 1000);
        assert!(first.has_more);
        let last = store
            .replay(&conversation.id, first.next_sequence, 1000)
            .await
            .unwrap();
        assert_eq!(last.events.len(), 201);
        assert!(!last.has_more);
        assert_eq!(
            store
                .last_user_message(&conversation.id)
                .await
                .unwrap()
                .unwrap()
                .sequence,
            1201
        );
        assert_eq!(
            store
                .complete_history(&conversation.id)
                .await
                .unwrap()
                .len(),
            1201
        );
        assert_eq!(
            store.last_request(&conversation.id).await.unwrap(),
            Some(snapshot)
        );
        assert_eq!(
            store
                .provider_state(&conversation.id)
                .await
                .unwrap()
                .native_session_id
                .as_deref(),
            Some("native-fork")
        );
        store
            .append(&conversation.id, "turn.completed", json!({}))
            .await
            .unwrap();
        assert_eq!(
            store
                .replay(&conversation.id, 1201, 1000)
                .await
                .unwrap()
                .events[0]
                .sequence,
            1202
        );
        assert_eq!(
            store.indexes.get(&conversation.id).unwrap().offsets.len(),
            1202
        );
        store.indexes.clear();
        assert_eq!(
            store
                .replay(&conversation.id, 1199, 1000)
                .await
                .unwrap()
                .events
                .len(),
            3
        );
        fs::remove_dir_all(root).unwrap();
    }

    #[tokio::test]
    async fn replay_before_pages_backward_from_an_inclusive_cursor() {
        let root = temp_dir("todex-replay-before");
        let store = ConversationStore::new(root.clone()).await.unwrap();
        let conversation = ConversationManifest::new(ProviderKind::Codex, root.clone(), None, None);
        let events = (1..=50)
            .map(|sequence| {
                ConversationEvent::new(
                    &conversation.id,
                    sequence,
                    "message.created",
                    json!({ "role": "user", "content": format!("message-{sequence}") }),
                )
            })
            .collect();
        store
            .create_with_history(conversation.clone(), events, None, None)
            .await
            .unwrap();

        // A full tail page ends exactly at the inclusive cursor.
        let tail = store.replay_before(&conversation.id, 50, 20).await.unwrap();
        assert_eq!(tail.events.len(), 20);
        assert_eq!(tail.events[0].sequence, 31);
        assert_eq!(tail.events[19].sequence, 50);
        assert_eq!(tail.next_sequence, 50);
        assert!(tail.has_more);

        // The next page continues with no overlap or gap.
        let middle = store
            .replay_before(&conversation.id, tail.events[0].sequence - 1, 20)
            .await
            .unwrap();
        assert_eq!(middle.events.len(), 20);
        assert_eq!(middle.events[0].sequence, 11);
        assert_eq!(middle.events[19].sequence, 30);
        assert!(middle.has_more);

        // A short first page reports exhaustion.
        let head = store.replay_before(&conversation.id, 10, 20).await.unwrap();
        assert_eq!(head.events.len(), 10);
        assert_eq!(head.events[0].sequence, 1);
        assert!(!head.has_more);

        // Cursors past the end clamp to the journal; empty ranges stay empty.
        let clamped = store
            .replay_before(&conversation.id, 10_000, 5)
            .await
            .unwrap();
        assert_eq!(clamped.events[0].sequence, 46);
        let empty = store.replay_before(&conversation.id, 0, 5).await.unwrap();
        assert!(empty.events.is_empty());
        assert!(!empty.has_more);
        fs::remove_dir_all(root).unwrap();
    }

    #[tokio::test]
    async fn replay_pages_stop_at_the_byte_budget_in_both_directions() {
        let root = temp_dir("todex-replay-byte-budget");
        let store = ConversationStore::new(root.clone()).await.unwrap();
        let conversation = ConversationManifest::new(ProviderKind::Codex, root.clone(), None, None);
        let bulk = "x".repeat(1000 * 1024);
        let events = (1..=20)
            .map(|sequence| {
                ConversationEvent::new(
                    &conversation.id,
                    sequence,
                    "tool.completed",
                    json!({ "output": bulk, "index": sequence }),
                )
            })
            .collect();
        store
            .create_with_history(conversation.clone(), events, None, None)
            .await
            .unwrap();
        let id = conversation.id;
        let page_bytes = |replay: &ConversationReplay| {
            replay
                .events
                .iter()
                .map(|event| serde_json::to_vec(event).unwrap().len() as u64)
                .sum::<u64>()
        };

        let mut forward = Vec::new();
        let mut cursor = 0;
        let mut pages = 0;
        loop {
            let page = store.replay(&id, cursor, 1000).await.unwrap();
            pages += 1;
            assert!(!page.events.is_empty());
            assert!(page_bytes(&page) <= MAX_REPLAY_PAGE_BYTES);
            assert_eq!(page.from_sequence, cursor);
            assert_eq!(page.next_sequence, page.events.last().unwrap().sequence);
            forward.extend(sequences(&page));
            cursor = page.next_sequence;
            if !page.has_more {
                break;
            }
        }
        assert_eq!(forward, (1..=20).collect::<Vec<_>>());
        assert_eq!(pages, 3, "8 + 8 + 4 records of ~1 MiB");

        let mut backward = Vec::new();
        let mut before = u64::MAX;
        let mut pages = 0;
        loop {
            let page = store.replay_before(&id, before, 1000).await.unwrap();
            pages += 1;
            assert!(!page.events.is_empty());
            assert!(page_bytes(&page) <= MAX_REPLAY_PAGE_BYTES);
            let first = page.events[0].sequence;
            assert_eq!(page.from_sequence, first - 1);
            assert_eq!(page.has_more, first > 1);
            assert_eq!(page.next_sequence, page.events.last().unwrap().sequence);
            let mut chunk = sequences(&page);
            chunk.extend(backward);
            backward = chunk;
            if !page.has_more {
                break;
            }
            before = page.from_sequence;
        }
        assert_eq!(backward, (1..=20).collect::<Vec<_>>());
        assert_eq!(pages, 3);

        // The record limit still applies below the byte budget.
        let small = store.replay(&id, 4, 2).await.unwrap();
        assert_eq!(sequences(&small), vec![5, 6]);
        assert!(small.has_more);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn budget_window_keeps_one_record_even_above_the_budget() {
        let offsets = vec![(0, 100), (101, 301), (302, 352), (353, 363)];
        assert_eq!(
            budget_window(&offsets, (0, 4), PageAnchor::Start, 10),
            (0, 1)
        );
        assert_eq!(budget_window(&offsets, (0, 4), PageAnchor::End, 5), (3, 4));
        assert_eq!(
            budget_window(&offsets, (0, 4), PageAnchor::Start, 300),
            (0, 2)
        );
        assert_eq!(
            budget_window(&offsets, (0, 4), PageAnchor::End, 260),
            (1, 4)
        );
        assert_eq!(
            budget_window(&offsets, (1, 3), PageAnchor::Start, 1000),
            (1, 3)
        );
        assert_eq!(budget_window(&offsets, (4, 9), PageAnchor::End, 1), (4, 4));
    }

    /// Seeds `count` events; the returned store has no replay index yet.
    async fn seed_journal(prefix: &str, count: u64) -> (PathBuf, String, PathBuf) {
        let root = temp_dir(prefix);
        let store = ConversationStore::new(root.clone()).await.unwrap();
        let conversation = ConversationManifest::new(ProviderKind::Codex, root.clone(), None, None);
        let events = (1..=count)
            .map(|sequence| {
                ConversationEvent::new(
                    &conversation.id,
                    sequence,
                    "message.created",
                    json!({ "role": "user", "content": format!("message-{sequence}") }),
                )
            })
            .collect();
        store
            .create_with_history(conversation.clone(), events, None, None)
            .await
            .unwrap();
        let path = root
            .join("conversations")
            .join(&conversation.id)
            .join(EVENTS_FILE);
        (root, conversation.id, path)
    }

    fn sequences(replay: &ConversationReplay) -> Vec<u64> {
        replay.events.iter().map(|event| event.sequence).collect()
    }

    #[tokio::test]
    async fn cold_index_on_a_clean_journal_skips_full_validation() {
        let (root, id, _) = seed_journal("todex-cold-index-fast", 120).await;
        // A fresh store, like a restarted daemon, starts without an index.
        let store = ConversationStore::new(root.clone()).await.unwrap();

        let tail = store.replay_before(&id, u64::MAX, 20).await.unwrap();
        assert_eq!(sequences(&tail), (101..=120).collect::<Vec<_>>());
        assert!(tail.has_more);
        {
            let index = store.indexes.get(&id).unwrap();
            assert!(
                !index.fully_validated,
                "clean journal must use the fast scan"
            );
            assert_eq!(index.offsets.len(), 120);
        }
        assert_eq!(store.tails.get(&id).unwrap().event.sequence, 120);

        let before = store.replay_before(&id, 50, 20).await.unwrap();
        assert_eq!(sequences(&before), (31..=50).collect::<Vec<_>>());
        assert!(before.has_more);
        let head = store.replay(&id, 0, 10).await.unwrap();
        assert_eq!(sequences(&head), (1..=10).collect::<Vec<_>>());
        assert!(head.has_more);
        assert_eq!(head.events[4].payload["content"], "message-5");

        // Appends extend the fast index in place instead of rebuilding it.
        let appended = store
            .append(&id, "turn.completed", json!({}))
            .await
            .unwrap();
        assert_eq!(appended.sequence, 121);
        {
            let index = store.indexes.get(&id).unwrap();
            assert!(!index.fully_validated);
            assert_eq!(index.offsets.len(), 121);
        }
        let newest = store.replay_before(&id, u64::MAX, 2).await.unwrap();
        assert_eq!(sequences(&newest), vec![120, 121]);
        fs::remove_dir_all(root).unwrap();
    }

    #[tokio::test]
    async fn cold_index_falls_back_to_full_recovery_for_a_torn_tail() {
        let (root, id, path) = seed_journal("todex-cold-index-torn", 30).await;
        let clean = fs::read(&path).unwrap();
        let mut torn = clean.clone();
        torn.extend_from_slice(b"{\"schemaVersion\":2,\"sequence\":31");
        fs::write(&path, &torn).unwrap();
        let store = ConversationStore::new(root.clone()).await.unwrap();

        let tail = store.replay_before(&id, u64::MAX, 10).await.unwrap();
        assert_eq!(sequences(&tail), (21..=30).collect::<Vec<_>>());
        assert!(store.indexes.get(&id).unwrap().fully_validated);
        // The torn record is quarantined and the journal cut back to its last
        // complete record, as the full scan always did.
        assert_eq!(fs::read(&path).unwrap(), clean);
        let quarantined = fs::read_dir(path.parent().unwrap())
            .unwrap()
            .filter_map(|entry| entry.ok())
            .any(|entry| {
                entry
                    .file_name()
                    .to_string_lossy()
                    .starts_with("events.corrupt.")
            });
        assert!(quarantined);
        let appended = store
            .append(&id, "turn.completed", json!({}))
            .await
            .unwrap();
        assert_eq!(appended.sequence, 31);
        fs::remove_dir_all(root).unwrap();
    }

    #[tokio::test]
    async fn cold_index_falls_back_when_sequence_disagrees_with_line_count() {
        let (root, id, path) = seed_journal("todex-cold-index-mismatch", 10).await;
        let clean = fs::read_to_string(&path).unwrap();
        let mut raw = clean.clone();
        let last_line = raw.lines().last().unwrap().to_owned();
        raw.push_str(&last_line);
        raw.push('\n');
        fs::write(&path, &raw).unwrap();
        let store = ConversationStore::new(root.clone()).await.unwrap();

        // A repeated final record does not continue the journal and nothing
        // valid follows it, so the full scan quarantines it as a tail.
        let tail = store.replay_before(&id, u64::MAX, 5).await.unwrap();
        assert_eq!(sequences(&tail), (6..=10).collect::<Vec<_>>());
        assert!(store.indexes.get(&id).unwrap().fully_validated);
        assert_eq!(fs::read_to_string(&path).unwrap(), clean);
        assert_eq!(corrupt_copies(&path).len(), 1);
        fs::remove_dir_all(root).unwrap();
    }

    #[tokio::test]
    async fn fast_index_salvages_interior_corruption_through_the_full_scan() {
        let (root, id, path) = seed_journal("todex-cold-index-interior", 10).await;
        let raw = fs::read_to_string(&path).unwrap();
        let mut lines = raw.lines().map(str::to_owned).collect::<Vec<_>>();
        lines[2] = "{".to_owned();
        let corrupt = format!("{}\n", lines.join("\n"));
        fs::write(&path, &corrupt).unwrap();
        let store = ConversationStore::new(root.clone()).await.unwrap();

        // First and last records are intact, so the cold scan accepts the
        // journal; pages that avoid the damaged record stay readable and
        // leave the file alone.
        let tail = store.replay_before(&id, u64::MAX, 5).await.unwrap();
        assert_eq!(sequences(&tail), (6..=10).collect::<Vec<_>>());
        assert_eq!(fs::read_to_string(&path).unwrap(), corrupt);
        // The page that reaches it runs the full scan, which salvages it.
        let page = store.replay(&id, 0, 10).await.unwrap();
        assert_eq!(sequences(&page), (1..=10).collect::<Vec<_>>());
        assert_eq!(page.events[2].event_type, JOURNAL_RECORD_LOST_EVENT);
        assert!(store.indexes.get(&id).unwrap().fully_validated);
        fs::remove_dir_all(root).unwrap();
    }

    fn strip_final_newline(path: &Path) {
        let mut raw = fs::read(path).unwrap();
        assert_eq!(raw.pop(), Some(b'\n'));
        fs::write(path, raw).unwrap();
    }

    fn quarantined(path: &Path) -> bool {
        fs::read_dir(path.parent().unwrap())
            .unwrap()
            .filter_map(|entry| entry.ok())
            .any(|entry| {
                entry
                    .file_name()
                    .to_string_lossy()
                    .starts_with("events.corrupt.")
            })
    }

    #[tokio::test]
    async fn append_after_restart_does_not_glue_onto_an_unterminated_record() {
        let (root, id, path) = seed_journal("todex-unterminated-cold", 2).await;
        strip_final_newline(&path);

        // Restarted daemon: no caches, the tail is read from disk.
        let restarted = ConversationStore::new(root.clone()).await.unwrap();
        let appended = restarted
            .append(&id, "turn.completed", json!({}))
            .await
            .unwrap();
        assert_eq!(appended.sequence, 3);
        assert_eq!(fs::read_to_string(&path).unwrap().lines().count(), 3);

        let fresh = ConversationStore::new(root.clone()).await.unwrap();
        let history = fresh.complete_history(&id).await.unwrap();
        assert_eq!(
            history
                .iter()
                .map(|event| event.sequence)
                .collect::<Vec<_>>(),
            vec![1, 2, 3]
        );
        let cold = ConversationStore::new(root.clone()).await.unwrap();
        let page = cold.replay(&id, 0, 10).await.unwrap();
        assert_eq!(sequences(&page), vec![1, 2, 3]);
        assert!(!quarantined(&path));
        fs::remove_dir_all(root).unwrap();
    }

    #[tokio::test]
    async fn recovery_terminates_a_complete_record_missing_its_newline() {
        let (root, id, path) = seed_journal("todex-unterminated-recover", 2).await;
        strip_final_newline(&path);

        let restarted = ConversationStore::new(root.clone()).await.unwrap();
        let (manifest, history) = restarted.recover_with_history(&id).await.unwrap();
        assert_eq!(manifest.last_sequence, 2);
        assert_eq!(history.len(), 2);
        assert!(fs::read(&path).unwrap().ends_with(b"\n"));
        assert!(!quarantined(&path));

        restarted
            .append(&id, "turn.completed", json!({}))
            .await
            .unwrap();
        let fresh = ConversationStore::new(root.clone()).await.unwrap();
        let page = fresh.replay(&id, 0, 10).await.unwrap();
        assert_eq!(sequences(&page), vec![1, 2, 3]);
        fs::remove_dir_all(root).unwrap();
    }

    #[tokio::test]
    async fn warm_append_does_not_glue_onto_an_unterminated_record() {
        let root = temp_dir("todex-unterminated-warm");
        let store = ConversationStore::new(root.clone()).await.unwrap();
        let conversation = store
            .create(ConversationManifest::new(
                ProviderKind::Codex,
                root.clone(),
                None,
                None,
            ))
            .await
            .unwrap();
        for index in 0..2 {
            store
                .append(
                    &conversation.id,
                    "provider.event",
                    json!({ "index": index }),
                )
                .await
                .unwrap();
        }
        // Warm the replay index before the journal loses its newline.
        store.replay(&conversation.id, 0, 10).await.unwrap();
        let path = root
            .join("conversations")
            .join(&conversation.id)
            .join(EVENTS_FILE);
        strip_final_newline(&path);

        let appended = store
            .append(&conversation.id, "provider.event", json!({ "index": 2 }))
            .await
            .unwrap();
        assert_eq!(appended.sequence, 3);
        let page = store.replay(&conversation.id, 0, 10).await.unwrap();
        assert_eq!(sequences(&page), vec![1, 2, 3]);
        let fresh = ConversationStore::new(root.clone()).await.unwrap();
        let history = fresh.complete_history(&conversation.id).await.unwrap();
        assert_eq!(history.len(), 3);
        assert_eq!(history[2].payload["index"], 2);
        assert!(!quarantined(&path));
        fs::remove_dir_all(root).unwrap();
    }

    #[tokio::test]
    async fn oversized_payload_is_truncated_instead_of_rejected() {
        let root = temp_dir("todex-bounded-payload");
        let store = ConversationStore::new(root.clone()).await.unwrap();
        let conversation = store
            .create(ConversationManifest::new(
                ProviderKind::Codex,
                root.clone(),
                None,
                None,
            ))
            .await
            .unwrap();
        let output = "x".repeat(2 * 1024 * 1024);
        let appended = store
            .append(
                &conversation.id,
                "tool.completed",
                json!({ "turnId": "t", "itemId": "i", "output": output }),
            )
            .await
            .unwrap();
        assert_eq!(appended.sequence, 1);

        let fresh = ConversationStore::new(root.clone()).await.unwrap();
        let stored = &fresh.complete_history(&conversation.id).await.unwrap()[0].payload;
        assert!(serde_json::to_vec(stored).unwrap().len() <= MAX_EVENT_PAYLOAD_BYTES);
        assert_eq!(stored["turnId"], "t");
        assert_eq!(stored["itemId"], "i");
        let kept = stored["output"].as_str().unwrap();
        assert!(kept.starts_with("xxxx"));
        let kept_bytes = kept.find('…').unwrap();
        assert!(kept.ends_with(&format!("…[truncated {} bytes]", output.len() - kept_bytes)));
        assert_eq!(stored["truncated"]["/output"], output.len());
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn bounded_payload_cuts_multibyte_and_escaped_text_on_char_boundaries() {
        let text = "中文🙂\"\n".repeat(200_000);
        let mut payload = json!({ "turnId": "t", "text": text, "short": "kept" });
        let bounded = bound_event_payload(&mut payload).unwrap();
        let serialized = serde_json::to_vec(&payload).unwrap();
        assert_eq!(bounded.bytes, serialized.len());
        assert!(bounded.bytes <= EVENT_PAYLOAD_BUDGET_BYTES);
        assert!(bounded.original_bytes.unwrap() > MAX_EVENT_PAYLOAD_BYTES);
        let kept = payload["text"].as_str().unwrap();
        let prefix = &kept[..kept.find('…').unwrap()];
        assert!(text.starts_with(prefix));
        assert!(prefix.len() > 512 * 1024);
        assert_eq!(payload["short"], "kept");
        assert_eq!(payload["truncated"]["/text"], text.len());
    }

    #[test]
    fn bounded_payload_leaves_small_payloads_byte_identical() {
        let original = json!({
            "turnId": "t",
            "content": "x".repeat(900 * 1024),
            "nested": [{ "a/b~c": "value" }, 1, null, true],
        });
        let mut payload = original.clone();
        let bounded = bound_event_payload(&mut payload).unwrap();
        assert!(bounded.original_bytes.is_none());
        assert_eq!(
            serde_json::to_vec(&payload).unwrap(),
            serde_json::to_vec(&original).unwrap()
        );
        assert_eq!(bounded.bytes, serde_json::to_vec(&original).unwrap().len());
    }

    #[test]
    fn bounded_payload_records_under_an_alternate_key_and_escapes_pointers() {
        let mut payload = json!({
            "truncated": false,
            "a/b~c": ["y".repeat(700 * 1024), "z".repeat(700 * 1024)],
        });
        bound_event_payload(&mut payload).unwrap();
        assert!(serde_json::to_vec(&payload).unwrap().len() <= EVENT_PAYLOAD_BUDGET_BYTES);
        assert_eq!(payload["truncated"], false);
        let records = payload["_truncated"].as_object().unwrap();
        assert!(!records.is_empty());
        for (pointer, bytes) in records {
            assert!(pointer.starts_with("/a~1b~0c/"));
            assert_eq!(bytes, 700 * 1024);
            assert!(payload
                .pointer(pointer)
                .unwrap()
                .as_str()
                .unwrap()
                .contains("…[truncated "));
        }
    }

    #[test]
    fn bounded_payload_replaces_bulk_made_of_small_values() {
        let items: Vec<String> = (0..200_000)
            .map(|index| format!("item-{index:06}"))
            .collect();
        let mut payload = json!({ "turnId": "t", "items": items });
        let original = serde_json::to_vec(&payload).unwrap().len();
        let bounded = bound_event_payload(&mut payload).unwrap();
        assert_eq!(
            payload,
            json!({ "turnId": "t", "truncated": true, "originalBytes": original })
        );
        assert_eq!(bounded.original_bytes, Some(original));
    }

    fn manifest_path(root: &Path, id: &str) -> PathBuf {
        root.join("conversations").join(id).join(MANIFEST_FILE)
    }

    fn disk_manifest(root: &Path, id: &str) -> Value {
        serde_json::from_slice(&fs::read(manifest_path(root, id)).unwrap()).unwrap()
    }

    /// Rewrite manifest.json with `last_sequence`, as a crash between
    /// debounced flushes (or before a tail repair) leaves it.
    fn set_disk_last_sequence(root: &Path, id: &str, last_sequence: u64) {
        let mut manifest = disk_manifest(root, id);
        manifest["lastSequence"] = json!(last_sequence);
        fs::write(
            manifest_path(root, id),
            serde_json::to_vec_pretty(&manifest).unwrap(),
        )
        .unwrap();
    }

    #[tokio::test]
    async fn set_status_persists_without_a_journal_event() {
        let (root, id) = seed_turn("todex-set-status", 3).await;
        let store = ConversationStore::new(root.clone()).await.unwrap();
        let failed = store
            .set_status(&id, super::super::ConversationStatus::Failed)
            .await
            .unwrap();
        assert_eq!(failed.status, super::super::ConversationStatus::Failed);
        assert_eq!(failed.last_sequence, 3);
        assert_eq!(disk_manifest(&root, &id)["status"], json!("failed"));
        assert_eq!(store.replay(&id, 0, 100).await.unwrap().events.len(), 3);
        let _ = fs::remove_dir_all(root);
    }

    async fn seed_turn(prefix: &str, count: u64) -> (PathBuf, String) {
        let root = temp_dir(prefix);
        let store = ConversationStore::new(root.clone()).await.unwrap();
        let conversation = ConversationManifest::new(ProviderKind::Codex, root.clone(), None, None);
        let events = (1..=count)
            .map(|sequence| {
                let event_type = if sequence == 1 {
                    "turn.started"
                } else {
                    "message.delta"
                };
                ConversationEvent::new(
                    &conversation.id,
                    sequence,
                    event_type,
                    json!({ "turnId": "t", "delta": format!("d{sequence}") }),
                )
            })
            .collect();
        store
            .create_with_history(conversation.clone(), events, None, None)
            .await
            .unwrap();
        (root, conversation.id)
    }

    #[tokio::test]
    async fn recovery_follows_the_journal_when_the_manifest_lags_far_behind() {
        let (root, id) = seed_turn("todex-manifest-behind-recover", 150).await;
        set_disk_last_sequence(&root, &id, 50);

        let restarted = ConversationStore::new(root.clone()).await.unwrap();
        assert_eq!(restarted.list().await.unwrap()[0].last_sequence, 50);
        let (recovered, history) = restarted.recover_with_history(&id).await.unwrap();
        assert_eq!(history.len(), 150);
        assert_eq!(recovered.last_sequence, 150);
        assert_eq!(recovered.status, ConversationStatus::Interrupted);
        assert_eq!(disk_manifest(&root, &id)["lastSequence"], 150);
        let appended = restarted
            .append(&id, "turn.started", json!({ "turnId": "t2" }))
            .await
            .unwrap();
        assert_eq!(appended.sequence, 151);
        assert_eq!(
            restarted.get(&id).await.unwrap().status,
            ConversationStatus::Running
        );
        fs::remove_dir_all(root).unwrap();
    }

    #[tokio::test]
    async fn append_follows_the_journal_when_the_manifest_is_behind_or_ahead() {
        let (root, id) = seed_turn("todex-manifest-drift", 150).await;
        set_disk_last_sequence(&root, &id, 50);
        let restarted = ConversationStore::new(root.clone()).await.unwrap();
        let appended = restarted
            .append(&id, "message.delta", json!({ "turnId": "t" }))
            .await
            .unwrap();
        assert_eq!(appended.sequence, 151);
        let manifest = restarted.get(&id).await.unwrap();
        assert_eq!(manifest.last_sequence, 151);
        assert_eq!(manifest.status, ConversationStatus::Running);

        // Ahead: the journal lost its newest records.
        restarted.flush_pending_deltas().await;
        let path = root.join("conversations").join(&id).join(EVENTS_FILE);
        let raw = fs::read_to_string(&path).unwrap();
        let kept: String = raw.split_inclusive('\n').take(140).collect();
        fs::write(&path, kept).unwrap();
        let restarted = ConversationStore::new(root.clone()).await.unwrap();
        let appended = restarted
            .append(&id, "turn.completed", json!({ "turnId": "t" }))
            .await
            .unwrap();
        assert_eq!(appended.sequence, 141);
        assert_eq!(disk_manifest(&root, &id)["lastSequence"], 141);
        assert_eq!(disk_manifest(&root, &id)["status"], "idle");
        let page = restarted.replay(&id, 0, 1000).await.unwrap();
        assert_eq!(sequences(&page), (1..=141).collect::<Vec<_>>());
        fs::remove_dir_all(root).unwrap();
    }

    #[tokio::test]
    async fn status_changes_persist_immediately_and_other_appends_are_debounced() {
        let root = temp_dir("todex-manifest-debounce");
        let store = ConversationStore::new(root.clone()).await.unwrap();
        let conversation = store
            .create(ConversationManifest::new(
                ProviderKind::Codex,
                root.clone(),
                None,
                None,
            ))
            .await
            .unwrap();
        let id = conversation.id.as_str();
        let snapshot_path = root.join("conversations").join(id).join(SNAPSHOT_FILE);
        store
            .append(id, "turn.started", json!({ "turnId": "t" }))
            .await
            .unwrap();
        assert_eq!(disk_manifest(&root, id)["status"], "running");
        assert_eq!(disk_manifest(&root, id)["lastSequence"], 1);
        let snapshot: Value = serde_json::from_slice(&fs::read(&snapshot_path).unwrap()).unwrap();
        assert_eq!(snapshot["status"], "running");

        for index in 0..5 {
            store
                .append(
                    id,
                    "message.delta",
                    json!({ "turnId": "t", "index": index }),
                )
                .await
                .unwrap();
        }
        // Readers see the cache; manifest.json waits for the debounce timer.
        assert_eq!(store.get(id).await.unwrap().last_sequence, 6);
        assert_eq!(disk_manifest(&root, id)["lastSequence"], 1);
        tokio::time::sleep(MANIFEST_FLUSH_INTERVAL + Duration::from_millis(700)).await;
        assert_eq!(disk_manifest(&root, id)["lastSequence"], 6);
        assert!(!store.manifests.get(id).unwrap().dirty);

        // A deleted conversation is never resurrected by a pending flush.
        store
            .append(id, "message.delta", json!({ "turnId": "t" }))
            .await
            .unwrap();
        store.delete(id).await.unwrap();
        store.flush_pending_deltas().await;
        assert!(store.list().await.unwrap().is_empty());
        assert!(matches!(store.get(id).await, Err(AppError::NotFound(_))));
        assert!(!root.join("conversations").join(id).exists());
        fs::remove_dir_all(root).unwrap();
    }

    #[tokio::test]
    async fn cached_manifests_match_disk_after_a_flush_and_after_restart() {
        let root = temp_dir("todex-manifest-consistency");
        let store = ConversationStore::new(root.clone()).await.unwrap();
        let mut ids = Vec::new();
        for _ in 0..3 {
            ids.push(
                store
                    .create(ConversationManifest::new(
                        ProviderKind::Codex,
                        root.clone(),
                        None,
                        None,
                    ))
                    .await
                    .unwrap()
                    .id,
            );
        }
        let operations = [
            "message.delta",
            "message.delta",
            "tool.completed",
            "turn.started",
            "permission.requested",
            "permission.resolved",
            "turn.completed",
            "turn.failed",
            "metadata",
        ];
        // Deterministic LCG: reproducible "random" interleaving.
        let mut state = 0x2545_f491_4f6c_dd1du64;
        for step in 0..240u64 {
            state = state
                .wrapping_mul(6_364_136_223_846_793_005)
                .wrapping_add(1_442_695_040_888_963_407);
            let id = &ids[(state >> 33) as usize % ids.len()];
            match operations[(state >> 40) as usize % operations.len()] {
                "metadata" => {
                    store
                        .update_metadata(
                            id,
                            Some(Some(format!("title {step}"))),
                            Some(step % 2 == 0),
                        )
                        .await
                        .unwrap();
                }
                event_type => {
                    store
                        .append(
                            id,
                            event_type,
                            json!({ "turnId": "t", "permissionId": "p", "step": step }),
                        )
                        .await
                        .unwrap();
                }
            }
        }
        store.flush_pending_deltas().await;
        let fresh = ConversationStore::new(root.clone()).await.unwrap();
        let listed = fresh.list().await.unwrap();
        assert_eq!(listed.len(), ids.len());
        for id in &ids {
            let cached = serde_json::to_value(store.get(id).await.unwrap()).unwrap();
            assert_eq!(cached, disk_manifest(&root, id));
            assert_eq!(
                cached,
                serde_json::to_value(fresh.get(id).await.unwrap()).unwrap()
            );
            let snapshot: Value = serde_json::from_slice(
                &fs::read(root.join("conversations").join(id).join(SNAPSHOT_FILE)).unwrap(),
            )
            .unwrap();
            assert_eq!(snapshot["status"], cached["status"]);
            let history = fresh.complete_history(id).await.unwrap();
            assert_eq!(cached["lastSequence"], history.len() as u64);
        }
        fs::remove_dir_all(root).unwrap();
    }

    #[tokio::test]
    async fn conversation_folder_is_sequenced_private_and_redacted() {
        let root = temp_dir("todex-conversation-store");
        let workspace = root.join("workspace");
        fs::create_dir_all(&workspace).unwrap();
        let store = ConversationStore::new(root.clone()).await.unwrap();
        let manifest = store
            .create(ConversationManifest::new(
                ProviderKind::Codex,
                workspace,
                Some("Example".to_owned()),
                None,
            ))
            .await
            .unwrap();

        let first = store
            .append(
                &manifest.id,
                "message.created",
                json!({
                    "content": "hello",
                    "authToken": "must-not-persist",
                    "nested": { "password": "also-secret" },
                }),
            )
            .await
            .unwrap();
        let second = store
            .append(&manifest.id, "turn.started", json!({ "turnId": "turn-1" }))
            .await
            .unwrap();
        assert_eq!((first.sequence, second.sequence), (1, 2));
        let replay = store.replay(&manifest.id, 0, 10).await.unwrap();
        assert!(replay
            .events
            .iter()
            .all(|event| event.provider == Some(ProviderKind::Codex)));

        let directory = root.join("conversations").join(&manifest.id);
        for name in [
            MANIFEST_FILE,
            EVENTS_FILE,
            SNAPSHOT_FILE,
            PROVIDER_STATE_FILE,
        ] {
            assert!(directory.join(name).is_file(), "missing {name}");
        }
        let raw = fs::read_to_string(directory.join(EVENTS_FILE)).unwrap();
        assert!(!raw.contains("must-not-persist"));
        assert!(!raw.contains("also-secret"));
        assert_eq!(raw.lines().count(), 2);

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                fs::metadata(&directory).unwrap().permissions().mode() & 0o777,
                0o700
            );
            assert_eq!(
                fs::metadata(directory.join(EVENTS_FILE))
                    .unwrap()
                    .permissions()
                    .mode()
                    & 0o777,
                0o600
            );
        }

        let recovered = ConversationStore::new(root.clone())
            .await
            .unwrap()
            .recover(&manifest.id)
            .await
            .unwrap();
        assert_eq!(recovered.status, ConversationStatus::Interrupted);
        assert_eq!(recovered.last_sequence, 2);
        assert!(matches!(
            store.get("../../outside").await,
            Err(AppError::InvalidRequest(_))
        ));
        let _ = fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn journal_recovers_only_an_invalid_tail() {
        let root = temp_dir("todex-conversation-tail");
        let workspace = root.join("workspace");
        fs::create_dir_all(&workspace).unwrap();
        let store = ConversationStore::new(root.clone()).await.unwrap();
        let manifest = store
            .create(ConversationManifest::new(
                ProviderKind::Pi,
                workspace,
                None,
                None,
            ))
            .await
            .unwrap();
        store
            .append(
                &manifest.id,
                "message.created",
                json!({ "content": "kept" }),
            )
            .await
            .unwrap();
        let directory = root.join("conversations").join(&manifest.id);
        let path = directory.join(EVENTS_FILE);
        let mut file = tokio::fs::OpenOptions::new()
            .append(true)
            .open(&path)
            .await
            .unwrap();
        file.write_all(b"{\"schemaVersion\":2").await.unwrap();
        file.flush().await.unwrap();
        drop(file);

        let replay = store.replay(&manifest.id, 0, 10).await.unwrap();
        assert_eq!(replay.events.len(), 1);
        let quarantined = fs::read_dir(&directory)
            .unwrap()
            .filter_map(|entry| entry.ok())
            .any(|entry| {
                entry
                    .file_name()
                    .to_string_lossy()
                    .starts_with("events.corrupt.")
            });
        assert!(quarantined);
        assert!(fs::read_to_string(path).unwrap().ends_with('\n'));
        let _ = fs::remove_dir_all(root);
    }

    /// `events.corrupt.*` copies next to the journal.
    fn corrupt_copies(path: &Path) -> Vec<PathBuf> {
        let mut copies = fs::read_dir(path.parent().unwrap())
            .unwrap()
            .filter_map(|entry| entry.ok())
            .filter(|entry| {
                entry
                    .file_name()
                    .to_string_lossy()
                    .starts_with("events.corrupt.")
            })
            .map(|entry| entry.path())
            .collect::<Vec<_>>();
        copies.sort();
        copies
    }

    fn write_lines(path: &Path, lines: &[String]) -> Vec<u8> {
        let raw = format!("{}\n", lines.join("\n")).into_bytes();
        fs::write(path, &raw).unwrap();
        raw
    }

    /// `(runStart, runLength)` of every placeholder, in sequence order.
    fn lost_runs(events: &[ConversationEvent]) -> Vec<(u64, u64, u64)> {
        events
            .iter()
            .filter(|event| event.event_type == JOURNAL_RECORD_LOST_EVENT)
            .map(|event| {
                (
                    event.sequence,
                    event.payload["runStart"].as_u64().unwrap(),
                    event.payload["runLength"].as_u64().unwrap(),
                )
            })
            .collect()
    }

    #[tokio::test]
    async fn journal_salvages_a_garbage_interior_line_with_a_placeholder() {
        let (root, id, path) = seed_journal("todex-salvage-garbage", 5).await;
        let original = fs::read_to_string(&path).unwrap();
        let mut lines = original.lines().map(str::to_owned).collect::<Vec<_>>();
        let second: ConversationEvent = serde_json::from_str(&lines[1]).unwrap();
        lines[2] = "\u{0}\u{0}garbage".to_owned();
        let corrupt = write_lines(&path, &lines);
        let store = ConversationStore::new(root.clone()).await.unwrap();

        let page = store.replay(&id, 0, 10).await.unwrap();
        assert_eq!(sequences(&page), vec![1, 2, 3, 4, 5]);
        let placeholder = &page.events[2];
        assert_eq!(placeholder.event_type, JOURNAL_RECORD_LOST_EVENT);
        assert_eq!(
            placeholder.normalized_type.as_deref(),
            Some(JOURNAL_RECORD_LOST_EVENT)
        );
        assert_eq!(placeholder.conversation_id, id);
        assert_eq!(placeholder.time, second.time);
        let copies = corrupt_copies(&path);
        assert_eq!(copies.len(), 1);
        assert_eq!(fs::read(&copies[0]).unwrap(), corrupt);
        let backup = copies[0].file_name().unwrap().to_string_lossy().to_string();
        assert_eq!(
            placeholder.payload,
            json!({ "reason": "corrupt", "runStart": 3, "runLength": 1, "backup": backup })
        );
        // Valid records survive byte for byte.
        let repaired = fs::read_to_string(&path).unwrap();
        let repaired_lines = repaired.lines().collect::<Vec<_>>();
        for index in [0, 1, 3, 4] {
            assert_eq!(repaired_lines[index], lines[index]);
        }

        // The rewrite is durable and valid: a restarted store reads it on the
        // fast path, appends continue the sequence and nothing is repaired
        // twice.
        let restarted = ConversationStore::new(root.clone()).await.unwrap();
        assert_eq!(
            sequences(&restarted.replay(&id, 0, 10).await.unwrap()),
            vec![1, 2, 3, 4, 5]
        );
        assert_eq!(
            restarted
                .append(&id, "turn.completed", json!({}))
                .await
                .unwrap()
                .sequence,
            6
        );
        let history = restarted.complete_history(&id).await.unwrap();
        assert_eq!(history.len(), 6);
        assert_eq!(corrupt_copies(&path).len(), 1);
        fs::remove_dir_all(root).unwrap();
    }

    #[tokio::test]
    async fn journal_salvages_two_records_glued_onto_one_line_as_a_run_of_two() {
        let (root, id, path) = seed_journal("todex-salvage-glued", 6).await;
        let original = fs::read_to_string(&path).unwrap();
        let mut lines = original.lines().map(str::to_owned).collect::<Vec<_>>();
        let glued = format!("{}{}", lines[2], lines[3]);
        lines.splice(2..4, [glued]);
        write_lines(&path, &lines);
        let store = ConversationStore::new(root.clone()).await.unwrap();

        let history = store.complete_history(&id).await.unwrap();
        assert_eq!(
            history
                .iter()
                .map(|event| event.sequence)
                .collect::<Vec<_>>(),
            vec![1, 2, 3, 4, 5, 6]
        );
        assert_eq!(lost_runs(&history), vec![(3, 3, 2), (4, 3, 2)]);
        assert_eq!(corrupt_copies(&path).len(), 1);
        fs::remove_dir_all(root).unwrap();
    }

    #[tokio::test]
    async fn journal_salvages_separate_corrupt_runs_and_a_corrupt_tail() {
        let (root, id, path) = seed_journal("todex-salvage-runs", 12).await;
        let original = fs::read_to_string(&path).unwrap();
        let mut lines = original.lines().map(str::to_owned).collect::<Vec<_>>();
        let first: ConversationEvent = serde_json::from_str(&lines[0]).unwrap();
        // Run one: sequences 2..=3 (garbage, then a repeat of sequence 1).
        lines[1] = "not json".to_owned();
        lines[2] = lines[0].clone();
        // Run two: sequence 7 torn mid-record.
        lines[6].truncate(40);
        // Run three: sequence 9 carries a damaged sequence number far ahead,
        // which must not swallow the records after it.
        let mut damaged: Value = serde_json::from_str(&lines[8]).unwrap();
        damaged["sequence"] = json!(1_000_000_000u64);
        lines[8] = damaged.to_string();
        // Unbounded tail: nothing valid follows, so it is cut, not salvaged.
        lines.push("{\"schemaVersion\":2,\"sequ".to_owned());
        let corrupt = write_lines(&path, &lines);
        let store = ConversationStore::new(root.clone()).await.unwrap();

        let history = store.complete_history(&id).await.unwrap();
        assert_eq!(
            history
                .iter()
                .map(|event| event.sequence)
                .collect::<Vec<_>>(),
            (1..=12).collect::<Vec<_>>()
        );
        assert_eq!(
            lost_runs(&history),
            vec![(2, 2, 2), (3, 2, 2), (7, 7, 1), (9, 9, 1)]
        );
        // A run at the start of the journal keeps the first record's time.
        assert_eq!(history[1].time, first.time);
        let copies = corrupt_copies(&path);
        assert_eq!(copies.len(), 1);
        assert_eq!(fs::read(&copies[0]).unwrap(), corrupt);
        let repaired = fs::read_to_string(&path).unwrap();
        assert_eq!(repaired.lines().count(), 12);
        assert!(repaired.ends_with('\n'));
        assert_eq!(repaired.lines().last(), original.lines().last());

        let page = store.replay_before(&id, u64::MAX, 4).await.unwrap();
        assert_eq!(sequences(&page), vec![9, 10, 11, 12]);
        fs::remove_dir_all(root).unwrap();
    }

    #[tokio::test]
    async fn journal_salvages_a_corrupt_first_record() {
        let (root, id, path) = seed_journal("todex-salvage-first", 3).await;
        let original = fs::read_to_string(&path).unwrap();
        let mut lines = original.lines().map(str::to_owned).collect::<Vec<_>>();
        lines[0] = "}".to_owned();
        write_lines(&path, &lines);
        let store = ConversationStore::new(root.clone()).await.unwrap();

        let page = store.replay(&id, 0, 10).await.unwrap();
        assert_eq!(sequences(&page), vec![1, 2, 3]);
        assert_eq!(lost_runs(&page.events), vec![(1, 1, 1)]);
        fs::remove_dir_all(root).unwrap();
    }

    #[tokio::test]
    async fn a_valid_journal_is_never_rewritten() {
        let (root, id, path) = seed_journal("todex-salvage-clean", 25).await;
        let original = fs::read(&path).unwrap();
        let store = ConversationStore::new(root.clone()).await.unwrap();

        let (manifest, history) = store.recover_with_history(&id).await.unwrap();
        assert_eq!(manifest.last_sequence, 25);
        assert_eq!(history.len(), 25);
        assert_eq!(
            sequences(&store.replay(&id, 0, 100).await.unwrap()),
            (1..=25).collect::<Vec<_>>()
        );
        assert_eq!(fs::read(&path).unwrap(), original);
        assert!(corrupt_copies(&path).is_empty());
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn every_journal_record_is_larger_than_the_salvage_minimum() {
        let smallest = ConversationEvent::new(Uuid::new_v4().to_string(), 1, "a", Value::Null);
        assert!(serde_json::to_vec(&smallest).unwrap().len() > MIN_JOURNAL_RECORD_BYTES);
    }

    #[tokio::test]
    async fn append_recovers_a_journal_record_written_before_manifest_update() {
        let root = temp_dir("todex-conversation-append-recovery");
        let workspace = root.join("workspace");
        fs::create_dir_all(&workspace).unwrap();
        let store = ConversationStore::new(root.clone()).await.unwrap();
        let manifest = store
            .create(ConversationManifest::new(
                ProviderKind::Codex,
                workspace,
                None,
                None,
            ))
            .await
            .unwrap();
        let orphaned = ConversationEvent::new(
            &manifest.id,
            1,
            "provider.event",
            json!({ "source": "crash-window" }),
        );
        fs::write(
            root.join("conversations")
                .join(&manifest.id)
                .join(EVENTS_FILE),
            format!("{}\n", serde_json::to_string(&orphaned).unwrap()),
        )
        .unwrap();

        let appended = store
            .append(&manifest.id, "provider.event", json!({ "source": "next" }))
            .await
            .unwrap();
        assert_eq!(appended.sequence, 2);
        assert_eq!(store.get(&manifest.id).await.unwrap().last_sequence, 2);
        assert_eq!(
            store
                .replay(&manifest.id, 0, 10)
                .await
                .unwrap()
                .events
                .len(),
            2
        );
        let _ = fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn append_compacts_a_full_journal_and_preserves_sequences() {
        let root = temp_dir("todex-journal-compact");
        let workspace = root.join("workspace");
        fs::create_dir_all(&workspace).unwrap();
        let store = ConversationStore::new(root.clone()).await.unwrap();
        let manifest = store
            .create(ConversationManifest::new(
                ProviderKind::Codex,
                workspace,
                None,
                None,
            ))
            .await
            .unwrap();
        let path = root
            .join("conversations")
            .join(&manifest.id)
            .join(EVENTS_FILE);

        // Hand-write a journal just under the cap; each event carries one
        // truncatable 256 KiB string.
        use std::io::Write;
        let bulky = "x".repeat(256 * 1024);
        let mut file = fs::File::create(&path).unwrap();
        let mut sequence = 0u64;
        let mut written = 0u64;
        loop {
            let mut line = serde_json::to_vec(&ConversationEvent::new(
                &manifest.id,
                sequence + 1,
                "tool.started",
                json!({ "output": bulky, "index": sequence }),
            ))
            .unwrap();
            line.push(b'\n');
            if written + line.len() as u64 > MAX_EVENTS_JOURNAL_BYTES - 2048 {
                break;
            }
            file.write_all(&line).unwrap();
            written += line.len() as u64;
            sequence += 1;
        }
        drop(file);
        store.recover(&manifest.id).await.unwrap();

        // A payload larger than the remaining headroom forces compaction.
        let appended = store
            .append(
                &manifest.id,
                "message.delta",
                json!({ "content": "y".repeat(300 * 1024) }),
            )
            .await
            .unwrap();
        assert_eq!(appended.sequence, sequence + 1);
        let compacted = fs::metadata(&path).unwrap().len();
        assert!(compacted <= JOURNAL_COMPACT_TARGET_BYTES + 400 * 1024);

        let events = store.complete_history(&manifest.id).await.unwrap();
        assert_eq!(events.len() as u64, sequence + 1);
        for (index, event) in events.iter().enumerate() {
            assert_eq!(event.sequence, index as u64 + 1);
        }
        // Older unprotected events lost their oversized payloads.
        let truncated_count = events[..sequence as usize]
            .iter()
            .filter(|event| {
                event.payload["output"]
                    .as_str()
                    .is_some_and(|output| output.contains("truncated"))
            })
            .count();
        assert!(truncated_count > 0);
        // The protected tail keeps full-fidelity payloads.
        let recent = events[(sequence - 1) as usize].payload["output"]
            .as_str()
            .unwrap();
        assert_eq!(recent.len(), bulky.len());
        // The byte index rebuilt during compaction still serves replay.
        let page = store.replay(&manifest.id, 0, 5).await.unwrap();
        assert_eq!(page.events.len(), 5);
        assert_eq!(page.events[0].sequence, 1);
        let _ = fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn journal_without_truncatable_payloads_still_reports_exhausted() {
        let root = temp_dir("todex-journal-exhausted");
        let workspace = root.join("workspace");
        fs::create_dir_all(&workspace).unwrap();
        let store = ConversationStore::new(root.clone()).await.unwrap();
        let manifest = store
            .create(ConversationManifest::new(
                ProviderKind::Codex,
                workspace,
                None,
                None,
            ))
            .await
            .unwrap();
        let path = root
            .join("conversations")
            .join(&manifest.id)
            .join(EVENTS_FILE);

        // Bulk made of many short strings leaves nothing to truncate.
        use std::io::Write;
        let items: Vec<String> = (0..3000).map(|index| format!("item-{index:05}")).collect();
        let mut file = fs::File::create(&path).unwrap();
        let mut sequence = 0u64;
        let mut written = 0u64;
        loop {
            let mut line = serde_json::to_vec(&ConversationEvent::new(
                &manifest.id,
                sequence + 1,
                "provider.event",
                json!({ "items": items }),
            ))
            .unwrap();
            line.push(b'\n');
            if written + line.len() as u64 > MAX_EVENTS_JOURNAL_BYTES - 2048 {
                break;
            }
            file.write_all(&line).unwrap();
            written += line.len() as u64;
            sequence += 1;
        }
        drop(file);
        store.recover(&manifest.id).await.unwrap();

        // A payload larger than the remaining headroom forces a compaction
        // attempt; with nothing to truncate the journal stays full.
        let error = store
            .append(
                &manifest.id,
                "message.delta",
                json!({ "content": "y".repeat(300 * 1024) }),
            )
            .await
            .unwrap_err();
        assert!(matches!(error, AppError::ResourceExhausted(_)));
        assert_eq!(error.code(), "RESOURCE_EXHAUSTED");
        let _ = fs::remove_dir_all(root);
    }

    /// Hand-write a journal just under the hard cap whose events carry
    /// `payload`, then recover so the store indexes it.
    async fn fill_journal(prefix: &str, payload: Value) -> (PathBuf, ConversationStore, String) {
        use std::io::Write;
        let root = temp_dir(prefix);
        let store = ConversationStore::new(root.clone()).await.unwrap();
        let manifest = store
            .create(ConversationManifest::new(
                ProviderKind::Codex,
                root.clone(),
                None,
                None,
            ))
            .await
            .unwrap();
        let path = root
            .join("conversations")
            .join(&manifest.id)
            .join(EVENTS_FILE);
        let mut file = fs::File::create(&path).unwrap();
        let mut sequence = 0u64;
        let mut written = 0u64;
        loop {
            let mut line = serde_json::to_vec(&ConversationEvent::new(
                &manifest.id,
                sequence + 1,
                "provider.event",
                payload.clone(),
            ))
            .unwrap();
            line.push(b'\n');
            if written + line.len() as u64 > MAX_EVENTS_JOURNAL_BYTES - 256 * 1024 {
                break;
            }
            file.write_all(&line).unwrap();
            written += line.len() as u64;
            sequence += 1;
        }
        drop(file);
        assert!(written > JOURNAL_PROMPT_LIMIT_BYTES);
        store.recover(&manifest.id).await.unwrap();
        (root, store, manifest.id)
    }

    #[tokio::test]
    async fn full_journal_refuses_new_prompts_but_running_turns_still_append() {
        // Bulk made of many short strings leaves nothing to compact.
        let items: Vec<String> = (0..3000).map(|index| format!("item-{index:05}")).collect();
        let (root, store, id) = fill_journal("todex-journal-full", json!({ "items": items })).await;
        let request = json!({ "turnId": "t" });

        let error = store.save_request(&id, &request).await.unwrap_err();
        assert!(matches!(error, AppError::JournalFull(_)));
        assert_eq!(error.code(), "JOURNAL_FULL");
        assert_eq!(store.last_request(&id).await.unwrap(), None);
        // The failed compaction is remembered for this exact journal.
        let journal = journal_metadata(&root.join("conversations").join(&id).join(EVENTS_FILE))
            .await
            .unwrap();
        assert_eq!(*store.incompressible.get(&id).unwrap(), journal);

        // A turn that already started keeps journalling up to the hard cap.
        let appended = store
            .append(&id, "turn.completed", json!({ "turnId": "t" }))
            .await
            .unwrap();
        assert!(appended.sequence > 1);
        // The journal changed, so the next prompt re-checks it (and fails).
        assert!(matches!(
            store.save_request(&id, &request).await,
            Err(AppError::JournalFull(_))
        ));
        fs::remove_dir_all(root).unwrap();
    }

    #[tokio::test]
    async fn prompt_on_a_full_but_compactable_journal_compacts_and_proceeds() {
        let (root, store, id) = fill_journal(
            "todex-journal-full-compactable",
            json!({ "output": "x".repeat(256 * 1024) }),
        )
        .await;
        let request = json!({ "turnId": "t" });
        store.save_request(&id, &request).await.unwrap();
        assert_eq!(store.last_request(&id).await.unwrap(), Some(request));
        let bytes = fs::metadata(root.join("conversations").join(&id).join(EVENTS_FILE))
            .unwrap()
            .len();
        assert!(bytes <= JOURNAL_COMPACT_TARGET_BYTES);
        assert!(store.incompressible.get(&id).is_none());
        fs::remove_dir_all(root).unwrap();
    }

    async fn delta(
        store: &ConversationStore,
        hub: &ConversationEventHub,
        id: &str,
        event_type: &str,
        block: &str,
        text: &str,
    ) {
        let payload = json!({ "delta": text, "block": { "id": block } });
        let fragment = DeltaFragment::from_payload(&payload, &["/delta"], None).unwrap();
        store
            .append_delta_and_publish(id, event_type, payload, fragment, hub)
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn coalesced_deltas_flush_in_order_before_any_other_event() {
        let root = temp_dir("todex-delta-order");
        let store = ConversationStore::new(root.clone()).await.unwrap();
        let conversation = store
            .create(ConversationManifest::new(
                ProviderKind::Codex,
                root.clone(),
                None,
                None,
            ))
            .await
            .unwrap();
        let id = conversation.id.as_str();
        let hub = ConversationEventHub::default();
        let mut receiver = hub.subscribe(id);
        for text in ["He", "ll", "o"] {
            delta(&store, &hub, id, "message.delta", "a", text).await;
        }
        delta(&store, &hub, id, "thought.delta", "a", "hmm").await;
        delta(&store, &hub, id, "message.delta", "b", "x").await;
        delta(&store, &hub, id, "message.delta", "b", "y").await;
        store
            .append_and_publish(id, "turn.completed", json!({ "turnId": "t" }), &hub)
            .await
            .unwrap();

        let expected = [
            ("message.delta", json!("Hello")),
            ("thought.delta", json!("hmm")),
            ("message.delta", json!("xy")),
            ("turn.completed", Value::Null),
        ];
        let history = store.complete_history(id).await.unwrap();
        assert_eq!(history.len(), expected.len());
        for (index, (event_type, text)) in expected.iter().enumerate() {
            let journalled = &history[index];
            let published = receiver.recv().await.unwrap();
            assert_eq!(journalled.sequence, index as u64 + 1);
            assert_eq!(journalled.event_type, *event_type);
            assert_eq!(journalled.payload["delta"], *text);
            assert_eq!(published.sequence, journalled.sequence);
            assert_eq!(published.payload, journalled.payload);
        }
        fs::remove_dir_all(root).unwrap();
    }

    #[tokio::test]
    async fn coalesced_delta_is_published_when_its_window_expires() {
        let root = temp_dir("todex-delta-window");
        let store = ConversationStore::new(root.clone()).await.unwrap();
        let conversation = store
            .create(ConversationManifest::new(
                ProviderKind::Codex,
                root.clone(),
                None,
                None,
            ))
            .await
            .unwrap();
        let hub = ConversationEventHub::default();
        let mut receiver = hub.subscribe(&conversation.id);
        delta(&store, &hub, &conversation.id, "message.delta", "a", "tail").await;
        let published = tokio::time::timeout(Duration::from_secs(2), receiver.recv())
            .await
            .expect("window expiry must publish the buffered fragment")
            .unwrap();
        assert_eq!(published.payload["delta"], "tail");
        assert_eq!(store.get(&conversation.id).await.unwrap().last_sequence, 1);
        fs::remove_dir_all(root).unwrap();
    }

    #[tokio::test]
    async fn open_delta_windows_are_journalled_on_shutdown_and_before_reads() {
        let root = temp_dir("todex-delta-flush");
        let store = ConversationStore::new(root.clone()).await.unwrap();
        let hub = ConversationEventHub::default();
        let mut ids = Vec::new();
        for _ in 0..2 {
            let conversation = store
                .create(ConversationManifest::new(
                    ProviderKind::Codex,
                    root.clone(),
                    None,
                    None,
                ))
                .await
                .unwrap();
            delta(&store, &hub, &conversation.id, "message.delta", "a", "x").await;
            ids.push(conversation.id);
        }
        store.flush_pending_deltas().await;
        assert_eq!(store.get(&ids[0]).await.unwrap().last_sequence, 1);
        assert_eq!(store.get(&ids[1]).await.unwrap().last_sequence, 1);

        delta(&store, &hub, &ids[0], "message.delta", "a", "y").await;
        let replay = store.replay(&ids[0], 0, 10).await.unwrap();
        assert_eq!(replay.events.len(), 2);
        assert_eq!(replay.events[1].payload["delta"], "y");
        fs::remove_dir_all(root).unwrap();
    }

    fn temp_dir(prefix: &str) -> PathBuf {
        std::env::temp_dir().join(format!("{prefix}-{}", Uuid::new_v4().simple()))
    }
}
