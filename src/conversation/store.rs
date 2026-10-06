use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use chrono::{DateTime, Utc};
use dashmap::DashMap;
use lru::LruCache;
use serde::de::DeserializeOwned;
use serde::Serialize;
use serde_json::Value;
use tokio::io::{AsyncReadExt, AsyncSeekExt, AsyncWriteExt};
use tokio::sync::{Mutex, OwnedMutexGuard};
use uuid::Uuid;

use crate::error::AppError;

use super::coalesce::{DeltaFragment, PendingDelta};
use super::digest::JournalDigest;
use super::maintenance::{available_space, MaintenanceQueue};
use super::record::{
    add_history_macs, decode_journal_record, encode_record, encrypted_content, seal_event,
    JOURNAL_COMPACTED_EVENT,
};
use super::segment::{
    self, CommitStep, FrameCache, FrameRef, MigrationKey, PreparedSegment, SealPolicy,
    SealedSegment, SegmentError, SegmentReader, JOURNAL_RECORD_LOST_EVENT,
};
use super::{
    redact_secrets, status_after_conversation_event, ConversationEvent, ConversationEventHub,
    ConversationManifest, ConversationReplay, ConversationSnapshot, ProviderState,
    CONVERSATION_SCHEMA_VERSION, MAX_EVENT_PAYLOAD_BYTES,
};
use crate::config::HistoryEncryption;
use crate::history_crypto::{self, ContentStream, SegmentKey};
use crate::history_keys::{FingerprintKey, HistoryKeys};

const MANIFEST_FILE: &str = "manifest.json";
const LAST_REQUEST_FILE: &str = "last-request.json";
pub(super) const EVENTS_FILE: &str = "events.jsonl";
const SNAPSHOT_FILE: &str = "snapshot.json";
const PROVIDER_STATE_FILE: &str = "provider-state.json";
/// Prompts waiting for the running turn to finish; never copied by fork.
const FOLLOW_UP_QUEUE_FILE: &str = "queue.json";
const MAX_REPLAY_LIMIT: usize = 1000;
/// New prompts are refused with `STORAGE_LOW` while the filesystem holding
/// the data directory has less than this free. History itself has no size
/// limit (`docs/history-encryption.md` §1): appends always land, so a
/// running turn can always finish.
pub(super) const STORAGE_LOW_BYTES: u64 = 1024 * 1024 * 1024;
/// Read buffer for the cold-index newline scan.
const JOURNAL_SCAN_BUFFER_BYTES: usize = 256 * 1024;
/// The journal is one logical sequence of files ordered by segment number:
/// sealed segments (`events.NNNNNN.seg` + `.idx`, or `events.NNNNNN.jsonl`
/// briefly between sealing and conversion) followed by the active
/// `events.jsonl`, the only file appends touch. Once the active file passes
/// this size it is sealed under the next number, a fresh active file takes
/// over and the background maintenance task converts the sealed file into a
/// compressed segment (see [`super::segment`]).
#[cfg(not(test))]
pub(super) const JOURNAL_SEGMENT_BYTES: u64 = 64 * 1024 * 1024;
/// Tests rotate the active segment well below the production size so a
/// multi-segment journal does not need hundreds of MiB of fixture writes.
#[cfg(test)]
pub(super) const JOURNAL_SEGMENT_BYTES: u64 = 128 * 1024;
/// Lower bound on the serialized size of any journal record line (each
/// carries a 36-byte event id plus its sequence, time and type). Salvage
/// uses it to bound how many sequences a corrupt region can have
/// swallowed, so a record with a damaged sequence number cannot conjure a
/// huge run of placeholders.
const MIN_JOURNAL_RECORD_BYTES: usize = 64;
/// Replay pages stop adding records once their journal bytes would exceed
/// this (records reach ~1 MiB, so 1000 of them could otherwise approach the
/// whole journal). A page always holds at least one record. Sealed records
/// count the length of their v3 line, as if they were still plaintext.
const MAX_REPLAY_PAGE_BYTES: u64 = 8 * 1024 * 1024;
/// Appends that leave the status unchanged only mark the cached manifest
/// dirty; it reaches `manifest.json` at most this often per conversation.
const MANIFEST_FLUSH_INTERVAL: std::time::Duration = std::time::Duration::from_secs(2);
/// Decompressed sealed frames kept across requests (process-wide).
const FRAME_CACHE_BYTES: usize = 32 * 1024 * 1024;
/// Replay indexes kept in memory; the least recently used is rebuilt from
/// the segment `.idx` files and the active file when needed again.
const INDEX_CACHE_ENTRIES: usize = 64;

#[derive(Clone)]
pub struct ConversationStore {
    pub(super) root: PathBuf,
    locks: Arc<DashMap<String, Arc<Mutex<()>>>>,
    /// Replay indexes, LRU-bounded. Only touched under the conversation
    /// lock and never held across an await.
    indexes: Arc<std::sync::Mutex<LruCache<String, JournalIndex>>>,
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
    /// Whole-journal digests (see [`super::digest`]): the merge of every
    /// sealed segment's persisted digest plus the plaintext files paged
    /// through, then folded forward by every append under the conversation
    /// lock. Like the replay index it is a rebuildable cache keyed by the
    /// journal file fingerprint.
    digests: Arc<DashMap<String, CachedDigest>>,
    /// Decompressed sealed frames shared by every conversation.
    frames: Arc<FrameCache>,
    /// Conversations whose directory went through crash reconciliation
    /// (see [`segment::reconcile_directory`]) in this process.
    reconciled: Arc<DashMap<String, ()>>,
    /// Conversations with a segment build in flight; their build
    /// temporaries are not leftovers.
    pub(super) sealing: Arc<DashMap<String, ()>>,
    /// Conversations with sealed plaintext waiting for conversion.
    pub(super) maintenance: Arc<MaintenanceQueue>,
    /// History encryption keys (`None`: plaintext only, as in most tests).
    /// See `docs/history-encryption.md` §3 and [`Self::with_history_keys`].
    pub(super) history: Option<HistoryKeys>,
    /// Test override for the free-space probe (`u64::MAX`: none).
    free_space_override: Arc<AtomicU64>,
    /// Crash-injection point for segment commits in tests.
    #[cfg(test)]
    commit_stop: Arc<std::sync::Mutex<Option<CommitStep>>>,
}

struct CachedDigest {
    /// Fingerprint of the journal files the digest describes in full.
    files: Vec<JournalFile>,
    digest: JournalDigest,
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

/// Rebuildable replay index of one conversation: a part per journal file.
/// Sealed segments contribute their range and frame table (from `.idx`);
/// plaintext files — the active file (at most one segment size) and sealed
/// files awaiting conversion or migration — keep per-record offsets.
#[derive(Clone)]
struct JournalIndex {
    /// Fingerprint of the ordered journal files the parts were built from.
    /// Any append, rotation, conversion, truncation or rewrite changes it,
    /// which is how the cache detects a stale index.
    files: Vec<JournalFile>,
    /// Contiguous sequence ranges starting at 1, in journal order.
    parts: Vec<IndexPart>,
    /// `false` when built by the cold scan, which parsed only the boundary
    /// records of each plaintext file. Pages still validate every record
    /// they return; one that fails triggers the full validating scan (see
    /// [`ConversationStore::read_indexed_page`]).
    fully_validated: bool,
}

#[derive(Clone)]
enum IndexPart {
    Sealed(Arc<SealedSegment>),
    /// `file` indexes [`JournalIndex::files`]; `offsets` are the
    /// `(start, end)` bytes of each record (`end` excludes the newline),
    /// the first holding sequence `first`.
    Plain {
        file: usize,
        first: u64,
        offsets: Vec<(u64, u64)>,
    },
}

impl IndexPart {
    fn first(&self) -> u64 {
        match self {
            Self::Sealed(segment) => segment.first,
            Self::Plain { first, .. } => *first,
        }
    }

    fn count(&self) -> u64 {
        match self {
            Self::Sealed(segment) => segment.count(),
            Self::Plain { offsets, .. } => offsets.len() as u64,
        }
    }
}

impl JournalIndex {
    fn total(&self) -> usize {
        self.parts.iter().map(IndexPart::count).sum::<u64>() as usize
    }

    /// Slices of the parts covering records `from..to` (0-based positions,
    /// i.e. sequences `from + 1..=to`), in journal order.
    fn slices(&self, from: usize, to: usize) -> Vec<PartSlice> {
        let (from, to) = (from as u64 + 1, to as u64);
        let mut slices = Vec::new();
        for part in &self.parts {
            let lo = part.first().max(from);
            let hi = (part.first() + part.count()).saturating_sub(1).min(to);
            if part.count() == 0 || lo > hi {
                continue;
            }
            slices.push(match part {
                IndexPart::Sealed(segment) => PartSlice::Sealed {
                    segment: segment.clone(),
                    first: lo,
                    last: hi,
                },
                IndexPart::Plain {
                    file,
                    first,
                    offsets,
                } => PartSlice::Plain {
                    name: self.files[*file].name.clone(),
                    first: lo,
                    offsets: offsets[(lo - first) as usize..=(hi - first) as usize].to_vec(),
                },
            });
        }
        slices
    }

    /// Record a durable append to the active file at `file`.
    fn push_active(&mut self, file: usize, sequence: u64, start: u64, end: u64) {
        if let Some(IndexPart::Plain {
            file: last_file,
            offsets,
            ..
        }) = self.parts.last_mut()
        {
            if *last_file == file {
                offsets.push((start, end));
                return;
            }
        }
        self.parts.push(IndexPart::Plain {
            file,
            first: sequence,
            offsets: vec![(start, end)],
        });
    }
}

/// The part of a replay window inside one journal file.
enum PartSlice {
    Sealed {
        segment: Arc<SealedSegment>,
        first: u64,
        last: u64,
    },
    Plain {
        name: String,
        first: u64,
        offsets: Vec<(u64, u64)>,
    },
}

/// Which ciphertext encrypted records carry in a replay (§5.3): the
/// summary or the full payload. Plaintext payloads are always full.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum ReplayDetail {
    #[default]
    Full,
    Summary,
}

/// One page read through the replay index.
struct IndexedPage {
    /// Window actually read (0-based positions) and the journal length.
    from: usize,
    to: usize,
    total: usize,
    events: Vec<ConversationEvent>,
    frames: serde_json::Map<String, Value>,
}

/// Which records of a sealed segment a page reads.
struct SealedSlice {
    segment: Arc<SealedSegment>,
    first: u64,
    last: u64,
    backward: bool,
    detail: ReplayDetail,
}

/// Directory inside a fork holding the sealed frames its copied records
/// refer to (`<frame id>.json`, the wire form `{kid, stream, counter, c,
/// ct}`).
const COPIED_FRAMES_DIR: &str = "frames";

/// Frame ids are `<segment id hex>-<offset hex>`; anything else is not
/// looked up on disk.
fn valid_frame_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 64
        && id
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit() || byte == b'-')
}

async fn read_copied_frame(directory: &Path, id: &str) -> Result<Option<Value>, AppError> {
    if !valid_frame_id(id) {
        return Ok(None);
    }
    let path = directory.join(COPIED_FRAMES_DIR).join(format!("{id}.json"));
    match tokio::fs::read(&path).await {
        Ok(bytes) => Ok(Some(serde_json::from_slice(&bytes)?)),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error.into()),
    }
}

/// Why a page read failed, so the caller can repair the right file.
enum PageError {
    Plain(AppError),
    Sealed(u64, AppError),
}

/// Byte budget of one replay page: every record is admitted until the
/// total would exceed the limit, but the first always is.
struct PageBudget {
    used: u64,
    limit: u64,
    admitted: bool,
}

impl PageBudget {
    fn new(limit: u64) -> Self {
        Self {
            used: 0,
            limit,
            admitted: false,
        }
    }

    fn admit(&mut self, bytes: u64) -> bool {
        let used = self.used.saturating_add(bytes);
        if self.admitted && used > self.limit {
            return false;
        }
        self.used = used;
        self.admitted = true;
        true
    }
}

/// One journal file in sequence order: a sealed `events.NNNNNN.seg`, a
/// sealed plaintext `events.NNNNNN.jsonl`, or the active `events.jsonl`
/// (always last when present). The `(name, bytes, modified)` triple
/// fingerprints it for cache validity.
#[derive(Clone, Debug, PartialEq)]
pub(super) struct JournalFile {
    pub name: String,
    pub bytes: u64,
    pub modified: Option<std::time::SystemTime>,
}

impl JournalFile {
    pub fn is_sealed_segment(&self) -> bool {
        self.name.ends_with(".seg")
    }

    /// Segment number; `None` for the active file.
    pub fn number(&self) -> Option<u64> {
        segment::numbered(&self.name, ".seg").or_else(|| sealed_segment_number(&self.name))
    }
}

#[derive(Clone)]
struct JournalTail {
    /// Name, size and mtime of the journal file the cached last record came
    /// from — the last non-empty file, not necessarily the active one.
    name: String,
    bytes: u64,
    modified: Option<std::time::SystemTime>,
    event: ConversationEvent,
}

fn blocking_error(error: tokio::task::JoinError) -> AppError {
    AppError::Anyhow(error.into())
}

fn segment_error(conversation_id: &str, number: u64, error: SegmentError) -> AppError {
    match error {
        SegmentError::Io(error) => error.into(),
        SegmentError::Invalid(reason) => AppError::InvalidRequest(format!(
            "conversation {conversation_id} journal segment {number} is unreadable: {reason}"
        )),
    }
}

impl ConversationStore {
    pub async fn new(data_dir: PathBuf) -> Result<Self, AppError> {
        let root = data_dir.join("conversations");
        tokio::fs::create_dir_all(&root).await?;
        set_owner_only(&root, true).await?;
        Ok(Self {
            root,
            locks: Arc::new(DashMap::new()),
            indexes: Arc::new(std::sync::Mutex::new(LruCache::new(
                std::num::NonZeroUsize::new(INDEX_CACHE_ENTRIES).expect("index cache is not empty"),
            ))),
            tails: Arc::new(DashMap::new()),
            pending_deltas: Arc::new(DashMap::new()),
            delta_generation: Arc::new(AtomicU64::new(0)),
            manifests: Arc::new(DashMap::new()),
            digests: Arc::new(DashMap::new()),
            frames: Arc::new(FrameCache::new(FRAME_CACHE_BYTES)),
            reconciled: Arc::new(DashMap::new()),
            sealing: Arc::new(DashMap::new()),
            maintenance: Arc::new(MaintenanceQueue::default()),
            history: None,
            free_space_override: Arc::new(AtomicU64::new(u64::MAX)),
            #[cfg(test)]
            commit_stop: Arc::new(std::sync::Mutex::new(None)),
        })
    }

    fn index_cache(&self) -> std::sync::MutexGuard<'_, LruCache<String, JournalIndex>> {
        self.indexes
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    fn index_get<R>(
        &self,
        conversation_id: &str,
        read: impl FnOnce(&JournalIndex) -> R,
    ) -> Option<R> {
        self.index_cache().get(conversation_id).map(read)
    }

    fn index_update<R>(
        &self,
        conversation_id: &str,
        update: impl FnOnce(&mut JournalIndex) -> R,
    ) -> Option<R> {
        self.index_cache().get_mut(conversation_id).map(update)
    }

    fn index_put(&self, conversation_id: &str, index: JournalIndex) {
        self.index_cache().put(conversation_id.to_owned(), index);
    }

    fn index_remove(&self, conversation_id: &str) {
        self.index_cache().pop(conversation_id);
    }

    /// Encrypt history with `keys` (`docs/history-encryption.md`). While
    /// their mode is `e2e` every new record is sealed under the
    /// conversation's current DEK, titles are stored as `titleEnc`, sealed
    /// segments are repacked into encrypted frames and older plaintext is
    /// migrated in the background.
    pub fn with_history_keys(mut self, keys: HistoryKeys) -> Self {
        self.history = Some(keys);
        self
    }

    /// The history encryption mode now (`off` without keys).
    pub(super) fn history_mode(&self) -> Result<HistoryEncryption, AppError> {
        match &self.history {
            Some(keys) => keys.recipients().mode(),
            None => Ok(HistoryEncryption::Off),
        }
    }

    /// Whether new history is encrypted now.
    pub fn history_encrypted(&self) -> Result<bool, AppError> {
        Ok(self.history_mode()? == HistoryEncryption::E2e)
    }

    /// The key behind `requestFingerprint` / `textMac` MACs, for comparing
    /// what earlier records stored (in any mode: records written while
    /// encryption was on keep their MACs). `None` without history keys.
    pub fn fingerprint_key(&self) -> Result<Option<Arc<FingerprintKey>>, AppError> {
        self.history
            .as_ref()
            .map(HistoryKeys::fingerprint)
            .transpose()
    }

    /// With history encryption on, new work must be able to encrypt what it
    /// writes: this creates (or confirms) the conversation's current DEK and
    /// fails with `CONFLICT` when no recipient is left to wrap it for, so a
    /// prompt is refused before its turn starts (§3.2). Turns already
    /// running keep their key; see [`Self::record_key`].
    pub async fn ensure_history_writable(&self, conversation_id: &str) -> Result<(), AppError> {
        if let Some(keys) = &self.history {
            keys.deks().current_key(conversation_id).await?;
        }
        Ok(())
    }

    /// The DEK the next record of `conversation_id` is sealed under and
    /// the fingerprint key; `None` while encryption is off. When no new DEK
    /// can be had (every recipient revoked, the keyring unwritable) a
    /// running turn keeps encrypting under the newest DEK still in memory;
    /// with none left the append fails rather than writing plaintext.
    async fn record_key(
        &self,
        conversation_id: &str,
    ) -> Result<Option<(String, Arc<SegmentKey>, Arc<FingerprintKey>)>, AppError> {
        let Some(keys) = &self.history else {
            return Ok(None);
        };
        let (kid, key) = match keys.deks().current_key(conversation_id).await {
            Ok(None) => return Ok(None),
            Ok(Some(current)) => current,
            Err(error) => match keys.deks().fallback_key(conversation_id).await {
                Some(previous) => {
                    tracing::error!(
                        conversation_id,
                        error = %error,
                        "no new history key; encrypting under the previous one until the turn ends"
                    );
                    previous
                }
                None => {
                    tracing::error!(
                        conversation_id,
                        error = %error,
                        "history encryption is on but no key is available; refusing to journal plaintext"
                    );
                    return Err(error);
                }
            },
        };
        Ok(Some((kid, key, keys.fingerprint()?)))
    }

    /// e2e: `manifest.titleEnc` for `title` (§3.2): stream 2, counter 0,
    /// under a fresh DEK of its own, so a new title never reuses a nonce.
    /// `None` while encryption is off.
    async fn seal_title(
        &self,
        conversation_id: &str,
        title: &str,
    ) -> Result<Option<super::model::TitleEnc>, AppError> {
        let Some(keys) = &self.history else {
            return Ok(None);
        };
        let Some((kid, key)) = keys.deks().fresh_key(conversation_id).await? else {
            return Ok(None);
        };
        let sealed = history_crypto::seal(
            &key,
            conversation_id,
            ContentStream::EventFull,
            0,
            title.as_bytes(),
        )?;
        Ok(Some(super::model::TitleEnc {
            kid,
            ct: base64::Engine::encode(&base64::engine::general_purpose::URL_SAFE_NO_PAD, sealed),
        }))
    }

    /// Set (or clear) the manifest title: plaintext while encryption is off,
    /// `titleEnc` with an empty `title` while it is on.
    pub(super) async fn apply_title(
        &self,
        manifest: &mut ConversationManifest,
        title: Option<String>,
    ) -> Result<(), AppError> {
        manifest.title_enc = None;
        manifest.title = None;
        let Some(title) = title else {
            return Ok(());
        };
        match self.seal_title(&manifest.id, &title).await? {
            Some(sealed) => manifest.title_enc = Some(sealed),
            None => manifest.title = Some(title),
        }
        Ok(())
    }

    pub async fn create(
        &self,
        manifest: ConversationManifest,
    ) -> Result<ConversationManifest, AppError> {
        self.create_with_history(manifest, Vec::new(), None, None)
            .await
    }

    /// Publish a conversation directory only after history, provider state
    /// and snapshot are complete.
    pub async fn create_with_history(
        &self,
        mut manifest: ConversationManifest,
        events: Vec<ConversationEvent>,
        provider_state: Option<ProviderState>,
        request: Option<Value>,
    ) -> Result<ConversationManifest, AppError> {
        validate_id(&manifest.id)?;
        for (index, event) in events.iter().enumerate() {
            validate_event(event, &manifest.id, index as u64 + 1)?;
        }
        let mut draft = self.begin_conversation(&manifest).await?;
        let filled = async {
            for event in &events {
                draft.writer.write(event).await?;
                manifest.status = status_after_conversation_event(manifest.status, event);
                manifest.last_sequence = event.sequence;
            }
            Ok(())
        }
        .await;
        self.publish_conversation(draft, filled, manifest, provider_state, request)
            .await
    }

    /// Create `manifest` with a copy of `source_id`'s journal, streamed page
    /// by page so a history of any size is copied in bounded memory. `copy`
    /// maps each source event to the record at the given new sequence;
    /// `trailer` receives the number copied and returns the events written
    /// after them. Only events present when the copy starts are copied.
    pub async fn create_from_journal(
        &self,
        source_id: &str,
        mut manifest: ConversationManifest,
        provider_state: Option<ProviderState>,
        request: Option<Value>,
        mut copy: impl FnMut(ConversationEvent, u64) -> ConversationEvent,
        trailer: impl FnOnce(u64) -> Vec<ConversationEvent>,
    ) -> Result<ConversationManifest, AppError> {
        validate_id(&manifest.id)?;
        let end = self
            .journal_tail(source_id)
            .await?
            .0
            .map_or(0, |event| event.sequence);
        let mut draft = self.begin_conversation(&manifest).await?;
        let temporary = draft.temporary.clone();
        let filled = async {
            let mut after = 0u64;
            let mut copied = 0u64;
            while after < end {
                let page = self.replay(source_id, after, MAX_REPLAY_LIMIT).await?;
                // Copied sealed records keep referring to their frames.
                write_copied_frames(&temporary, &page.frames).await?;
                for event in page.events {
                    if event.sequence > end {
                        break;
                    }
                    after = event.sequence;
                    copied += 1;
                    let next = copy(event, copied);
                    validate_event(&next, &manifest.id, copied)?;
                    manifest.status = status_after_conversation_event(manifest.status, &next);
                    manifest.last_sequence = next.sequence;
                    draft.writer.write(&next).await?;
                }
                if !page.has_more {
                    break;
                }
            }
            for next in trailer(copied) {
                validate_event(&next, &manifest.id, manifest.last_sequence + 1)?;
                manifest.status = status_after_conversation_event(manifest.status, &next);
                manifest.last_sequence = next.sequence;
                draft.writer.write(&next).await?;
            }
            // Copied ciphertext stays readable through the fork's own id:
            // its keys are the source's.
            if let Some(keys) = &self.history {
                keys.keyrings().copy_into(source_id, &temporary).await?;
            }
            Ok(())
        }
        .await;
        self.publish_conversation(draft, filled, manifest, provider_state, request)
            .await
    }

    /// A private temporary directory with an empty journal writer; see
    /// [`Self::publish_conversation`].
    async fn begin_conversation(
        &self,
        manifest: &ConversationManifest,
    ) -> Result<ConversationDraft, AppError> {
        if tokio::fs::try_exists(self.directory(&manifest.id)?).await? {
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
        let created = async {
            set_owner_only(&temporary, true).await?;
            PlainJournalWriter::create(&temporary).await
        }
        .await;
        match created {
            Ok(writer) => Ok(ConversationDraft { temporary, writer }),
            Err(error) => {
                let _ = tokio::fs::remove_dir_all(&temporary).await;
                Err(error)
            }
        }
    }

    /// Finish a draft whose journal `filled` reports on: manifest, snapshot
    /// and provider state are written, then the directory is renamed into
    /// place. Sealed plaintext files the writer rotated are queued for
    /// conversion. Any failure removes the draft.
    async fn publish_conversation(
        &self,
        draft: ConversationDraft,
        filled: Result<(), AppError>,
        mut manifest: ConversationManifest,
        provider_state: Option<ProviderState>,
        request: Option<Value>,
    ) -> Result<ConversationManifest, AppError> {
        manifest.storage_version = Some(super::model::STORAGE_VERSION);
        // With history encryption on the title is sealed once the
        // conversation directory (and so its keyring) exists; until then
        // no plaintext title is written. A conversation that starts empty
        // under encryption is entirely encrypted from its first record.
        let encrypting = self.history_encrypted()?;
        let title = if encrypting {
            manifest.title.take()
        } else {
            None
        };
        if encrypting && !draft.writer.wrote_records() {
            manifest.history_encrypted_at = Some(Utc::now());
        }
        let ConversationDraft { temporary, writer } = draft;
        let create_result = async {
            filled?;
            let sealed = writer.finish().await?;
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
                write_atomic_json(&temporary.join(LAST_REQUEST_FILE), &request).await?;
            }
            sync_directory(&temporary).await?;
            Ok::<_, AppError>(sealed)
        }
        .await;
        let sealed = match create_result {
            Ok(sealed) => sealed,
            Err(error) => {
                let _ = tokio::fs::remove_dir_all(&temporary).await;
                return Err(error);
            }
        };
        let _guard = self.lock(&manifest.id).await;
        let directory = self.directory(&manifest.id)?;
        let published = async {
            if tokio::fs::try_exists(&directory).await? {
                return Err(AppError::Conflict(format!(
                    "conversation {} already exists",
                    manifest.id
                )));
            }
            tokio::fs::rename(&temporary, &directory).await?;
            sync_directory(&self.root).await
        }
        .await;
        if let Err(error) = published {
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
        if sealed > 0 {
            self.maintenance.request(&manifest.id);
        }
        if let Some(title) = title {
            // Still under the conversation lock taken for the rename.
            self.apply_title(&mut manifest, Some(title)).await?;
            self.persist_manifest_locked(&manifest).await?;
        }
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
            let title = title
                .map(|value| value.trim().chars().take(200).collect::<String>())
                .filter(|value| !value.is_empty());
            self.apply_title(&mut manifest, title).await?;
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

    /// Drop every cache entry of a removed conversation, its DEKs included
    /// (its keyring went with its directory). Callers hold the
    /// conversation lock.
    fn forget_locked(&self, conversation_id: &str) {
        if let Some(keys) = &self.history {
            keys.deks().forget(conversation_id);
        }
        self.index_remove(conversation_id);
        self.tails.remove(conversation_id);
        self.pending_deltas.remove(conversation_id);
        self.manifests.remove(conversation_id);
        self.digests.remove(conversation_id);
        self.reconciled.remove(conversation_id);
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

    /// Whether an open merge window holds unjournalled stream text.
    pub(super) fn has_pending_delta(&self, conversation_id: &str) -> bool {
        self.pending_deltas.contains_key(conversation_id)
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
        let (last_event, mut terminated) = self.read_last_event(conversation_id).await?;
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
        // v3 records store microseconds; the published event matches what a
        // later replay decodes.
        event.time = truncate_to_micros(event.time);
        let event_path = directory.join(EVENTS_FILE);
        let mut files = self.files_locked(conversation_id, &directory).await?;
        // A torn final record (a write interrupted before its newline) must
        // be closed before anything else is appended. When it is the active
        // file's tail the separator is prepended to the new line below; when
        // a sealed plaintext file owns it (the active file is missing or
        // empty) it is terminated in place so the records never glue across
        // the file boundary. Sealed `.seg` files are always complete.
        if !terminated {
            if let Some(file) = files.iter().rev().find(|file| file.bytes > 0) {
                if file.name == EVENTS_FILE || file.is_sealed_segment() {
                    // Handled by the separator prepended to `line` below.
                } else {
                    terminate_journal(&directory.join(&file.name)).await?;
                    terminated = true;
                }
            } else {
                terminated = true;
            }
        }
        // Seal the active file once it passed its target size and start a
        // fresh `events.jsonl`. Sealing only renames the active file, so a
        // matching cached index survives by retagging its file list: the
        // sealed file holds exactly the bytes the active file held and the
        // fresh file starts empty. Conversion to a compressed segment runs
        // in the background.
        if files
            .last()
            .is_some_and(|file| file.name == EVENTS_FILE && file.bytes >= JOURNAL_SEGMENT_BYTES)
        {
            let previous = files.clone();
            let (sealed, fresh) = rotate_journal(&directory, &files, terminated).await?;
            *files.last_mut().expect("active segment is present") = sealed;
            files.push(fresh);
            self.index_update(conversation_id, |index| {
                if index.files == previous {
                    index.files = files.clone();
                }
            });
            if let Some(mut cached) = self.digests.get_mut(conversation_id) {
                if cached.files == previous {
                    cached.files = files.clone();
                }
            }
            terminated = true;
            self.maintenance.request(conversation_id);
            // Each DEK's records stay inside one sealed file, which is what
            // lets the seal converter repack them and then drop the key.
            if let Some(keys) = &self.history {
                keys.deks().rotate(conversation_id).await;
            }
        }
        // With history encryption on, the record is sealed now — after any
        // rotation, so it uses the new file's key — and the stored form
        // (envelope plus `$enc`) is what is journalled, published and
        // folded into the digest.
        let event = match self.record_key(conversation_id).await? {
            Some((kid, key, fingerprint)) => {
                add_history_macs(&event.event_type, &mut event.payload, &fingerprint);
                seal_event(&event, &kid, &key)?
            }
            None => {
                // Plaintext from here on: the conversation is no longer
                // entirely encrypted.
                manifest.history_encrypted_at = None;
                event
            }
        };
        let record = encode_record(&event)?;
        // `create` normally made the journal and rotation always creates the
        // fresh active file; only here does a missing one get created, and
        // only then do its permissions and directory entry need work.
        let created = files.last().is_none_or(|file| file.name != EVENTS_FILE);
        if created {
            files.push(JournalFile {
                name: EVENTS_FILE.to_owned(),
                bytes: 0,
                modified: None,
            });
        }
        let active_index = files.len() - 1;
        let active_bytes = files[active_index].bytes;
        // A torn sealed `.seg` cannot exist, and an unterminated active
        // file gets the separator.
        let separator = u64::from(!terminated && active_bytes > 0);
        let mut line = Vec::with_capacity(record.len() + 2);
        if separator == 1 {
            line.push(b'\n');
        }
        line.extend_from_slice(&record);
        line.push(b'\n');
        let pre_write_files = files.clone();
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
        let last = files.last_mut().expect("active segment is present");
        last.bytes = metadata.len();
        last.modified = metadata.modified().ok();
        self.index_update(conversation_id, |index| {
            if index.files == pre_write_files {
                index.push_active(
                    active_index,
                    event.sequence,
                    active_bytes + separator,
                    active_bytes + line.len() as u64 - 1,
                );
                index.files = files.clone();
            }
        });
        // A digest that described the journal up to this record stays exact
        // by folding the record in; any other is stale and dropped, so the
        // next query rebuilds it.
        let digest_current = match self.digests.get_mut(conversation_id) {
            Some(mut cached) if cached.files == pre_write_files => {
                cached.digest.apply(&event);
                cached.files = files;
                true
            }
            Some(_) => false,
            None => true,
        };
        if !digest_current {
            self.digests.remove(conversation_id);
        }
        self.tails.insert(
            conversation_id.to_owned(),
            JournalTail {
                name: EVENTS_FILE.to_owned(),
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
        // where a nearly full disk refuses new turns. Running turns are
        // unaffected — appends never refuse on size. Likewise a turn whose
        // history could not be encrypted (no recipient left) never starts.
        self.ensure_free_space(&directory).await?;
        self.ensure_history_writable(conversation_id).await?;
        write_atomic_json(&directory.join(LAST_REQUEST_FILE), request).await
    }

    /// `STORAGE_LOW` when the data directory's filesystem has less than
    /// [`STORAGE_LOW_BYTES`] available.
    async fn ensure_free_space(&self, directory: &Path) -> Result<(), AppError> {
        let available = match self.free_space_override.load(Ordering::Relaxed) {
            u64::MAX => {
                let directory = directory.to_owned();
                match tokio::task::spawn_blocking(move || available_space(&directory))
                    .await
                    .map_err(blocking_error)?
                {
                    Ok(bytes) => bytes,
                    Err(error) => {
                        // An unanswerable probe must not block prompts.
                        tracing::warn!(error = %error, "free disk space unavailable; not enforcing the storage floor");
                        return Ok(());
                    }
                }
            }
            bytes => bytes,
        };
        if available >= STORAGE_LOW_BYTES {
            return Ok(());
        }
        Err(AppError::StorageLow(format!(
            "only {available} bytes are free on the disk holding TodeX data, below the \
             {STORAGE_LOW_BYTES} byte floor for new turns; free disk space and retry"
        )))
    }

    /// Pretend the data directory's filesystem has `bytes` free.
    #[cfg(test)]
    pub fn set_free_space_for_tests(&self, bytes: Option<u64>) {
        self.free_space_override
            .store(bytes.unwrap_or(u64::MAX), Ordering::Relaxed);
    }

    pub async fn last_request(&self, conversation_id: &str) -> Result<Option<Value>, AppError> {
        let _guard = self.lock(conversation_id).await;
        let path = self.directory(conversation_id)?.join(LAST_REQUEST_FILE);
        if !tokio::fs::try_exists(&path).await? {
            return Ok(None);
        }
        Ok(Some(
            read_json(&path, "conversation request snapshot").await?,
        ))
    }

    /// The backend follow-up queue persisted beside the request snapshot;
    /// `None` when the conversation never queued anything.
    pub async fn follow_up_queue(&self, conversation_id: &str) -> Result<Option<Value>, AppError> {
        let _guard = self.lock(conversation_id).await;
        let path = self.directory(conversation_id)?.join(FOLLOW_UP_QUEUE_FILE);
        if !tokio::fs::try_exists(&path).await? {
            return Ok(None);
        }
        Ok(Some(
            read_json(&path, "conversation follow-up queue").await?,
        ))
    }

    /// Replaces the follow-up queue file atomically. The conversation must
    /// exist, so a deleted conversation cannot be recreated by a late write.
    pub async fn save_follow_up_queue(
        &self,
        conversation_id: &str,
        queue: &Value,
    ) -> Result<(), AppError> {
        let _guard = self.lock(conversation_id).await;
        let directory = self.directory(conversation_id)?;
        self.get_unlocked(conversation_id).await?;
        write_atomic_json(&directory.join(FOLLOW_UP_QUEUE_FILE), queue).await
    }

    /// Every event, paged through replay into one vector. Only tests use
    /// it; hot paths ask [`Self::digest`] and fork streams
    /// ([`Self::create_from_journal`]).
    #[cfg(test)]
    pub async fn complete_history(
        &self,
        conversation_id: &str,
    ) -> Result<Vec<ConversationEvent>, AppError> {
        {
            let _guard = self.lock(conversation_id).await;
            self.flush_pending_delta_logged(conversation_id).await;
            let directory = self.directory(conversation_id)?;
            if self
                .files_locked(conversation_id, &directory)
                .await?
                .is_empty()
            {
                return Ok(Vec::new());
            }
        }
        let mut events = Vec::new();
        let mut after = 0;
        loop {
            let page = self
                .replay(conversation_id, after, MAX_REPLAY_LIMIT)
                .await?;
            after = page.next_sequence;
            events.extend(page.events);
            if !page.has_more {
                return Ok(events);
            }
        }
    }

    /// Full-scan answer the digest replaced; tests keep it as the reference.
    #[cfg(test)]
    pub async fn last_user_message(
        &self,
        conversation_id: &str,
    ) -> Result<Option<ConversationEvent>, AppError> {
        let events = self.complete_history(conversation_id).await?;
        Ok(events.into_iter().rev().find(|event| {
            event.event_type == "message.created"
                && event.payload.get("role").and_then(Value::as_str) == Some("user")
        }))
    }

    /// Runs `read` against the conversation's journal digest, building the
    /// digest first when none matches the journal on disk. Pending stream
    /// text is journalled first, so the digest covers every emitted event.
    pub async fn digest<R>(
        &self,
        conversation_id: &str,
        read: impl FnOnce(&JournalDigest) -> R,
    ) -> Result<R, AppError> {
        let _guard = self.lock(conversation_id).await;
        self.flush_pending_delta_logged(conversation_id).await;
        self.ensure_digest_locked(conversation_id).await?;
        Ok(match self.digests.get(conversation_id) {
            Some(cached) => read(&cached.digest),
            None => read(&JournalDigest::default()),
        })
    }

    /// Point read of the event with `sequence` through the replay index;
    /// `None` when the journal holds no such event. Callers pair it with
    /// [`Self::digest`], which names the sequence, to reach payload content
    /// without loading the history around it.
    pub async fn event_at(
        &self,
        conversation_id: &str,
        sequence: u64,
    ) -> Result<Option<ConversationEvent>, AppError> {
        let Some(after) = sequence.checked_sub(1) else {
            return Ok(None);
        };
        let page = self.replay(conversation_id, after, 1).await?;
        Ok(page
            .events
            .into_iter()
            .next()
            .filter(|event| event.sequence == sequence))
    }

    /// Make the cached digest match the journal files on disk. A miss
    /// merges the digest each sealed segment persisted in its `.idx` (no
    /// sealed bytes are read) and pages through the plaintext files with
    /// the replay index — the same validation and salvage fallback as
    /// replay, bounded by one page of events in memory — and restarts when
    /// a repair changed the journal mid-build. An absent journal has the
    /// empty digest and caches nothing. Callers hold the conversation lock.
    async fn ensure_digest_locked(&self, conversation_id: &str) -> Result<(), AppError> {
        let directory = self.directory(conversation_id)?;
        let files = self.files_locked(conversation_id, &directory).await?;
        if files.is_empty() {
            self.digests.remove(conversation_id);
            return Ok(());
        }
        if self
            .digests
            .get(conversation_id)
            .is_some_and(|cached| cached.files == files)
        {
            return Ok(());
        }
        self.digests.remove(conversation_id);
        let index_files =
            |store: &Self| store.index_get(conversation_id, |index| index.files.clone());
        // One pass normally suffices; a second follows a salvage rewrite.
        for _ in 0..3 {
            let directory = self.replay_journal(conversation_id).await?;
            let Some(start_files) = index_files(self) else {
                return Err(AppError::InvalidRequest(format!(
                    "conversation {conversation_id} journal index is missing"
                )));
            };
            let ranges: Vec<(Option<u64>, u64, u64)> = self
                .index_get(conversation_id, |index| {
                    index
                        .parts
                        .iter()
                        .map(|part| match part {
                            IndexPart::Sealed(segment) => {
                                (Some(segment.number), part.first(), part.count())
                            }
                            IndexPart::Plain { .. } => (None, part.first(), part.count()),
                        })
                        .collect()
                })
                .unwrap_or_default();
            let mut digest = JournalDigest::default();
            let mut complete = true;
            'parts: for (number, first, count) in ranges {
                if let Some(number) = number {
                    let path = directory.clone();
                    let loaded =
                        tokio::task::spawn_blocking(move || segment::load_index(&path, number))
                            .await
                            .map_err(blocking_error)?;
                    match loaded {
                        Ok(loaded) => digest.merge(loaded.body.digest),
                        Err(error) => {
                            // Unreadable since the index was built: salvage
                            // through the page path, then start over.
                            tracing::warn!(conversation_id, number, error = %error, "sealed segment index unreadable while building the digest");
                            self.salvage_segment_locked(conversation_id, &directory, number)
                                .await?;
                            complete = false;
                            break 'parts;
                        }
                    }
                    continue;
                }
                let mut from = (first - 1) as usize;
                let end = (first - 1 + count) as usize;
                while from < end {
                    let page = self
                        .read_indexed_page(
                            conversation_id,
                            &directory,
                            PageAnchor::Start,
                            ReplayDetail::Full,
                            |_| (from, from.saturating_add(MAX_REPLAY_LIMIT).min(end)),
                        )
                        .await?;
                    let (to, events) = (page.to, page.events);
                    if index_files(self).as_ref() != Some(&start_files) {
                        complete = false;
                        break 'parts;
                    }
                    for event in &events {
                        digest.apply(event);
                    }
                    if events.is_empty() {
                        break;
                    }
                    from = to;
                }
            }
            if complete && index_files(self).as_ref() == Some(&start_files) {
                self.digests.insert(
                    conversation_id.to_owned(),
                    CachedDigest {
                        files: start_files,
                        digest,
                    },
                );
                return Ok(());
            }
        }
        Err(AppError::Conflict(format!(
            "conversation {conversation_id} journal kept changing while its digest was built"
        )))
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
        self.replay_detail(conversation_id, after_sequence, limit, ReplayDetail::Full)
            .await
    }

    /// [`Self::replay`] with the ciphertext of encrypted records in
    /// `detail`: summary pages carry summary frames and `$enc` objects whose
    /// separate summary ciphertext exists. Plaintext payloads are always
    /// full; callers summarize them.
    pub async fn replay_detail(
        &self,
        conversation_id: &str,
        after_sequence: u64,
        limit: usize,
        detail: ReplayDetail,
    ) -> Result<ConversationReplay, AppError> {
        let _guard = self.lock(conversation_id).await;
        // Readers see every fragment emitted so far.
        self.flush_pending_delta_logged(conversation_id).await;
        let limit = limit.clamp(1, MAX_REPLAY_LIMIT);
        let directory = self.replay_journal(conversation_id).await?;
        let page = self
            .read_indexed_page(
                conversation_id,
                &directory,
                PageAnchor::Start,
                detail,
                |total| {
                    let from = usize::try_from(after_sequence)
                        .unwrap_or(usize::MAX)
                        .min(total);
                    (from, from.saturating_add(limit).min(total))
                },
            )
            .await?;
        let next_sequence = page
            .events
            .last()
            .map_or(after_sequence, |event| event.sequence);
        Ok(ConversationReplay {
            conversation_id: conversation_id.to_owned(),
            from_sequence: after_sequence,
            next_sequence,
            has_more: page.to < page.total,
            events: page.events,
            frames: page.frames,
        })
    }

    /// Reverse replay for lazy history loading: returns the newest events with
    /// `sequence <= before_sequence` in ascending order. `has_more` reports
    /// whether earlier events remain, so clients page back with
    /// `before_sequence = first_returned_sequence - 1` (`from_sequence`). The
    /// same byte budget as [`Self::replay`] applies, dropping the oldest
    /// records of the window first.
    #[cfg(test)]
    pub async fn replay_before(
        &self,
        conversation_id: &str,
        before_sequence: u64,
        limit: usize,
    ) -> Result<ConversationReplay, AppError> {
        self.replay_before_detail(conversation_id, before_sequence, limit, ReplayDetail::Full)
            .await
    }

    /// [`Self::replay_before`] in `detail`; see [`Self::replay_detail`].
    pub async fn replay_before_detail(
        &self,
        conversation_id: &str,
        before_sequence: u64,
        limit: usize,
        detail: ReplayDetail,
    ) -> Result<ConversationReplay, AppError> {
        let _guard = self.lock(conversation_id).await;
        // Readers see every fragment emitted so far.
        self.flush_pending_delta_logged(conversation_id).await;
        let limit = limit.clamp(1, MAX_REPLAY_LIMIT);
        let directory = self.replay_journal(conversation_id).await?;
        let page = self
            .read_indexed_page(
                conversation_id,
                &directory,
                PageAnchor::End,
                detail,
                |total| {
                    let to = usize::try_from(before_sequence)
                        .unwrap_or(usize::MAX)
                        .min(total);
                    (to.saturating_sub(limit), to)
                },
            )
            .await?;
        let next_sequence = page
            .events
            .last()
            .map_or(before_sequence, |event| event.sequence);
        Ok(ConversationReplay {
            conversation_id: conversation_id.to_owned(),
            from_sequence: page.from as u64,
            next_sequence,
            has_more: page.from > 0,
            events: page.events,
            frames: page.frames,
        })
    }

    /// The conversation's ordered journal files, after crash
    /// reconciliation of its directory once per process. Callers hold the
    /// conversation lock.
    pub(super) async fn files_locked(
        &self,
        conversation_id: &str,
        directory: &Path,
    ) -> Result<Vec<JournalFile>, AppError> {
        if !self.reconciled.contains_key(conversation_id) {
            let building = self.sealing.contains_key(conversation_id);
            let path = directory.to_owned();
            let outcome =
                tokio::task::spawn_blocking(move || segment::reconcile_directory(&path, building))
                    .await
                    .map_err(blocking_error)??;
            if outcome.changed {
                tracing::warn!(
                    conversation_id,
                    "reconciled journal segments after an interrupted conversion"
                );
                self.index_remove(conversation_id);
                self.digests.remove(conversation_id);
                self.tails.remove(conversation_id);
            }
            self.reconciled.insert(conversation_id.to_owned(), ());
        }
        journal_files(directory).await
    }

    /// Shared replay prelude: the manifest must exist and the index — a
    /// rebuildable cache — must match the journal files on disk. Returns
    /// the conversation directory replay pages read segments from.
    /// Callers hold the conversation lock.
    async fn replay_journal(&self, conversation_id: &str) -> Result<PathBuf, AppError> {
        let directory = self.directory(conversation_id)?;
        if !tokio::fs::try_exists(directory.join(MANIFEST_FILE)).await? {
            return Err(AppError::NotFound(format!(
                "conversation {conversation_id}"
            )));
        }
        let files = self.files_locked(conversation_id, &directory).await?;
        if files.is_empty() {
            return Err(std::io::Error::from(std::io::ErrorKind::NotFound).into());
        }
        let valid_index = self
            .index_get(conversation_id, |index| index.files == files)
            .unwrap_or(false);
        if !valid_index
            && !self
                .index_journal_fast(conversation_id, &directory, files)
                .await?
        {
            // Validate and repair once; the index is only a rebuildable cache.
            self.validate_journal_locked(conversation_id).await?;
        }
        Ok(directory)
    }

    /// Cold index build without deserializing every record: each sealed
    /// segment contributes its `.idx` (no `.seg` bytes are read) and each
    /// plaintext file a newline scan plus validation of its boundary
    /// records. Returns `false` when the journal is not in the clean shape
    /// appends and conversions leave behind so the caller runs the full
    /// validating scan with its repairs.
    async fn index_journal_fast(
        &self,
        conversation_id: &str,
        directory: &Path,
        files: Vec<JournalFile>,
    ) -> Result<bool, AppError> {
        let path = directory.to_owned();
        let id = conversation_id.to_owned();
        let scan_files = files.clone();
        let scanned =
            tokio::task::spawn_blocking(move || scan_journal_fast(&path, &scan_files, &id))
                .await
                .map_err(blocking_error)??;
        let Some((parts, last)) = scanned else {
            tracing::debug!(
                conversation_id,
                "journal needs a full scan to build its replay index"
            );
            return Ok(false);
        };
        if let Some((file, event)) = last {
            self.tails.insert(
                conversation_id.to_owned(),
                JournalTail {
                    name: files[file].name.clone(),
                    bytes: files[file].bytes,
                    modified: files[file].modified,
                    event,
                },
            );
        }
        self.index_put(
            conversation_id,
            JournalIndex {
                files,
                parts,
                fully_validated: false,
            },
        );
        Ok(true)
    }

    /// Read the page `window(total)` selects from the current index, cut to
    /// [`MAX_REPLAY_PAGE_BYTES`] from its `anchor` end. A record that does
    /// not parse or validate is repaired where it lives: a plaintext file
    /// through the full validating scan (when the index was built fast), a
    /// sealed segment by segment salvage; the page is then read again.
    async fn read_indexed_page(
        &self,
        conversation_id: &str,
        directory: &Path,
        anchor: PageAnchor,
        detail: ReplayDetail,
        window: impl Fn(usize) -> (usize, usize),
    ) -> Result<IndexedPage, AppError> {
        let mut attempt = 0;
        loop {
            attempt += 1;
            let total = self.journal_len(conversation_id);
            let (from, to) = window(total);
            let error = match self
                .read_window(
                    conversation_id,
                    directory,
                    from.min(to),
                    to.min(total),
                    anchor,
                    detail,
                )
                .await
            {
                Ok(mut page) => {
                    page.total = total;
                    return Ok(page);
                }
                Err(error) => error,
            };
            match error {
                PageError::Sealed(number, error) if attempt < 3 => {
                    tracing::warn!(conversation_id, number, error = %error, "unreadable sealed journal segment; salvaging it");
                    self.salvage_segment_locked(conversation_id, directory, number)
                        .await?;
                    self.replay_journal(conversation_id).await?;
                }
                PageError::Plain(error)
                    if attempt < 3
                        && !self
                            .index_get(conversation_id, |index| index.fully_validated)
                            .unwrap_or(true) =>
                {
                    tracing::warn!(conversation_id, error = %error, "unreadable record behind the fast journal index; running full validation");
                    self.validate_journal_locked(conversation_id).await?;
                }
                PageError::Plain(error) | PageError::Sealed(_, error) => return Err(error),
            }
        }
    }

    fn journal_len(&self, conversation_id: &str) -> usize {
        self.index_get(conversation_id, JournalIndex::total)
            .unwrap_or_default()
    }

    /// Records `from..to` (0-based positions) under the page byte budget,
    /// kept from the `anchor` end, with the sealed frames they refer to.
    async fn read_window(
        &self,
        conversation_id: &str,
        directory: &Path,
        from: usize,
        to: usize,
        anchor: PageAnchor,
        detail: ReplayDetail,
    ) -> Result<IndexedPage, PageError> {
        let mut page = IndexedPage {
            from,
            to: from,
            total: 0,
            events: Vec::new(),
            frames: serde_json::Map::new(),
        };
        if from >= to {
            return Ok(page);
        }
        let mut slices = self
            .index_get(conversation_id, |index| index.slices(from, to))
            .unwrap_or_default();
        let backward = matches!(anchor, PageAnchor::End);
        if backward {
            slices.reverse();
        }
        let mut budget = PageBudget::new(MAX_REPLAY_PAGE_BYTES);
        let mut chunks: Vec<Vec<ConversationEvent>> = Vec::new();
        let mut read = 0usize;
        for slice in slices {
            let (events, complete) = match slice {
                PartSlice::Plain {
                    name,
                    first,
                    offsets,
                } => self
                    .read_plain_slice(
                        conversation_id,
                        directory,
                        &name,
                        first,
                        &offsets,
                        backward,
                        &mut budget,
                    )
                    .await
                    .map_err(PageError::Plain)?,
                PartSlice::Sealed {
                    segment,
                    first,
                    last,
                } => {
                    let number = segment.number;
                    self.read_sealed_slice(
                        conversation_id,
                        directory,
                        SealedSlice {
                            segment,
                            first,
                            last,
                            backward,
                            detail,
                        },
                        &mut budget,
                        &mut page.frames,
                    )
                    .await
                    .map_err(|error| PageError::Sealed(number, error))?
                }
            };
            read += events.len();
            chunks.push(events);
            if !complete {
                break;
            }
        }
        if backward {
            chunks.reverse();
        }
        page.events = chunks.into_iter().flatten().collect();
        // Forked copies of sealed records refer to frames the fork keeps
        // beside its journal.
        self.attach_copied_frames(directory, &page.events, detail, &mut page.frames)
            .await
            .map_err(PageError::Plain)?;
        if backward {
            page.from = to - read;
            page.to = to;
        } else {
            page.to = from + read;
        }
        Ok(page)
    }

    /// Adds the frames of event-level `$enc.fr` references (a fork's copies
    /// of sealed records, see [`Self::create_from_journal`]) from the
    /// conversation's `frames/` directory: under `detail` the matching frame
    /// when present, else the other.
    async fn attach_copied_frames(
        &self,
        directory: &Path,
        events: &[ConversationEvent],
        detail: ReplayDetail,
        frames: &mut serde_json::Map<String, Value>,
    ) -> Result<(), AppError> {
        for event in events {
            let Some(reference) = encrypted_content(&event.payload)
                .and_then(|encrypted| encrypted.get("fr"))
                .and_then(Value::as_object)
            else {
                continue;
            };
            let ids = match detail {
                ReplayDetail::Summary => ["s", "f"],
                ReplayDetail::Full => ["f", "s"],
            };
            for id in ids.iter().filter_map(|key| reference.get(*key)?.as_str()) {
                if frames.contains_key(id) {
                    break;
                }
                if let Some(frame) = read_copied_frame(directory, id).await? {
                    frames.insert(id.to_owned(), frame);
                    break;
                }
            }
        }
        Ok(())
    }

    /// Read the plaintext records `offsets` (the first holding `first`)
    /// that fit the budget, from the start or (`backward`) the end, as one
    /// contiguous span. Returns the records in order and whether all fit.
    #[allow(clippy::too_many_arguments)]
    async fn read_plain_slice(
        &self,
        conversation_id: &str,
        directory: &Path,
        name: &str,
        first: u64,
        offsets: &[(u64, u64)],
        backward: bool,
        budget: &mut PageBudget,
    ) -> Result<(Vec<ConversationEvent>, bool), AppError> {
        let size = |(start, end): &(u64, u64)| end.saturating_sub(*start);
        let (lo, hi) = if backward {
            let mut lo = offsets.len();
            while lo > 0 && budget.admit(size(&offsets[lo - 1])) {
                lo -= 1;
            }
            (lo, offsets.len())
        } else {
            let mut hi = 0;
            while hi < offsets.len() && budget.admit(size(&offsets[hi])) {
                hi += 1;
            }
            (0, hi)
        };
        let complete = lo == 0 && hi == offsets.len();
        if lo == hi {
            return Ok((Vec::new(), complete));
        }
        let start = offsets[lo].0;
        let end = offsets[hi - 1].1;
        let mut file = tokio::fs::File::open(directory.join(name)).await?;
        file.seek(std::io::SeekFrom::Start(start)).await?;
        let mut page = vec![0u8; (end - start) as usize];
        file.read_exact(&mut page).await?;
        let mut events = Vec::with_capacity(hi - lo);
        for (index, (start_offset, end_offset)) in offsets[lo..hi].iter().enumerate() {
            let line =
                trim_ascii(&page[(start_offset - start) as usize..(end_offset - start) as usize]);
            let event = decode_journal_record(line, conversation_id)?;
            validate_event(&event, conversation_id, first + (lo + index) as u64)?;
            events.push(event);
        }
        Ok((events, complete))
    }

    /// Sealed counterpart of [`Self::read_plain_slice`] for sequences
    /// `first..=last` of `segment`; sizes are known only once a record is
    /// decoded, so the budget is applied as records are read. A sealed
    /// frame counts once per page, when its first record is admitted, and
    /// is added to `frames`.
    async fn read_sealed_slice(
        &self,
        conversation_id: &str,
        directory: &Path,
        slice: SealedSlice,
        budget: &mut PageBudget,
        frames: &mut serde_json::Map<String, Value>,
    ) -> Result<(Vec<ConversationEvent>, bool), AppError> {
        let directory = directory.to_owned();
        let cache = self.frames.clone();
        let id = conversation_id.to_owned();
        let mut owned_budget = PageBudget::new(budget.limit);
        owned_budget.used = budget.used;
        owned_budget.admitted = budget.admitted;
        let mut owned_frames = std::mem::take(frames);
        let (events, complete, owned_budget, owned_frames) =
            tokio::task::spawn_blocking(move || {
                let SealedSlice {
                    segment,
                    first,
                    last,
                    backward,
                    detail,
                } = slice;
                let number = segment.number;
                let mut reader = SegmentReader::open(&directory, &segment, &cache, &id)
                    .map_err(|error| segment_error(&id, number, error))?;
                let mut events = Vec::new();
                let mut complete = true;
                let sequences: Box<dyn Iterator<Item = u64>> = if backward {
                    Box::new((first..=last).rev())
                } else {
                    Box::new(first..=last)
                };
                for sequence in sequences {
                    let record = reader
                        .record(sequence, detail == ReplayDetail::Summary)
                        .map_err(|error| segment_error(&id, number, error))?;
                    let new_frame = record
                        .frame
                        .as_ref()
                        .filter(|frame| !owned_frames.contains_key(&frame.id));
                    let frame_bytes = new_frame.map_or(0, |frame: &FrameRef| {
                        u64::from(frame.entry.len).div_ceil(3) * 4
                    });
                    if !owned_budget.admit(record.bytes + frame_bytes) {
                        complete = false;
                        break;
                    }
                    if let Some(frame) = new_frame {
                        let wire = reader
                            .wire_frame(&frame.entry)
                            .map_err(|error| segment_error(&id, number, error))?;
                        owned_frames.insert(frame.id.clone(), wire);
                    }
                    validate_event(&record.event, &id, sequence)?;
                    events.push(record.event);
                }
                if backward {
                    events.reverse();
                }
                Ok::<_, AppError>((events, complete, owned_budget, owned_frames))
            })
            .await
            .map_err(blocking_error)??;
        *budget = owned_budget;
        *frames = owned_frames;
        Ok((events, complete))
    }

    /// Rebuild sealed segment `number` from what still reads (see
    /// [`segment::salvage_segment`]), keeping the damaged files as
    /// `events.corrupt.<ts>.seg`/`.idx`. Callers hold the conversation lock.
    async fn salvage_segment_locked(
        &self,
        conversation_id: &str,
        directory: &Path,
        number: u64,
    ) -> Result<(), AppError> {
        // The range comes from the index when it still knows the segment,
        // else from its neighbours.
        let known = self
            .index_get(conversation_id, |index| {
                index.parts.iter().find_map(|part| match part {
                    IndexPart::Sealed(segment) if segment.number == number => {
                        Some((segment.first, Some(segment.last)))
                    }
                    _ => None,
                })
            })
            .flatten();
        let (first, last) = match known {
            Some(range) => range,
            None => {
                self.neighbour_range(conversation_id, directory, number)
                    .await?
            }
        };
        let backup = corrupt_copy_path(&directory.join(EVENTS_FILE), "seg");
        let backup_name = backup
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_default();
        let path = directory.to_owned();
        let id = conversation_id.to_owned();
        let lost = tokio::task::spawn_blocking(move || {
            let salvaged = segment::salvage_segment(&path, &id, number, first, last, &backup_name)?;
            if let Err(error) =
                segment::replace_with_salvaged(&path, &salvaged.prepared, &backup_name)
            {
                salvaged.prepared.discard();
                return Err(error);
            }
            Ok(salvaged.lost)
        })
        .await
        .map_err(blocking_error)?
        .map_err(|error| segment_error(conversation_id, number, error))?;
        tracing::warn!(
            conversation_id,
            number,
            lost_records = lost,
            "salvaged corrupt sealed journal segment; lost records replaced with placeholders"
        );
        self.index_remove(conversation_id);
        self.digests.remove(conversation_id);
        self.tails.remove(conversation_id);
        Ok(())
    }

    /// Sequence range of segment `number` from the files around it, for a
    /// segment whose own index is unusable: it starts after the previous
    /// file's last record and ends before the next file's first.
    async fn neighbour_range(
        &self,
        conversation_id: &str,
        directory: &Path,
        number: u64,
    ) -> Result<(u64, Option<u64>), AppError> {
        let files = journal_files(directory).await?;
        let position = files
            .iter()
            .position(|file| file.is_sealed_segment() && file.number() == Some(number))
            .ok_or_else(|| {
                AppError::NotFound(format!("conversation {conversation_id} segment {number}"))
            })?;
        let path = directory.to_owned();
        let id = conversation_id.to_owned();
        let before = files[..position].to_vec();
        let after = files[position + 1..].to_vec();
        tokio::task::spawn_blocking(move || {
            let first = match before.last() {
                None => 1,
                Some(file) => boundary_sequence(&path, file, &id, true)?
                    .map(|last| last + 1)
                    .ok_or_else(|| {
                        AppError::InvalidRequest(format!(
                            "conversation {id} journal has no readable record before segment {number}"
                        ))
                    })?,
            };
            let last = match after.iter().find(|file| file.bytes > 0) {
                None => None,
                Some(file) => boundary_sequence(&path, file, &id, false)?.map(|next| next - 1),
            };
            Ok::<_, AppError>((first, last))
        })
        .await
        .map_err(blocking_error)?
    }

    /// The journal's newest record, read from the end of the file only, and
    /// whether the journal ends with a newline (`false`: an interrupted write
    /// left the final record unterminated). Startup uses it to skip the full
    /// scan of settled conversations; it neither validates nor repairs the
    /// rest of the journal, which the first full read does.
    pub async fn journal_tail(
        &self,
        conversation_id: &str,
    ) -> Result<(Option<ConversationEvent>, bool), AppError> {
        let _guard = self.lock(conversation_id).await;
        self.flush_pending_delta_logged(conversation_id).await;
        self.read_last_event(conversation_id).await
    }

    /// Full-scan recovery the digest replaced, plus the history tests check.
    #[cfg(test)]
    pub async fn recover_with_history(
        &self,
        conversation_id: &str,
    ) -> Result<(ConversationManifest, Vec<ConversationEvent>), AppError> {
        {
            let _guard = self.lock(conversation_id).await;
            self.validate_journal_locked(conversation_id).await?;
        }
        let manifest = self.recover(conversation_id).await?;
        let history = self.complete_history(conversation_id).await?;
        Ok((manifest, history))
    }

    /// Bring the manifest in line with the journal after a restart: its
    /// sequence, time and status (an unfinished turn becomes `Interrupted`)
    /// come from the digest, so the journal is never loaded whole.
    pub async fn recover(&self, conversation_id: &str) -> Result<ConversationManifest, AppError> {
        let _guard = self.lock(conversation_id).await;
        self.flush_pending_delta_logged(conversation_id).await;
        let mut manifest = self.get_unlocked(conversation_id).await?;
        self.ensure_digest_locked(conversation_id).await?;
        let (last_sequence, last_time, mut status) = match self.digests.get(conversation_id) {
            Some(cached) => (
                cached.digest.last_sequence(),
                cached.digest.last_time(),
                cached.digest.status_from(super::ConversationStatus::Idle),
            ),
            None => (0, None, super::ConversationStatus::Idle),
        };
        if status == super::ConversationStatus::Running
            || status == super::ConversationStatus::WaitingPermission
        {
            status = super::ConversationStatus::Interrupted;
        }
        let updated_at = last_time.unwrap_or(manifest.updated_at);
        // Most journals already match their manifest. Rewriting it anyway cost
        // two atomic writes with full syncs per conversation on every start.
        let unchanged = manifest.last_sequence == last_sequence
            && manifest.status == status
            && manifest.updated_at == updated_at;
        manifest.last_sequence = last_sequence;
        manifest.status = status;
        manifest.updated_at = updated_at;
        if !unchanged {
            self.persist_manifest_locked(&manifest).await?;
        }
        Ok(manifest)
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
    pub(super) async fn get_unlocked(
        &self,
        conversation_id: &str,
    ) -> Result<ConversationManifest, AppError> {
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
    pub(super) async fn persist_manifest_locked(
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

    /// Validate every record of the journal and repair what is damaged,
    /// then cache a fully validated index. Sealed segments are checked
    /// through their `.idx` (an unusable one triggers segment salvage);
    /// each run of plaintext files is scanned line by line: interior
    /// corruption is rewritten in the damaged files only, with a
    /// `journal.recordLost` placeholder per lost sequence and the damaged
    /// files backed up to `events.corrupt.<ts>.jsonl`; a corrupt tail is
    /// quarantined and cut; a missing final newline is added. Memory is
    /// bounded by offsets, not events. Callers hold the conversation lock.
    async fn validate_journal_locked(&self, conversation_id: &str) -> Result<(), AppError> {
        let directory = self.directory(conversation_id)?;
        for _ in 0..4 {
            let files = self.files_locked(conversation_id, &directory).await?;
            let mut parts = Vec::new();
            let mut expected = 1u64;
            let mut tail: Option<(usize, ConversationEvent)> = None;
            let mut repaired = false;
            let mut position = 0usize;
            while position < files.len() {
                if files[position].is_sealed_segment() {
                    let number = files[position].number().unwrap_or_default();
                    let path = directory.clone();
                    let loaded =
                        tokio::task::spawn_blocking(move || segment::load_index(&path, number))
                            .await
                            .map_err(blocking_error)?;
                    match loaded {
                        Ok(loaded) if loaded.segment.first == expected => {
                            expected = loaded.segment.last + 1;
                            tail = None;
                            parts.push(IndexPart::Sealed(Arc::new(loaded.segment)));
                            position += 1;
                            continue;
                        }
                        Ok(loaded) => {
                            return Err(AppError::InvalidRequest(format!(
                                "conversation {conversation_id} journal segment {number} starts at \
                                 sequence {} where {expected} belongs",
                                loaded.segment.first
                            )));
                        }
                        Err(SegmentError::Io(error))
                            if error.kind() != std::io::ErrorKind::NotFound =>
                        {
                            return Err(error.into());
                        }
                        Err(reason) => {
                            // A missing `.idx` is as unusable as a damaged one.
                            let reason = reason.to_string();
                            tracing::warn!(
                                conversation_id,
                                number,
                                reason,
                                "sealed journal segment index unusable"
                            );
                            self.salvage_segment_locked(conversation_id, &directory, number)
                                .await?;
                            repaired = true;
                            break;
                        }
                    }
                }
                // A run of plaintext files up to the next sealed segment,
                // whose first sequence bounds any loss inside the run.
                let end = files[position..]
                    .iter()
                    .position(JournalFile::is_sealed_segment)
                    .map_or(files.len(), |offset| position + offset);
                let barrier = if end < files.len() {
                    let path = directory.clone();
                    let id = conversation_id.to_owned();
                    let file = files[end].clone();
                    let next = tokio::task::spawn_blocking(move || {
                        boundary_sequence(&path, &file, &id, false)
                    })
                    .await
                    .map_err(blocking_error)??;
                    Some(next.ok_or_else(|| {
                        AppError::InvalidRequest(format!(
                            "conversation {conversation_id} sealed segment after plaintext journal is unreadable"
                        ))
                    })?)
                } else {
                    None
                };
                let path = directory.clone();
                let id = conversation_id.to_owned();
                let run: Vec<(usize, JournalFile)> = (position..end)
                    .map(|index| (index, files[index].clone()))
                    .collect();
                let scan = tokio::task::spawn_blocking(move || {
                    scan_plain_run(&path, &run, &id, expected, barrier)
                })
                .await
                .map_err(blocking_error)??;
                if scan.needs_repair() {
                    self.repair_plain_run(conversation_id, &directory, &files, scan)
                        .await?;
                    repaired = true;
                    break;
                }
                expected = scan.expected;
                if let Some(last) = scan.last {
                    tail = Some(last);
                }
                parts.extend(scan.parts);
                position = end;
            }
            if repaired {
                self.index_remove(conversation_id);
                self.digests.remove(conversation_id);
                self.tails.remove(conversation_id);
                self.follow_repaired_journal_locked(conversation_id).await?;
                continue;
            }
            match tail {
                Some((file, event)) => {
                    self.tails.insert(
                        conversation_id.to_owned(),
                        JournalTail {
                            name: files[file].name.clone(),
                            bytes: files[file].bytes,
                            modified: files[file].modified,
                            event,
                        },
                    );
                }
                None => {
                    self.tails.remove(conversation_id);
                }
            }
            self.index_put(
                conversation_id,
                JournalIndex {
                    files,
                    parts,
                    fully_validated: true,
                },
            );
            return Ok(());
        }
        Err(AppError::Conflict(format!(
            "conversation {conversation_id} journal kept changing while it was repaired"
        )))
    }

    /// Apply the repairs a [`PlainRunScan`] found. Callers hold the
    /// conversation lock.
    async fn repair_plain_run(
        &self,
        conversation_id: &str,
        directory: &Path,
        files: &[JournalFile],
        mut scan: PlainRunScan,
    ) -> Result<(), AppError> {
        if let Some((segment, _)) = scan.corrupt_tail {
            if scan.damaged.is_empty() {
                // Quarantine first: the tail copy must hold the original
                // bytes.
                let (segment, start) = scan.corrupt_tail.expect("tail is present");
                quarantine_tail(directory, files, segment, start).await?;
            } else {
                // The interior backup covers the tail as well: its files
                // are rewritten with their valid records only.
                scan.damaged.extend(
                    (segment..files.len()).filter(|index| !files[*index].is_sealed_segment()),
                );
            }
        }
        let mut rewritten = BTreeSet::new();
        if !scan.damaged.is_empty() {
            let backup = corrupt_copy_path(&directory.join(EVENTS_FILE), "jsonl");
            let backup_name = backup
                .file_name()
                .map(|name| name.to_string_lossy().into_owned())
                .unwrap_or_default();
            // The damaged files, concatenated in journal order, must be
            // durable before any of them is rewritten.
            {
                let copy = tokio::fs::OpenOptions::new()
                    .create_new(true)
                    .write(true)
                    .open(&backup)
                    .await?;
                set_owner_only(&backup, false).await?;
                let mut writer =
                    tokio::io::BufWriter::with_capacity(JOURNAL_SCAN_BUFFER_BYTES, copy);
                for file in &scan.damaged {
                    writer
                        .write_all(&tokio::fs::read(directory.join(&files[*file].name)).await?)
                        .await?;
                }
                writer.flush().await?;
                writer.into_inner().sync_all().await?;
            }
            sync_directory(directory).await?;
            let mut lost_records = 0u64;
            for file in &scan.damaged {
                let bytes = tokio::fs::read(directory.join(&files[*file].name)).await?;
                let mut lines: Vec<Vec<u8>> = Vec::new();
                for entry in scan.entries.iter().filter(|entry| entry.file() == *file) {
                    match entry {
                        SalvagedEntry::Record { start, end, .. } => {
                            lines.push(trim_ascii(&bytes[*start as usize..*end as usize]).to_vec());
                        }
                        SalvagedEntry::Lost {
                            run_start,
                            run_length,
                            time,
                            provider,
                            ..
                        } => {
                            for sequence in *run_start..run_start + run_length {
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
                                placeholder.time =
                                    time.unwrap_or_else(|| truncate_to_micros(Utc::now()));
                                placeholder.provider = *provider;
                                lines.push(encode_record(&placeholder)?);
                            }
                            lost_records += run_length;
                        }
                    }
                }
                replace_journal(&directory.join(&files[*file].name), &lines).await?;
                rewritten.insert(*file);
            }
            tracing::warn!(
                conversation_id,
                lost_records,
                backup = %backup_name,
                "salvaged corrupt conversation journal; lost records replaced with placeholders"
            );
        }
        if let Some((segment, start)) = scan.corrupt_tail {
            // Nothing valid follows these lines, so they are an interrupted
            // write rather than lost history: cut the journal back to its
            // last valid record. A corrupt tail is a suffix, so it may span
            // several trailing files; wholly corrupt sealed ones are
            // deleted, the one containing `start` is truncated (unless the
            // interior rewrite already dropped its tail lines), and an
            // empty active file is kept so the journal still ends in
            // `events.jsonl`.
            for (index, entry) in files.iter().enumerate().skip(segment) {
                if rewritten.contains(&index) || entry.is_sealed_segment() {
                    continue;
                }
                let path = directory.join(&entry.name);
                if index > segment && entry.name != EVENTS_FILE {
                    tokio::fs::remove_file(&path).await?;
                    continue;
                }
                let file = tokio::fs::OpenOptions::new()
                    .write(true)
                    .open(&path)
                    .await?;
                file.set_len(if index == segment { start } else { 0 })
                    .await?;
                file.sync_all().await?;
            }
            sync_directory(directory).await?;
            let quarantined: u64 = files[segment..]
                .iter()
                .map(|file| file.bytes)
                .sum::<u64>()
                .saturating_sub(start);
            tracing::warn!(
                conversation_id,
                quarantined_bytes = quarantined,
                "recovered invalid conversation journal tail"
            );
        } else if let Some(last) = scan.unterminated {
            // A complete record whose trailing newline never reached the
            // disk parses fine, but the next O_APPEND write would glue its
            // record onto it and a later scan would quarantine both.
            terminate_journal(&directory.join(&files[last].name)).await?;
            tracing::warn!(
                conversation_id,
                "terminated conversation journal missing its final newline"
            );
        }
        Ok(())
    }

    /// A repair can leave the journal ending below the cached manifest, which
    /// would make subscribers wait for sequences that no longer exist. Pull
    /// `last_sequence` back to the journal; a manifest that is merely behind
    /// is left to [`Self::append_locked`], which also replays the status.
    /// Callers hold the conversation lock.
    async fn follow_repaired_journal_locked(&self, conversation_id: &str) -> Result<(), AppError> {
        let journal_sequence = self
            .read_last_event(conversation_id)
            .await?
            .0
            .map_or(0, |event| event.sequence);
        let mut manifest = self.get_unlocked(conversation_id).await?;
        if manifest.last_sequence > journal_sequence {
            manifest.last_sequence = journal_sequence;
            self.mark_manifest_dirty_locked(manifest);
        }
        Ok(())
    }

    /// The newest journal record plus whether the last non-empty journal
    /// file ends with a newline. `false` means an interrupted write left
    /// the final record unterminated, so the next append must close that
    /// record first — with an in-line separator when the torn file is the
    /// active one, or by terminating a sealed plaintext file in place. A
    /// journal ending in a sealed `.seg` decodes its last frames.
    async fn read_last_event(
        &self,
        conversation_id: &str,
    ) -> Result<(Option<ConversationEvent>, bool), AppError> {
        let directory = self.directory(conversation_id)?;
        let files = self.files_locked(conversation_id, &directory).await?;
        let Some(last) = files.iter().rev().find(|file| file.bytes > 0) else {
            return Ok((None, true));
        };
        // Every writer that fills the tail cache leaves its file
        // newline-terminated (appends, recovery, the cold scan).
        if let Some(tail) = self.tails.get(conversation_id) {
            if tail.name == last.name && tail.bytes == last.bytes && tail.modified == last.modified
            {
                return Ok((Some(tail.event.clone()), true));
            }
        }
        if last.is_sealed_segment() {
            let number = last.number().unwrap_or_default();
            let path = directory.clone();
            let cache = self.frames.clone();
            let id = conversation_id.to_owned();
            let event = tokio::task::spawn_blocking(move || {
                let loaded = segment::load_index(&path, number)?;
                let mut reader = SegmentReader::open(&path, &loaded.segment, &cache, &id)?;
                reader.event(loaded.segment.last).map(|(event, _)| event)
            })
            .await
            .map_err(blocking_error)?
            .map_err(|error| segment_error(conversation_id, number, error))?;
            self.tails.insert(
                conversation_id.to_owned(),
                JournalTail {
                    name: last.name.clone(),
                    bytes: last.bytes,
                    modified: last.modified,
                    event: event.clone(),
                },
            );
            return Ok((Some(event), true));
        }
        let path = directory.join(&last.name);
        let window = (MAX_EVENT_PAYLOAD_BYTES as u64 + 64 * 1024).min(last.bytes);
        let mut file = tokio::fs::File::open(&path).await?;
        file.seek(std::io::SeekFrom::End(-(window as i64))).await?;
        let mut raw = Vec::with_capacity(window as usize);
        file.read_to_end(&mut raw).await?;
        let terminated = raw.last().is_none_or(|byte| *byte == b'\n');
        if window < last.bytes {
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
        let event = decode_journal_record(line, conversation_id).map_err(|error| {
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

    /// Convert the oldest sealed plaintext files of a conversation (one
    /// file, or several consecutive ones up to [`JOURNAL_SEGMENT_BYTES`]
    /// when migrating 8 MiB v2 files) into one compressed segment named
    /// after the first. The build runs without the conversation lock — the
    /// sources are immutable once sealed — and is committed under it only
    /// if they are unchanged. Returns `false` when nothing was waiting.
    pub(super) async fn seal_next(&self, conversation_id: &str) -> Result<bool, AppError> {
        let directory = self.directory(conversation_id)?;
        let (group, expected_first) = {
            let _guard = self.lock(conversation_id).await;
            if !tokio::fs::try_exists(directory.join(MANIFEST_FILE)).await? {
                return Ok(false);
            }
            let files = self.files_locked(conversation_id, &directory).await?;
            let Some(start) = files
                .iter()
                .position(|file| !file.is_sealed_segment() && file.name != EVENTS_FILE)
            else {
                return Ok(false);
            };
            let mut group = vec![files[start].clone()];
            let mut bytes = files[start].bytes;
            for file in &files[start + 1..] {
                if file.is_sealed_segment()
                    || file.name == EVENTS_FILE
                    || bytes + file.bytes > JOURNAL_SEGMENT_BYTES
                {
                    break;
                }
                bytes += file.bytes;
                group.push(file.clone());
            }
            self.replay_journal(conversation_id).await?;
            let first = self
                .index_get(conversation_id, |index| {
                    index.parts.iter().find_map(|part| match part {
                        IndexPart::Plain { file, first, .. } if index.files[*file] == group[0] => {
                            Some(*first)
                        }
                        _ => None,
                    })
                })
                .flatten();
            match first {
                Some(first) => (group, first),
                // An empty sealed file holds nothing to convert.
                None => {
                    for file in &group {
                        if file.bytes == 0 {
                            remove_if_exists(&directory.join(&file.name)).await?;
                        }
                    }
                    if group.iter().any(|file| file.bytes > 0) {
                        return Err(AppError::InvalidRequest(format!(
                            "conversation {conversation_id} sealed journal file is not indexed"
                        )));
                    }
                    sync_directory(&directory).await?;
                    return Ok(true);
                }
            }
        };
        let number = group[0].number().unwrap_or_default();
        let sources: Vec<String> = group.iter().map(|file| file.name.clone()).collect();
        self.sealing.insert(conversation_id.to_owned(), ());
        let path = directory.clone();
        let id = conversation_id.to_owned();
        let policy = self.seal_policy(conversation_id).await;
        let built = tokio::task::spawn_blocking(move || {
            segment::build_from_plain(&path, &id, number, &sources, expected_first, &policy)
        })
        .await;
        let prepared = match built {
            Ok(Ok(prepared)) => prepared,
            Ok(Err(error)) => {
                self.sealing.remove(conversation_id);
                if let SegmentError::Invalid(reason) = &error {
                    // Damage in a sealed plaintext file: repair it through
                    // the validating scan so the next attempt can convert.
                    tracing::warn!(
                        conversation_id,
                        reason,
                        "sealed journal file failed to convert; validating the journal"
                    );
                    let _guard = self.lock(conversation_id).await;
                    self.validate_journal_locked(conversation_id).await?;
                }
                return Err(segment_error(conversation_id, number, error));
            }
            Err(error) => {
                self.sealing.remove(conversation_id);
                return Err(blocking_error(error));
            }
        };
        let committed = self
            .commit_segment(conversation_id, &directory, &group, &prepared)
            .await;
        self.sealing.remove(conversation_id);
        if !matches!(committed, Ok(true)) {
            prepared.discard();
        }
        if matches!(committed, Ok(true)) && !prepared.repacked.is_empty() {
            // The repacked records now live in sealed frames: their DEKs
            // are no longer needed and are zeroized (§3.2).
            if let Some(keys) = &self.history {
                keys.deks()
                    .release_sealed(conversation_id, &prepared.repacked)
                    .await;
            }
        }
        if committed.is_ok() && self.commit_stop().is_some() {
            // A simulated crash ends the caller like a killed process.
            return Err(AppError::Conflict(
                "simulated crash during segment commit".to_owned(),
            ));
        }
        committed.map(|_| true)
    }

    /// The keys a segment build of `conversation_id` may use: every DEK
    /// still in memory (their records are repacked into sealed frames), and
    /// while encryption is on and the conversation is not known to be fully
    /// encrypted, a fresh key its plaintext records are sealed under (§8).
    /// Without that key plaintext simply stays plaintext; the migration
    /// pass retries it.
    pub(super) async fn seal_policy(&self, conversation_id: &str) -> SealPolicy {
        let Some(keys) = &self.history else {
            return SealPolicy::default();
        };
        let mut policy = SealPolicy {
            keys: keys.deks().keys_snapshot(conversation_id).await,
            migrate: None,
        };
        let encrypted = self
            .get(conversation_id)
            .await
            .is_ok_and(|manifest| manifest.history_encrypted_at.is_some());
        if encrypted || !matches!(self.history_mode(), Ok(HistoryEncryption::E2e)) {
            return policy;
        }
        let fresh = match keys.deks().fresh_key(conversation_id).await {
            Ok(fresh) => fresh,
            Err(error) => {
                tracing::warn!(conversation_id, error = %error, "no key to encrypt plaintext history while sealing; it stays plaintext for now");
                return policy;
            }
        };
        match (fresh, keys.fingerprint()) {
            (Some((kid, key)), Ok(fingerprint)) => {
                policy.migrate = Some(MigrationKey {
                    kid,
                    key: Arc::new(key),
                    fingerprint,
                });
            }
            (None, _) => {}
            (Some(_), Err(error)) => {
                tracing::warn!(conversation_id, error = %error, "history fingerprint key unavailable while sealing; plaintext stays plaintext for now");
            }
        }
        policy
    }

    /// Publish a built segment if its sources are unchanged and update the
    /// caches in place. Returns whether it was committed.
    async fn commit_segment(
        &self,
        conversation_id: &str,
        directory: &Path,
        group: &[JournalFile],
        prepared: &PreparedSegment,
    ) -> Result<bool, AppError> {
        let _guard = self.lock(conversation_id).await;
        let files = self.files_locked(conversation_id, directory).await?;
        if !group.iter().all(|source| files.contains(source)) {
            tracing::debug!(
                conversation_id,
                "journal changed during segment build; discarding it"
            );
            return Ok(false);
        }
        let stop = self.commit_stop();
        let path = directory.to_owned();
        let body_sources = prepared.body.sources.clone();
        let (temp_seg, temp_idx, number) = (
            prepared.temp_seg.clone(),
            prepared.temp_idx.clone(),
            prepared.number,
        );
        let body = prepared.body.clone();
        tokio::task::spawn_blocking(move || {
            let prepared = PreparedSegment {
                number,
                temp_seg,
                temp_idx,
                body,
                repacked: Vec::new(),
            };
            segment::commit_prepared(&path, &prepared, stop)
        })
        .await
        .map_err(blocking_error)?
        .map_err(|error| segment_error(conversation_id, number, error))?;
        if stop.is_some() {
            // A simulated crash: leave the directory as it is and forget
            // everything, like a restarted process would.
            self.forget_journal_caches_locked(conversation_id);
            return Ok(true);
        }
        let new_files = journal_files(directory).await?;
        let sealed = SealedSegment::from_body(number, &prepared.body)
            .map_err(|error| segment_error(conversation_id, number, error))?;
        let sealed = Arc::new(sealed);
        let updated = self.index_update(conversation_id, |index| {
            if index.files != files {
                return false;
            }
            let mut parts = Vec::with_capacity(index.parts.len());
            let mut placed = false;
            for part in index.parts.drain(..) {
                match &part {
                    IndexPart::Plain { file, .. } if body_sources.contains(&files[*file].name) => {
                        if !placed {
                            parts.push(IndexPart::Sealed(sealed.clone()));
                            placed = true;
                        }
                    }
                    _ => parts.push(part),
                }
            }
            // Plain parts refer to files by position; renumber them.
            for part in &mut parts {
                if let IndexPart::Plain { file, .. } = part {
                    let name = &files[*file].name;
                    *file = new_files
                        .iter()
                        .position(|candidate| &candidate.name == name)
                        .unwrap_or(*file);
                }
            }
            index.parts = parts;
            index.files = new_files.clone();
            true
        });
        if updated != Some(true) {
            self.index_remove(conversation_id);
        }
        if let Some(mut cached) = self.digests.get_mut(conversation_id) {
            // Slimming keeps every sequence and the digest is computed from
            // the original records, so it still describes the journal.
            if cached.files == files {
                cached.files = new_files.clone();
            }
        }
        tracing::info!(
            conversation_id,
            segment = number,
            records = prepared.body.last_sequence - prepared.body.first_sequence + 1,
            slimmed = prepared.body.slimmed,
            bytes = prepared.body.seg_bytes,
            "sealed journal segment"
        );
        Ok(true)
    }

    /// The crash-injection point of segment commits (always `None` outside
    /// tests).
    pub(super) fn commit_stop(&self) -> Option<CommitStep> {
        #[cfg(test)]
        return *self
            .commit_stop
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        #[cfg(not(test))]
        None
    }

    /// Forget the rebuildable journal caches of a conversation whose files
    /// were rewritten, and reconcile its directory again on next access.
    /// Callers hold the conversation lock.
    pub(super) fn forget_journal_caches_locked(&self, conversation_id: &str) {
        self.reconciled.remove(conversation_id);
        self.index_remove(conversation_id);
        self.digests.remove(conversation_id);
        self.tails.remove(conversation_id);
    }

    /// Ask the maintenance task to encrypt existing plaintext history now
    /// (history encryption was just enabled, §8). Plaintext outside the
    /// journal is not touched: migration backups keep theirs until they
    /// expire, and filesystem snapshots are out of reach.
    pub fn request_history_migration(&self) {
        tracing::warn!(
            "history encryption enabled: existing plaintext history is re-encrypted in the \
             background; journal-v2-backup/ copies (deleted after 7 days), events.corrupt.* \
             salvage copies and Time Machine / APFS snapshots may still hold old plaintext"
        );
        self.maintenance.request_migration();
    }

    /// Replace the prompt text of `last-request.json` with its MAC (see
    /// [`seal_request_snapshot`]). Callers hold the conversation lock.
    pub(super) async fn seal_last_request_locked(
        &self,
        conversation_id: &str,
    ) -> Result<(), AppError> {
        let path = self.directory(conversation_id)?.join(LAST_REQUEST_FILE);
        if !tokio::fs::try_exists(&path).await? {
            return Ok(());
        }
        let mut saved: Value = read_json(&path, "conversation request snapshot").await?;
        let Some(request) = saved.get_mut("request") else {
            return Ok(());
        };
        if request.get("textMac").is_some() {
            return Ok(());
        }
        let key = self.fingerprint_key()?.ok_or_else(|| {
            AppError::Conflict("history fingerprint key is unavailable".to_owned())
        })?;
        seal_request_snapshot(request, &key);
        write_atomic_json(&path, &saved).await
    }

    /// Seal the active file now, as a rotation would.
    #[cfg(test)]
    pub async fn seal_active_for_tests(&self, conversation_id: &str) -> Result<(), AppError> {
        let _guard = self.lock(conversation_id).await;
        self.seal_active_locked(conversation_id).await
    }

    /// Convert every sealed plaintext file now (what the maintenance task
    /// does in the background). Returns how many segments were sealed.
    #[cfg(test)]
    pub async fn seal_all_for_tests(&self, conversation_id: &str) -> Result<usize, AppError> {
        let mut sealed = 0;
        while self.seal_next(conversation_id).await? {
            sealed += 1;
        }
        Ok(sealed)
    }

    /// Stop segment commits after `step`, simulating a crash.
    #[cfg(test)]
    pub(super) fn set_commit_stop(&self, step: Option<CommitStep>) {
        *self
            .commit_stop
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = step;
    }

    /// Seal the active file now (as a rotation would) when it holds any
    /// record, so migration can convert it. Callers hold the lock.
    pub(super) async fn seal_active_locked(&self, conversation_id: &str) -> Result<(), AppError> {
        let directory = self.directory(conversation_id)?;
        let files = self.files_locked(conversation_id, &directory).await?;
        if !files
            .last()
            .is_some_and(|file| file.name == EVENTS_FILE && file.bytes > 0)
        {
            return Ok(());
        }
        let terminated = self.read_last_event(conversation_id).await?.1;
        let previous = files.clone();
        let mut files = files;
        let (sealed, fresh) = rotate_journal(&directory, &files, terminated).await?;
        *files.last_mut().expect("active segment is present") = sealed;
        files.push(fresh);
        self.index_update(conversation_id, |index| {
            if index.files == previous {
                index.files = files.clone();
            }
        });
        if let Some(mut cached) = self.digests.get_mut(conversation_id) {
            if cached.files == previous {
                cached.files = files.clone();
            }
        }
        self.tails.remove(conversation_id);
        if let Some(keys) = &self.history {
            keys.deks().rotate(conversation_id).await;
        }
        Ok(())
    }

    pub(super) fn directory(&self, conversation_id: &str) -> Result<PathBuf, AppError> {
        validate_id(conversation_id)?;
        Ok(self.root.join(conversation_id))
    }

    pub(super) async fn lock(&self, conversation_id: &str) -> OwnedMutexGuard<()> {
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

/// History encryption for a `last-request.json` `request` (a serialized
/// prompt): the text and the inline content items (`text`, `image`) are
/// replaced by `textMac` / `contentMac` — enough to recognize the same
/// request again and to check the text a retry supplies — and only file
/// references stay. Idempotent.
pub fn seal_request_snapshot(request: &mut Value, key: &FingerprintKey) {
    let Some(map) = request.as_object_mut() else {
        return;
    };
    if map.contains_key("textMac") {
        return;
    }
    let text = map
        .get("text")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_owned();
    map.insert("textMac".to_owned(), Value::String(key.mac(&text)));
    map.insert("text".to_owned(), Value::String(String::new()));
    let items = match map.remove("content") {
        Some(Value::Array(items)) => items,
        _ => Vec::new(),
    };
    let (inline, kept): (Vec<Value>, Vec<Value>) = items.into_iter().partition(|item| {
        matches!(
            item.get("type").and_then(Value::as_str),
            Some("text" | "image")
        )
    });
    if !inline.is_empty() {
        let mac = key.mac(&Value::Array(inline).to_string());
        map.insert("contentMac".to_owned(), Value::String(mac));
    }
    map.insert("content".to_owned(), Value::Array(kept));
}

/// Drop the sub-microsecond part v3 records do not store.
fn truncate_to_micros(time: DateTime<Utc>) -> DateTime<Utc> {
    DateTime::from_timestamp_micros(time.timestamp_micros()).unwrap_or(time)
}

async fn remove_if_exists(path: &Path) -> Result<(), AppError> {
    match tokio::fs::remove_file(path).await {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error.into()),
    }
}

/// `events.NNNNNN.jsonl` → its segment number; anything else → `None`.
fn sealed_segment_number(name: &str) -> Option<u64> {
    segment::numbered(name, ".jsonl")
}

/// Ordered journal files of a conversation directory: sealed segments
/// (`events.NNNNNN.seg`, or `events.NNNNNN.jsonl` awaiting conversion) by
/// number followed by the active `events.jsonl` when present. Everything
/// else — `.idx` siblings, `events.corrupt.*` backups, manifests,
/// temporaries — is ignored. The list is the journal's identity: any
/// rename, append, conversion or truncation changes it.
pub(super) async fn journal_files(directory: &Path) -> Result<Vec<JournalFile>, AppError> {
    let mut sealed: Vec<((u64, bool), JournalFile)> = Vec::new();
    let mut active = None;
    let mut entries = match tokio::fs::read_dir(directory).await {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(error.into()),
    };
    while let Some(entry) = entries.next_entry().await? {
        let name = entry.file_name().to_string_lossy().into_owned();
        let is_active = name == EVENTS_FILE;
        let key = if is_active {
            None
        } else if let Some(number) = sealed_segment_number(&name) {
            Some((number, false))
        } else {
            segment::numbered(&name, ".seg").map(|number| (number, true))
        };
        if !is_active && key.is_none() {
            continue;
        }
        let metadata = entry.metadata().await?;
        if !metadata.is_file() {
            continue;
        }
        let file = JournalFile {
            name,
            bytes: metadata.len(),
            modified: metadata.modified().ok(),
        };
        match key {
            None => active = Some(file),
            Some(key) => sealed.push((key, file)),
        }
    }
    sealed.sort_by_key(|(key, _)| *key);
    let mut files: Vec<JournalFile> = sealed.into_iter().map(|(_, file)| file).collect();
    files.extend(active);
    Ok(files)
}

/// Seal the active `events.jsonl` under the next segment number and create
/// a fresh empty active file. `terminated` reports whether the active file
/// ends with a newline; a torn final record is terminated before sealing so
/// no record ever straddles a file boundary. Returns the sealed and fresh
/// active file metadata. Callers hold the conversation lock.
async fn rotate_journal(
    directory: &Path,
    files: &[JournalFile],
    terminated: bool,
) -> Result<(JournalFile, JournalFile), AppError> {
    let active_path = directory.join(EVENTS_FILE);
    if !terminated {
        terminate_journal(&active_path).await?;
    }
    let next = files
        .iter()
        .filter_map(JournalFile::number)
        .max()
        .unwrap_or(0)
        + 1;
    let sealed_name = format!("events.{next:06}.jsonl");
    let sealed_path = directory.join(&sealed_name);
    tokio::fs::rename(&active_path, &sealed_path).await?;
    // A crash between the rename and this create leaves the journal ending
    // in a sealed file; the next append recreates `events.jsonl`.
    let fresh = tokio::fs::OpenOptions::new()
        .create_new(true)
        .write(true)
        .open(&active_path)
        .await?;
    fresh.sync_all().await?;
    drop(fresh);
    set_owner_only(&active_path, false).await?;
    sync_directory(directory).await?;
    let sealed_metadata = tokio::fs::metadata(&sealed_path).await?;
    let fresh_metadata = tokio::fs::metadata(&active_path).await?;
    Ok((
        JournalFile {
            name: sealed_name,
            bytes: sealed_metadata.len(),
            modified: sealed_metadata.modified().ok(),
        },
        JournalFile {
            name: EVENTS_FILE.to_owned(),
            bytes: fresh_metadata.len(),
            modified: fresh_metadata.modified().ok(),
        },
    ))
}

/// A conversation directory being written before it is published.
struct ConversationDraft {
    temporary: PathBuf,
    writer: PlainJournalWriter,
}

/// Writes v3 records into a new conversation's temporary directory,
/// rotating plaintext files at [`JOURNAL_SEGMENT_BYTES`] like appends do.
pub(super) struct PlainJournalWriter {
    directory: PathBuf,
    writer: tokio::io::BufWriter<tokio::fs::File>,
    bytes: u64,
    sealed: u64,
}

impl PlainJournalWriter {
    async fn create(directory: &Path) -> Result<Self, AppError> {
        let path = directory.join(EVENTS_FILE);
        let file = tokio::fs::OpenOptions::new()
            .create_new(true)
            .write(true)
            .open(&path)
            .await?;
        set_owner_only(&path, false).await?;
        Ok(Self {
            directory: directory.to_owned(),
            writer: tokio::io::BufWriter::with_capacity(JOURNAL_SCAN_BUFFER_BYTES, file),
            bytes: 0,
            sealed: 0,
        })
    }

    /// Whether any record was written.
    fn wrote_records(&self) -> bool {
        self.bytes > 0 || self.sealed > 0
    }

    async fn write(&mut self, event: &ConversationEvent) -> Result<(), AppError> {
        if self.bytes >= JOURNAL_SEGMENT_BYTES {
            self.seal().await?;
        }
        let mut line = encode_record(event)?;
        line.push(b'\n');
        self.writer.write_all(&line).await?;
        self.bytes += line.len() as u64;
        Ok(())
    }

    async fn sync(&mut self) -> Result<(), AppError> {
        self.writer.flush().await?;
        self.writer.get_mut().sync_all().await?;
        Ok(())
    }

    async fn seal(&mut self) -> Result<(), AppError> {
        self.sync().await?;
        self.sealed += 1;
        let active = self.directory.join(EVENTS_FILE);
        tokio::fs::rename(
            &active,
            self.directory
                .join(format!("events.{:06}.jsonl", self.sealed)),
        )
        .await?;
        let file = tokio::fs::OpenOptions::new()
            .create_new(true)
            .write(true)
            .open(&active)
            .await?;
        set_owner_only(&active, false).await?;
        self.writer = tokio::io::BufWriter::with_capacity(JOURNAL_SCAN_BUFFER_BYTES, file);
        self.bytes = 0;
        Ok(())
    }

    /// Make everything durable; returns how many files were sealed.
    async fn finish(mut self) -> Result<u64, AppError> {
        self.sync().await?;
        Ok(self.sealed)
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
    if matches!(
        event.event_type.as_str(),
        JOURNAL_RECORD_LOST_EVENT | JOURNAL_COMPACTED_EVENT
    ) {
        return Ok(());
    }
    validate_event_type(&event.event_type)
}

fn validate_event_type(event_type: &str) -> Result<(), AppError> {
    // `journal.` is the store's internal namespace: markers it writes itself
    // (record-loss placeholders, compaction markers) must not be appendable.
    let valid = !event_type.is_empty()
        && event_type.len() <= 96
        && !event_type.starts_with("journal.")
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

/// Keep the sealed frames a fork's copied records refer to in its
/// [`COPIED_FRAMES_DIR`].
async fn write_copied_frames(
    directory: &Path,
    frames: &serde_json::Map<String, Value>,
) -> Result<(), AppError> {
    if frames.is_empty() {
        return Ok(());
    }
    let target = directory.join(COPIED_FRAMES_DIR);
    tokio::fs::create_dir_all(&target).await?;
    set_owner_only(&target, true).await?;
    for (id, frame) in frames {
        if !valid_frame_id(id) {
            return Err(AppError::InvalidRequest(format!(
                "invalid history frame id {id}"
            )));
        }
        write_atomic_json(&target.join(format!("{id}.json")), frame).await?;
    }
    Ok(())
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

/// Quarantine the journal tail starting at `start` bytes into
/// `files[segment]` and covering every later plaintext file. The copy is
/// written as one concatenated stream so a tail spanning several files
/// lands in a single backup.
async fn quarantine_tail(
    directory: &Path,
    files: &[JournalFile],
    segment: usize,
    start: u64,
) -> Result<(), AppError> {
    let copy_path = corrupt_copy_path(&directory.join(EVENTS_FILE), "jsonl");
    let copy = tokio::fs::OpenOptions::new()
        .create_new(true)
        .write(true)
        .open(&copy_path)
        .await?;
    set_owner_only(&copy_path, false).await?;
    let mut writer = tokio::io::BufWriter::with_capacity(JOURNAL_SCAN_BUFFER_BYTES, copy);
    for (index, file) in files.iter().enumerate().skip(segment) {
        if file.is_sealed_segment() {
            continue;
        }
        match tokio::fs::read(directory.join(&file.name)).await {
            Ok(bytes) => {
                let offset = if index == segment { start as usize } else { 0 };
                writer.write_all(&bytes[offset.min(bytes.len())..]).await?;
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error.into()),
        }
    }
    writer.flush().await?;
    writer.into_inner().sync_all().await?;
    Ok(())
}

/// `events.corrupt.<UTC timestamp>.<extension>` next to the journal: where
/// damaged journal bytes are kept before recovery cuts or rewrites them.
fn corrupt_copy_path(path: &Path, extension: &str) -> PathBuf {
    path.with_file_name(format!(
        "events.corrupt.{}.{extension}",
        Utc::now().format("%Y%m%dT%H%M%S%.6fZ")
    ))
}

/// One entry of a salvaged plaintext run, in sequence order.
enum SalvagedEntry {
    /// A valid record and its line range inside `file` — `end` excludes
    /// the newline, matching `line_ranges`.
    Record {
        file: usize,
        sequence: u64,
        start: u64,
        end: u64,
    },
    /// Sequences `run_start..run_start + run_length` that no valid line
    /// holds, placed in `file` (where the corrupt lines began) and stamped
    /// with the previous valid record's time and provider.
    Lost {
        file: usize,
        run_start: u64,
        run_length: u64,
        time: Option<DateTime<Utc>>,
        provider: Option<super::ProviderKind>,
    },
}

impl SalvagedEntry {
    fn file(&self) -> usize {
        match self {
            Self::Record { file, .. } | Self::Lost { file, .. } => *file,
        }
    }
}

/// Corrupt lines since the last valid record.
struct PendingCorrupt {
    /// File and byte offset of the run's first line.
    file: usize,
    start: u64,
    lines: u64,
    bytes: u64,
    files: BTreeSet<usize>,
}

/// Result of [`scan_plain_run`]: the run's index parts when it is clean,
/// otherwise everything [`ConversationStore::repair_plain_run`] needs.
struct PlainRunScan {
    parts: Vec<IndexPart>,
    /// Next sequence after the run.
    expected: u64,
    /// The run's newest record and the file holding it.
    last: Option<(usize, ConversationEvent)>,
    entries: Vec<SalvagedEntry>,
    /// Files that must be rewritten: they hold interior corrupt lines or
    /// receive placeholders.
    damaged: BTreeSet<usize>,
    /// `(file, byte offset)` where corrupt lines no valid record follows
    /// begin; everything from there to the journal end is a tail.
    corrupt_tail: Option<(usize, u64)>,
    /// The last non-empty file lacks its final newline.
    unterminated: Option<usize>,
}

impl PlainRunScan {
    fn needs_repair(&self) -> bool {
        !self.damaged.is_empty() || self.corrupt_tail.is_some() || self.unterminated.is_some()
    }
}

/// Validate a run of plaintext journal files line by line, file by file, so
/// a journal of any size is checked without concatenating its files. A line
/// is corrupt when it does not parse as one record of this conversation
/// (garbage, a torn write, two records glued onto one line) or its sequence
/// does not continue the previous valid record. Each corrupt run is bounded
/// by the valid records around it: after valid sequence `p`, the next valid
/// record `q > p` resumes the journal and `p + 1..q` are lost. `q` is only
/// accepted while the lost count fits in the corrupt lines seen (one record
/// per line plus one per [`MIN_JOURNAL_RECORD_BYTES`]); otherwise the record
/// is itself treated as corrupt, so a damaged sequence number cannot swallow
/// the rest of the journal. A sealed segment after the run (`barrier`, its
/// first sequence) ends the run like a valid record would; without one,
/// corrupt lines no valid record follows are a tail. Blocking.
fn scan_plain_run(
    directory: &Path,
    run: &[(usize, JournalFile)],
    conversation_id: &str,
    expected_first: u64,
    barrier: Option<u64>,
) -> Result<PlainRunScan, AppError> {
    let mut expected = expected_first;
    let mut entries = Vec::new();
    let mut damaged = BTreeSet::new();
    let mut pending: Option<PendingCorrupt> = None;
    let mut last: Option<(usize, ConversationEvent)> = None;
    let mut unterminated = None;
    let mut present = Vec::new();
    for (file, meta) in run {
        let bytes = match std::fs::read(directory.join(&meta.name)) {
            Ok(bytes) => bytes,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
            Err(error) => return Err(error.into()),
        };
        present.push(*file);
        if !bytes.is_empty() {
            unterminated = (bytes.last() != Some(&b'\n')).then_some(*file);
        }
        for (start, end) in line_ranges(&bytes) {
            let line = trim_ascii(&bytes[start..end]);
            if line.is_empty() {
                continue;
            }
            let record = decode_journal_record(line, conversation_id)
                .ok()
                .filter(|event| {
                    event.sequence >= expected
                        && barrier.is_none_or(|barrier| event.sequence < barrier)
                        && validate_event(event, conversation_id, event.sequence).is_ok()
                });
            if let Some(event) = record {
                let lost = event.sequence - expected;
                let capacity = pending.as_ref().map_or(0, |pending| {
                    pending
                        .lines
                        .saturating_add(pending.bytes / MIN_JOURNAL_RECORD_BYTES as u64)
                });
                if lost <= capacity {
                    if let Some(corrupt) = pending.take() {
                        damaged.extend(corrupt.files);
                        if lost > 0 {
                            entries.push(SalvagedEntry::Lost {
                                file: corrupt.file,
                                run_start: expected,
                                run_length: lost,
                                time: last.as_ref().map(|(_, event)| event.time),
                                provider: last.as_ref().and_then(|(_, event)| event.provider),
                            });
                        }
                    }
                    expected = event.sequence + 1;
                    entries.push(SalvagedEntry::Record {
                        file: *file,
                        sequence: event.sequence,
                        start: start as u64,
                        end: end as u64,
                    });
                    last = Some((*file, event));
                    continue;
                }
            }
            let corrupt = pending.get_or_insert(PendingCorrupt {
                file: *file,
                start: start as u64,
                lines: 0,
                bytes: 0,
                files: BTreeSet::new(),
            });
            corrupt.lines += 1;
            corrupt.bytes += (end - start + 1) as u64;
            corrupt.files.insert(*file);
        }
    }
    let mut corrupt_tail = None;
    match barrier {
        Some(barrier) => {
            if expected > barrier {
                return Err(AppError::InvalidRequest(format!(
                    "conversation {conversation_id} journal overlaps its sealed segment at sequence {barrier}"
                )));
            }
            // The sealed segment fixes where the run must end; anything
            // missing before it is lost interior history.
            if expected < barrier || pending.is_some() {
                let place = pending
                    .as_ref()
                    .map(|corrupt| corrupt.file)
                    .or(present.last().copied());
                if let Some(corrupt) = pending.take() {
                    damaged.extend(corrupt.files);
                }
                if let (Some(file), true) = (place, expected < barrier) {
                    damaged.insert(file);
                    entries.push(SalvagedEntry::Lost {
                        file,
                        run_start: expected,
                        run_length: barrier - expected,
                        time: last.as_ref().map(|(_, event)| event.time),
                        provider: last.as_ref().and_then(|(_, event)| event.provider),
                    });
                    expected = barrier;
                }
            }
        }
        None => corrupt_tail = pending.map(|corrupt| (corrupt.file, corrupt.start)),
    }
    let mut parts = Vec::new();
    for entry in &entries {
        if let SalvagedEntry::Record {
            file,
            sequence,
            start,
            end,
        } = entry
        {
            match parts.last_mut() {
                Some(IndexPart::Plain {
                    file: part_file,
                    offsets,
                    ..
                }) if part_file == file => offsets.push((*start, *end)),
                _ => parts.push(IndexPart::Plain {
                    file: *file,
                    first: *sequence,
                    offsets: vec![(*start, *end)],
                }),
            }
        }
    }
    Ok(PlainRunScan {
        parts,
        expected,
        last,
        entries,
        damaged,
        corrupt_tail,
        unterminated,
    })
}

/// First (or, with `last`, newest) sequence a journal file holds; `None`
/// when it holds no readable boundary record. Blocking.
fn boundary_sequence(
    directory: &Path,
    file: &JournalFile,
    conversation_id: &str,
    last: bool,
) -> Result<Option<u64>, AppError> {
    if file.is_sealed_segment() {
        return Ok(
            segment::load_index(directory, file.number().unwrap_or_default())
                .ok()
                .map(|loaded| {
                    if last {
                        loaded.segment.last
                    } else {
                        loaded.segment.first
                    }
                }),
        );
    }
    let bytes = match std::fs::read(directory.join(&file.name)) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error.into()),
    };
    let ranges = line_ranges(&bytes);
    let decode = |(start, end): &(usize, usize)| {
        decode_journal_record(trim_ascii(&bytes[*start..*end]), conversation_id)
            .ok()
            .map(|event| event.sequence)
    };
    Ok(if last {
        ranges.iter().rev().find_map(decode)
    } else {
        ranges.iter().find_map(decode)
    })
}

type FastScan = Option<(Vec<IndexPart>, Option<(usize, ConversationEvent)>)>;

/// Blocking cold index build. Sealed segments contribute their `.idx`
/// (checksummed, never their `.seg` bytes). Plaintext files get a newline
/// scan in which only each file's first and last records are parsed: a
/// file's first record must carry the running sequence and its last must
/// carry `first + lines − 1`. Returns `None` whenever the journal differs
/// from what appends and conversions produce — no records at all, a missing
/// final newline, an unusable `.idx`, or a boundary record that fails to
/// parse or validate. Callers then run the full validating scan, which owns
/// repairs and corruption reporting.
fn scan_journal_fast(
    directory: &Path,
    files: &[JournalFile],
    conversation_id: &str,
) -> Result<FastScan, AppError> {
    use std::io::{BufRead, Read, Seek, SeekFrom};

    let mut parts = Vec::new();
    let mut expected = 1u64;
    let mut last = None;
    for (file_index, meta) in files.iter().enumerate() {
        if meta.is_sealed_segment() {
            let loaded = match segment::load_index(directory, meta.number().unwrap_or_default()) {
                Ok(loaded) => loaded,
                Err(SegmentError::Io(error)) if error.kind() != std::io::ErrorKind::NotFound => {
                    return Err(error.into());
                }
                Err(_) => return Ok(None),
            };
            if loaded.segment.first != expected {
                return Ok(None);
            }
            expected = loaded.segment.last + 1;
            // The tail of a journal ending in a `.seg` is decoded on demand.
            last = None;
            parts.push(IndexPart::Sealed(Arc::new(loaded.segment)));
            continue;
        }
        let path = directory.join(&meta.name);
        let mut file = match std::fs::File::open(&path) {
            Ok(file) => file,
            // A file that vanished since enumeration is not corruption.
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
            Err(error) => return Err(error.into()),
        };
        let bytes = file.metadata()?.len();
        if bytes == 0 {
            continue;
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
                Ok(decode_journal_record(&line, conversation_id).ok())
            };
        let count = offsets.len() as u64;
        let first_valid = read_record(offsets[0])?
            .is_some_and(|event| validate_event(&event, conversation_id, expected).is_ok());
        if !first_valid {
            return Ok(None);
        }
        let Some(file_last) = read_record(offsets[offsets.len() - 1])?
            .filter(|event| validate_event(event, conversation_id, expected + count - 1).is_ok())
        else {
            return Ok(None);
        };
        parts.push(IndexPart::Plain {
            file: file_index,
            first: expected,
            offsets,
        });
        expected += count;
        last = Some((file_index, file_last));
    }
    if parts.is_empty() {
        return Ok(None);
    }
    Ok(Some((parts, last)))
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

/// Which end of a replay window the byte budget keeps.
#[derive(Clone, Copy)]
enum PageAnchor {
    /// Forward replay: keep the oldest records, drop newer ones.
    Start,
    /// Reverse replay: keep the newest records, drop older ones.
    End,
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
#[path = "store_v3_tests.rs"]
mod v3_tests;

#[cfg(test)]
#[path = "store_v3_bench.rs"]
mod v3_bench;

#[cfg(test)]
#[path = "store_e2e_tests.rs"]
mod e2e_tests;

#[cfg(test)]
mod tests {
    use std::fs;
    use std::time::Duration;

    use serde_json::json;
    use tokio::io::AsyncWriteExt;

    use super::*;
    use crate::conversation::{ConversationStatus, ProviderKind};

    impl ConversationStore {
        /// A copy of the cached replay index; panics when none is cached.
        fn test_index(&self, conversation_id: &str) -> JournalIndex {
            self.index_get(conversation_id, Clone::clone)
                .expect("replay index is cached")
        }
    }

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
            store.index_cache().clear();
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
                let index = store.test_index(&manifest.id);
                index.total() * std::mem::size_of::<(u64, u64)>()
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
        assert_eq!(store.test_index(&conversation.id).total(), 1202);
        store.index_cache().clear();
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
    fn page_budget_keeps_one_record_even_above_the_budget() {
        let mut budget = PageBudget::new(10);
        assert!(budget.admit(100));
        assert!(!budget.admit(1));
        let mut budget = PageBudget::new(300);
        assert!(budget.admit(100) && budget.admit(200));
        assert!(!budget.admit(50));
        let mut budget = PageBudget::new(0);
        assert!(budget.admit(0) && budget.admit(0));
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
            let index = store.test_index(&id);
            assert!(
                !index.fully_validated,
                "clean journal must use the fast scan"
            );
            assert_eq!(index.total(), 120);
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
            let index = store.test_index(&id);
            assert!(!index.fully_validated);
            assert_eq!(index.total(), 121);
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
        assert!(store.test_index(&id).fully_validated);
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
        assert!(store.test_index(&id).fully_validated);
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
        assert!(store.test_index(&id).fully_validated);
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
        let second = decode_journal_record(lines[1].as_bytes(), &id).unwrap();
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

    /// Peak resident set size of this process in bytes.
    fn peak_rss_bytes() -> u64 {
        let mut usage: libc::rusage = unsafe { std::mem::zeroed() };
        // SAFETY: `getrusage` only writes the struct it is handed.
        unsafe { libc::getrusage(libc::RUSAGE_SELF, &mut usage) };
        let max = usage.ru_maxrss as u64;
        if cfg!(target_os = "macos") {
            max
        } else {
            max * 1024
        }
    }

    /// Idempotency lookups on a 200k-event journal: the full scans the
    /// digest replaced against a cold digest build and warm digest lookups
    /// plus point reads. Peak RSS only grows, so the digest is measured
    /// first. Run alone:
    /// `cargo test -- --ignored measure_digest_lookups_200k --nocapture`.
    #[tokio::test]
    #[ignore = "opt-in 200k-event digest lookup measurement"]
    async fn measure_digest_lookups_200k() {
        use std::io::Write;
        const COUNT: u64 = 200_000;
        let root = temp_dir("todex-digest-measurement");
        let store = ConversationStore::new(root.clone()).await.unwrap();
        let id = store
            .create(ConversationManifest::new(
                ProviderKind::Codex,
                root.clone(),
                None,
                None,
            ))
            .await
            .unwrap()
            .id;
        let path = root.join("conversations").join(&id).join(EVENTS_FILE);
        let mut file = std::io::BufWriter::new(fs::File::create(&path).unwrap());
        for sequence in 1..=COUNT {
            let turn = format!("turn-{}", sequence / 50);
            let (event_type, payload) = match sequence % 50 {
                1 => (
                    "message.created",
                    json!({ "role": "user", "turnId": turn, "content": "question ".repeat(20),
                        "clientRequestId": format!("request-{}", sequence / 50),
                        "requestFingerprint": "f" }),
                ),
                2 => ("turn.started", json!({ "turnId": turn })),
                3 => (
                    "control.requested",
                    json!({ "turnId": turn, "requestId": format!("control-{}", sequence / 50),
                        "control": { "action": "setModel", "model": "m" }, "status": "pending" }),
                ),
                4 => (
                    "control.completed",
                    json!({ "turnId": turn, "requestId": format!("control-{}", sequence / 50),
                        "result": {} }),
                ),
                0 => ("turn.completed", json!({ "turnId": turn })),
                _ => ("message.delta", json!({ "turnId": turn, "delta": "d" })),
            };
            serde_json::to_writer(
                &mut file,
                &ConversationEvent::new(&id, sequence, event_type, payload),
            )
            .unwrap();
            file.write_all(b"\n").unwrap();
        }
        file.flush().unwrap();
        drop(file);
        let journal_mib = fs::metadata(&path).unwrap().len() as f64 / (1024. * 1024.);
        let probe = "request-1";
        let control = "control-1";
        let ms = |start: std::time::Instant| start.elapsed().as_secs_f64() * 1000.;

        let store = ConversationStore::new(root.clone()).await.unwrap();
        let rss_before = peak_rss_bytes();
        let start = std::time::Instant::now();
        let first = store
            .digest(&id, |digest| {
                digest.client_request(probe).map(|facts| facts.first)
            })
            .await
            .unwrap()
            .unwrap();
        let cold_build_ms = ms(start);
        let rss_digest = peak_rss_bytes();
        const ROUNDS: u32 = 200;
        let start = std::time::Instant::now();
        for _ in 0..ROUNDS {
            let first = store
                .digest(&id, |digest| {
                    digest.client_request(probe).map(|facts| facts.first)
                })
                .await
                .unwrap()
                .unwrap();
            assert_eq!(
                store.event_at(&id, first).await.unwrap().unwrap().sequence,
                51
            );
        }
        let warm_prompt_ms = ms(start) / f64::from(ROUNDS);
        let start = std::time::Instant::now();
        for _ in 0..ROUNDS {
            let facts = store
                .digest(&id, |digest| digest.control(control).cloned())
                .await
                .unwrap()
                .unwrap();
            store.event_at(&id, facts.requested.unwrap()).await.unwrap();
            store.event_at(&id, facts.outcome.unwrap().1).await.unwrap();
        }
        let warm_control_ms = ms(start) / f64::from(ROUNDS);
        // An append folds into the digest; the next lookup stays warm.
        let start = std::time::Instant::now();
        for _ in 0..ROUNDS {
            store
                .append(&id, "message.delta", json!({ "turnId": "t", "delta": "x" }))
                .await
                .unwrap();
            store
                .digest(&id, |digest| digest.control(control).cloned())
                .await
                .unwrap();
        }
        let append_and_lookup_ms = ms(start) / f64::from(ROUNDS);
        drop(store);

        let store = ConversationStore::new(root.clone()).await.unwrap();
        let start = std::time::Instant::now();
        let history = store.complete_history(&id).await.unwrap();
        let previous = history
            .iter()
            .find(|event| event.payload.get("clientRequestId") == Some(&json!(probe)))
            .unwrap();
        assert_eq!(previous.sequence, first);
        let full_scan_ms = ms(start);
        let rss_full = peak_rss_bytes();
        drop(history);
        let start = std::time::Instant::now();
        let history = store.complete_history(&id).await.unwrap();
        assert!(history.iter().any(|event| {
            event.event_type == "control.requested" && event.payload["requestId"] == control
        }));
        let full_scan_warm_ms = ms(start);
        eprintln!(
            "digest_measurement events={COUNT} journal_mib={journal_mib:.1} \
             full_scan_lookup_cold_ms={full_scan_ms:.1} full_scan_lookup_warm_ms={full_scan_warm_ms:.1} \
             digest_cold_build_ms={cold_build_ms:.1} digest_prompt_lookup_ms={warm_prompt_ms:.3} \
             digest_control_lookup_ms={warm_control_ms:.3} append_plus_lookup_ms={append_and_lookup_ms:.3} \
             peak_rss_mib_start={:.1} peak_rss_mib_after_digest_build={:.1} peak_rss_mib_after_full_scan={:.1}",
            rss_before as f64 / 1048576.,
            rss_digest as f64 / 1048576.,
            rss_full as f64 / 1048576.,
        );
        fs::remove_dir_all(root).unwrap();
    }

    #[tokio::test]
    async fn digest_follows_appends_rotation_deltas_and_restarts() {
        use crate::conversation::digest::tests::{assert_matches_reference, varied_payloads};
        let root = temp_dir("todex-digest-appends");
        let store = ConversationStore::new(root.clone()).await.unwrap();
        let hub = ConversationEventHub::default();
        let id = store
            .create(ConversationManifest::new(
                ProviderKind::Codex,
                root.clone(),
                None,
                None,
            ))
            .await
            .unwrap()
            .id;
        // Built while the journal is still empty, then only folded forward.
        let empty = store.digest(&id, Clone::clone).await.unwrap();
        assert_eq!(empty.last_sequence(), 0);
        for (index, (event_type, mut payload)) in varied_payloads(7, 600).into_iter().enumerate() {
            // Padding rolls the journal across several test segments.
            payload["pad"] = json!("p".repeat(400));
            // Appends only take lower-case types; legacy journals hold the
            // camel-case alias, which the in-memory equivalence test covers.
            let event_type = match event_type.as_str() {
                "tool.awaitingApproval" => "permission.requested".to_owned(),
                _ => event_type,
            };
            store.append(&id, event_type, payload).await.unwrap();
            if index % 40 == 0 {
                // A coalesced fragment reaches the digest through its flush.
                delta(&store, &hub, &id, "message.delta", "b", "text").await;
            }
            if index % 75 == 0 {
                let digest = store.digest(&id, Clone::clone).await.unwrap();
                assert_matches_reference(&digest, &store.complete_history(&id).await.unwrap());
            }
        }
        let directory = root.join("conversations").join(&id);
        assert!(journal_files(&directory).await.unwrap().len() > 2);
        // Appends (rotation included) kept the cached digest current.
        assert!(store.digests.contains_key(&id));
        let live = store.digest(&id, Clone::clone).await.unwrap();
        let history = store.complete_history(&id).await.unwrap();
        assert_matches_reference(&live, &history);

        // A restarted store pages through the segments to the same digest.
        let restarted = ConversationStore::new(root.clone()).await.unwrap();
        assert_eq!(restarted.digest(&id, Clone::clone).await.unwrap(), live);
        let sequence = live.last_user_message().unwrap();
        let point = restarted.event_at(&id, sequence).await.unwrap().unwrap();
        assert_eq!(point.event_id, history[sequence as usize - 1].event_id);
        assert_eq!(point.payload, history[sequence as usize - 1].payload);
        assert!(restarted.event_at(&id, 0).await.unwrap().is_none());
        assert!(restarted
            .event_at(&id, live.last_sequence() + 1)
            .await
            .unwrap()
            .is_none());
        fs::remove_dir_all(root).unwrap();
    }

    #[tokio::test]
    async fn digest_is_rebuilt_after_a_salvage_rewrite() {
        use crate::conversation::digest::tests::assert_matches_reference;
        let (root, id, path) = seed_journal("todex-digest-salvage", 6).await;
        let store = ConversationStore::new(root.clone()).await.unwrap();
        assert_eq!(
            store
                .digest(&id, |digest| digest.last_user_message())
                .await
                .unwrap(),
            Some(6)
        );
        let mut lines = fs::read_to_string(&path)
            .unwrap()
            .lines()
            .map(str::to_owned)
            .collect::<Vec<_>>();
        lines[5] = "\u{0}garbage".to_owned();
        lines.push(
            serde_json::to_string(&ConversationEvent::new(
                &id,
                7,
                "turn.completed",
                json!({ "turnId": "t" }),
            ))
            .unwrap(),
        );
        write_lines(&path, &lines);
        // The changed files make the cached digest stale; the rebuild meets
        // the damaged record, salvages it and starts over on the rewrite.
        let digest = store.digest(&id, Clone::clone).await.unwrap();
        assert_eq!(corrupt_copies(&path).len(), 1);
        let history = store.complete_history(&id).await.unwrap();
        assert_eq!(history[5].event_type, JOURNAL_RECORD_LOST_EVENT);
        assert_matches_reference(&digest, &history);
        assert_eq!(digest.last_user_message(), Some(5));
        assert_eq!(digest.has_turn_terminal("t", "turn.completed"), Some(true));
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
        let first = decode_journal_record(lines[0].as_bytes(), &id).unwrap();
        // Run one: sequences 2..=3 (garbage, then a repeat of sequence 1).
        lines[1] = "not json".to_owned();
        lines[2] = lines[0].clone();
        // Run two: sequence 7 torn mid-record.
        lines[6].truncate(40);
        // Run three: sequence 9 carries a damaged sequence number far ahead,
        // which must not swallow the records after it.
        let mut damaged: Value = serde_json::from_str(&lines[8]).unwrap();
        damaged["s"] = json!(1_000_000_000u64);
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
    async fn recovery_rewrites_the_manifest_only_when_the_journal_changed_it() {
        let root = temp_dir("todex-recover-no-rewrite");
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
        for event_type in ["turn.started", "turn.completed"] {
            store
                .append(&manifest.id, event_type, json!({ "turnId": "t" }))
                .await
                .unwrap();
        }
        let directory = root.join("conversations").join(&manifest.id);
        fs::remove_file(directory.join(SNAPSHOT_FILE)).unwrap();

        // In step with its journal: nothing is written.
        let restarted = ConversationStore::new(root.clone()).await.unwrap();
        restarted.recover_with_history(&manifest.id).await.unwrap();
        assert!(!directory.join(SNAPSHOT_FILE).exists());

        // Behind its journal: recovery persists both files again.
        let mut stale: ConversationManifest =
            serde_json::from_slice(&fs::read(directory.join(MANIFEST_FILE)).unwrap()).unwrap();
        stale.last_sequence -= 1;
        fs::write(
            directory.join(MANIFEST_FILE),
            serde_json::to_vec(&stale).unwrap(),
        )
        .unwrap();
        let restarted = ConversationStore::new(root.clone()).await.unwrap();
        let (recovered, _) = restarted.recover_with_history(&manifest.id).await.unwrap();
        assert_eq!(recovered.last_sequence, 2);
        assert!(directory.join(SNAPSHOT_FILE).exists());
        fs::remove_dir_all(root).unwrap();
    }

    #[tokio::test]
    async fn journal_namespace_events_cannot_be_appended() {
        let root = temp_dir("todex-journal-namespace");
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
        for event_type in [
            JOURNAL_RECORD_LOST_EVENT,
            JOURNAL_COMPACTED_EVENT,
            "journal.anything",
        ] {
            assert!(matches!(
                store.append(&manifest.id, event_type, json!({})).await,
                Err(AppError::InvalidRequest(_))
            ));
        }
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

    /// Append `count` records padded so the active segment rotates several
    /// times at the test segment size, then return the store and directory.
    async fn segmented_journal(
        prefix: &str,
        count: u64,
    ) -> (PathBuf, ConversationStore, String, PathBuf) {
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
        // ~8.5 KiB per record: about sixteen records per test segment.
        let payload = json!({ "pad": "x".repeat(8 * 1024) });
        for _ in 0..count {
            store
                .append(&manifest.id, "provider.event", payload.clone())
                .await
                .unwrap();
        }
        let directory = root.join("conversations").join(&manifest.id);
        (root, store, manifest.id, directory)
    }

    #[tokio::test]
    async fn journal_files_orders_segments_and_ignores_non_journal_files() {
        let root = temp_dir("todex-journal-files");
        let directory = root.join("conversations").join("files");
        fs::create_dir_all(&directory).unwrap();
        for name in [
            "events.000010.jsonl",
            "events.jsonl",
            "events.000002.jsonl",
            "events.corrupt.20260101T000000.000Z.jsonl",
            ".events.deadbeef.tmp",
            "manifest.json",
            "events.notanumber.jsonl",
        ] {
            fs::write(directory.join(name), b"x\n").unwrap();
        }
        let files = journal_files(&directory).await.unwrap();
        assert_eq!(
            files
                .iter()
                .map(|file| file.name.as_str())
                .collect::<Vec<_>>(),
            vec!["events.000002.jsonl", "events.000010.jsonl", "events.jsonl"]
        );
        assert_eq!(sealed_segment_number("events.000042.jsonl"), Some(42));
        assert_eq!(sealed_segment_number("events.jsonl"), None);
        assert_eq!(sealed_segment_number("events.corrupt.x.jsonl"), None);
        fs::remove_dir_all(root).unwrap();
    }

    #[tokio::test]
    async fn append_seals_a_full_segment_and_keeps_sequences_global() {
        // 40 records of ~16 KiB seal two test segments (128 KiB each).
        let (root, store, id, directory) = segmented_journal("todex-seal", 40).await;
        let files = journal_files(&directory).await.unwrap();
        let names: Vec<&str> = files.iter().map(|file| file.name.as_str()).collect();
        assert_eq!(
            names,
            vec!["events.000001.jsonl", "events.000002.jsonl", "events.jsonl"]
        );
        assert!(files[0].bytes >= JOURNAL_SEGMENT_BYTES);
        assert!(files[1].bytes >= JOURNAL_SEGMENT_BYTES);
        assert!(files[2].bytes < JOURNAL_SEGMENT_BYTES);
        assert_eq!(store.get(&id).await.unwrap().last_sequence, 40);

        // Replay crosses the sealed/active boundary with global sequences.
        let page = store.replay(&id, 14, 4).await.unwrap();
        assert_eq!(sequences(&page), vec![15, 16, 17, 18]);
        let tail = store.replay_before(&id, u64::MAX, 6).await.unwrap();
        assert_eq!(sequences(&tail), (35..=40).collect::<Vec<_>>());
        fs::remove_dir_all(root).unwrap();
    }

    #[tokio::test]
    async fn restart_indexes_and_replays_a_segmented_journal() {
        let (root, store, id, _directory) = segmented_journal("todex-seal-cold", 40).await;
        drop(store);

        // A restarted store cold-indexes every segment; the index must track
        // more than one file and replay must validate across the boundary.
        let restarted = ConversationStore::new(root.clone()).await.unwrap();
        let (manifest, history) = restarted.recover_with_history(&id).await.unwrap();
        assert_eq!(manifest.last_sequence, 40);
        assert_eq!(history.len(), 40);
        assert_eq!(
            history
                .iter()
                .map(|event| event.sequence)
                .collect::<Vec<_>>(),
            (1..=40).collect::<Vec<_>>()
        );
        {
            let index = restarted.test_index(&id);
            assert!(index.files.len() >= 3);
            let segments: std::collections::BTreeSet<usize> = index
                .parts
                .iter()
                .filter_map(|part| match part {
                    IndexPart::Plain { file, .. } => Some(*file),
                    IndexPart::Sealed(_) => None,
                })
                .collect();
            assert!(segments.len() >= 3, "records must span sealed segments");
        }
        // A fresh store with no index yet builds it from the segment scan.
        let cold = ConversationStore::new(root.clone()).await.unwrap();
        let page = cold.replay(&id, 0, 40).await.unwrap();
        assert_eq!(sequences(&page), (1..=40).collect::<Vec<_>>());
        assert!(!cold.test_index(&id).fully_validated);
        let before = cold.replay_before(&id, 20, 6).await.unwrap();
        assert_eq!(sequences(&before), (15..=20).collect::<Vec<_>>());
        fs::remove_dir_all(root).unwrap();
    }

    #[tokio::test]
    async fn append_recreates_the_active_segment_lost_between_rename_and_create() {
        let (root, store, id, directory) = segmented_journal("todex-seal-crash", 24).await;
        assert!(directory.join("events.000001.jsonl").exists());
        // The journal ends in a sealed segment — the crash window between
        // sealing the active file and creating its replacement, or losing
        // the whole active file. Sequences continue from the sealed tail
        // (records the lost file held are gone, as the manifest learns too).
        fs::remove_file(directory.join(EVENTS_FILE)).unwrap();

        let appended = store
            .append(&id, "turn.completed", json!({ "turnId": "t" }))
            .await
            .unwrap();
        assert_eq!(appended.sequence, 17);
        assert!(directory.join(EVENTS_FILE).exists());
        assert!(directory.join("events.000001.jsonl").exists());
        let page = store.replay(&id, 14, 4).await.unwrap();
        assert_eq!(sequences(&page), vec![15, 16, 17]);
        fs::remove_dir_all(root).unwrap();
    }

    #[tokio::test]
    async fn recovery_cuts_a_torn_active_tail_and_keeps_sealed_segments() {
        let (root, store, id, directory) = segmented_journal("todex-seal-torn", 24).await;
        let sealed = directory.join("events.000001.jsonl");
        let sealed_bytes = fs::read(&sealed).unwrap();
        drop(store);

        // A torn write tail in the active file only: the sealed segments are
        // untouched and the appended garbage is quarantined.
        let mut active = fs::read(directory.join(EVENTS_FILE)).unwrap();
        active.extend_from_slice(b"{\"schemaVersion\":2,\"sequence\":25");
        fs::write(directory.join(EVENTS_FILE), &active).unwrap();
        let restarted = ConversationStore::new(root.clone()).await.unwrap();
        let tail = restarted.replay_before(&id, u64::MAX, 5).await.unwrap();
        assert_eq!(sequences(&tail), (20..=24).collect::<Vec<_>>());
        assert_eq!(fs::read(&sealed).unwrap(), sealed_bytes);
        assert_eq!(corrupt_copies(&directory.join(EVENTS_FILE)).len(), 1);
        fs::remove_dir_all(root).unwrap();
    }

    #[tokio::test]
    async fn salvage_rewrites_sealed_interior_damage_into_the_active_file() {
        let (root, store, id, directory) = segmented_journal("todex-seal-salvage", 24).await;
        drop(store);

        // Corrupt a record inside the sealed file: salvage rewrites that
        // file only, keeps every sequence occupied by a record or a
        // placeholder, and leaves the other files untouched.
        let sealed = directory.join("events.000001.jsonl");
        let raw = fs::read_to_string(&sealed).unwrap();
        let mut lines = raw.lines().map(str::to_owned).collect::<Vec<_>>();
        lines[3] = "garbage".to_owned();
        fs::write(&sealed, format!("{}\n", lines.join("\n"))).unwrap();

        let restarted = ConversationStore::new(root.clone()).await.unwrap();
        let history = restarted.complete_history(&id).await.unwrap();
        assert_eq!(
            history
                .iter()
                .map(|event| event.sequence)
                .collect::<Vec<_>>(),
            (1..=24).collect::<Vec<_>>()
        );
        assert_eq!(lost_runs(&history), vec![(4, 4, 1)]);
        // Per-file salvage: the sealed file is rewritten in place and the
        // backup holds only that file's original bytes.
        let names: Vec<String> = journal_files(&directory)
            .await
            .unwrap()
            .into_iter()
            .map(|file| file.name)
            .collect();
        assert!(
            names.contains(&"events.000001.jsonl".to_owned()),
            "{names:?}"
        );
        assert_eq!(names.last().map(String::as_str), Some(EVENTS_FILE));
        let copies = corrupt_copies(&directory.join(EVENTS_FILE));
        assert_eq!(copies.len(), 1);
        assert_eq!(
            fs::read_to_string(&copies[0]).unwrap(),
            format!("{}\n", lines.join("\n"))
        );
        fs::remove_dir_all(root).unwrap();
    }

    fn temp_dir(prefix: &str) -> PathBuf {
        std::env::temp_dir().join(format!("{prefix}-{}", Uuid::new_v4().simple()))
    }
}
