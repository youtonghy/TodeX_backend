use std::path::{Path, PathBuf};
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
const MAX_EVENTS_JOURNAL_BYTES: u64 = 64 * 1024 * 1024;

#[derive(Clone)]
pub struct ConversationStore {
    root: PathBuf,
    locks: Arc<DashMap<String, Arc<Mutex<()>>>>,
    indexes: Arc<DashMap<String, JournalIndex>>,
    tails: Arc<DashMap<String, JournalTail>>,
}

#[derive(Clone)]
struct JournalIndex {
    bytes: u64,
    modified: Option<std::time::SystemTime>,
    offsets: Vec<(u64, u64)>,
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
                return Err(AppError::Conflict(
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
        Ok(manifest)
    }

    pub async fn get(&self, conversation_id: &str) -> Result<ConversationManifest, AppError> {
        let directory = self.directory(conversation_id)?;
        let manifest: ConversationManifest =
            read_json(&directory.join(MANIFEST_FILE), "conversation manifest").await?;
        validate_manifest(&manifest, conversation_id)?;
        Ok(manifest)
    }

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
        let directory = self.directory(conversation_id)?;
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
        write_atomic_json(&directory.join(MANIFEST_FILE), &manifest).await?;
        write_atomic_json(
            &directory.join(SNAPSHOT_FILE),
            &ConversationSnapshot::from_manifest(&manifest),
        )
        .await?;
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
        self.indexes.remove(conversation_id);
        self.tails.remove(conversation_id);
        Ok(())
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
                self.indexes.remove(&id);
                self.tails.remove(&id);
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

    async fn append_inner(
        &self,
        conversation_id: &str,
        event_type: String,
        mut payload: Value,
        hub: Option<&ConversationEventHub>,
    ) -> Result<ConversationEvent, AppError> {
        validate_event_type(&event_type)?;
        redact_secrets(&mut payload);
        if serde_json::to_vec(&payload)?.len() > MAX_EVENT_PAYLOAD_BYTES {
            return Err(AppError::InvalidRequest(format!(
                "conversation event payload exceeds {MAX_EVENT_PAYLOAD_BYTES} bytes"
            )));
        }

        let _guard = self.lock(conversation_id).await;
        let directory = self.directory(conversation_id)?;
        let mut manifest: ConversationManifest =
            read_json(&directory.join(MANIFEST_FILE), "conversation manifest").await?;
        validate_manifest(&manifest, conversation_id)?;
        let last_event = self.read_last_event(conversation_id).await?;
        let journal_sequence = last_event.as_ref().map_or(0, |event| event.sequence);
        if manifest.last_sequence != journal_sequence {
            if journal_sequence != manifest.last_sequence.saturating_add(1) {
                return Err(AppError::InvalidRequest(format!(
                    "conversation {conversation_id} manifest and journal require recovery"
                )));
            }
            let last_event = last_event.ok_or_else(|| {
                AppError::InvalidRequest(format!(
                    "conversation {conversation_id} journal sequence is inconsistent"
                ))
            })?;
            manifest.last_sequence = last_event.sequence;
            manifest.status = status_after_conversation_event(manifest.status, &last_event);
            manifest.updated_at = last_event.time;
        }
        let mut event = ConversationEvent::new(
            conversation_id,
            manifest.last_sequence.saturating_add(1),
            event_type,
            payload,
        );
        event.provider = Some(manifest.provider);
        let mut line = serde_json::to_vec(&event)?;
        line.push(b'\n');
        let event_path = directory.join(EVENTS_FILE);
        let (journal_bytes, journal_modified) = match tokio::fs::metadata(&event_path).await {
            Ok(metadata) => (metadata.len(), metadata.modified().ok()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => (0, None),
            Err(error) => return Err(error.into()),
        };
        if journal_bytes.saturating_add(line.len() as u64) > MAX_EVENTS_JOURNAL_BYTES {
            return Err(AppError::Conflict(format!(
                "conversation {conversation_id} journal reached its storage limit"
            )));
        }
        let mut file = tokio::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&event_path)
            .await?;
        set_owner_only(&event_path, false).await?;
        file.write_all(&line).await?;
        file.flush().await?;
        file.sync_data().await?;
        let metadata = file.metadata().await?;
        if let Some(mut index) = self.indexes.get_mut(conversation_id) {
            if index.bytes == journal_bytes && index.modified == journal_modified {
                index
                    .offsets
                    .push((journal_bytes, journal_bytes + line.len() as u64 - 1));
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

        manifest.last_sequence = event.sequence;
        manifest.status = status_after_conversation_event(manifest.status, &event);
        manifest.updated_at = event.time;
        write_atomic_json(&directory.join(MANIFEST_FILE), &manifest).await?;
        write_atomic_json(
            &directory.join(SNAPSHOT_FILE),
            &ConversationSnapshot::from_manifest(&manifest),
        )
        .await?;
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
        read_json::<ConversationManifest>(&directory.join(MANIFEST_FILE), "conversation manifest")
            .await?;
        write_atomic_json(&directory.join("last-request.json"), request).await
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
        self.read_and_recover_events(conversation_id).await
    }

    /// Internal complete scan: unlike the public paginated API this never truncates.
    pub async fn last_user_message(
        &self,
        conversation_id: &str,
    ) -> Result<Option<ConversationEvent>, AppError> {
        let _guard = self.lock(conversation_id).await;
        let events = self.read_and_recover_events(conversation_id).await?;
        Ok(events.into_iter().rev().find(|event| {
            event.event_type == "message.created"
                && event.payload.get("role").and_then(Value::as_str) == Some("user")
        }))
    }

    pub async fn replay(
        &self,
        conversation_id: &str,
        after_sequence: u64,
        limit: usize,
    ) -> Result<ConversationReplay, AppError> {
        let _guard = self.lock(conversation_id).await;
        let directory = self.directory(conversation_id)?;
        if !tokio::fs::try_exists(directory.join(MANIFEST_FILE)).await? {
            return Err(AppError::NotFound(format!(
                "conversation {conversation_id}"
            )));
        }
        let limit = limit.clamp(1, MAX_REPLAY_LIMIT);
        let event_path = directory.join(EVENTS_FILE);
        let metadata = tokio::fs::metadata(&event_path).await?;
        let valid_index = self.indexes.get(conversation_id).is_some_and(|index| {
            index.bytes == metadata.len() && index.modified == metadata.modified().ok()
        });
        if !valid_index {
            // Validate and repair once; the byte index is only a rebuildable cache.
            self.read_and_recover_events(conversation_id).await?;
        }
        let (from, has_more, offsets) = {
            let index = self.indexes.get(conversation_id);
            let offsets = index
                .as_ref()
                .map(|entry| entry.offsets.as_slice())
                .unwrap_or_default();
            let from = usize::try_from(after_sequence)
                .unwrap_or(usize::MAX)
                .min(offsets.len());
            let to = from.saturating_add(limit).min(offsets.len());
            (from, to < offsets.len(), offsets[from..to].to_vec())
        };
        let mut events = Vec::with_capacity(offsets.len());
        if let (Some(first), Some(last)) = (offsets.first(), offsets.last()) {
            let start = first.0;
            let end = last.1;
            let mut file = tokio::fs::File::open(&event_path).await?;
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
        let next_sequence = events.last().map_or(after_sequence, |event| event.sequence);
        Ok(ConversationReplay {
            conversation_id: conversation_id.to_owned(),
            from_sequence: after_sequence,
            next_sequence,
            has_more,
            events,
        })
    }

    pub async fn recover(&self, conversation_id: &str) -> Result<ConversationManifest, AppError> {
        let _guard = self.lock(conversation_id).await;
        let directory = self.directory(conversation_id)?;
        let mut manifest: ConversationManifest =
            read_json(&directory.join(MANIFEST_FILE), "conversation manifest").await?;
        validate_manifest(&manifest, conversation_id)?;
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
        write_atomic_json(&directory.join(MANIFEST_FILE), &manifest).await?;
        write_atomic_json(
            &directory.join(SNAPSHOT_FILE),
            &ConversationSnapshot::from_manifest(&manifest),
        )
        .await?;
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

    async fn get_unlocked(&self, conversation_id: &str) -> Result<ConversationManifest, AppError> {
        let directory = self.directory(conversation_id)?;
        let manifest = read_json(&directory.join(MANIFEST_FILE), "conversation manifest").await?;
        validate_manifest(&manifest, conversation_id)?;
        Ok(manifest)
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
        let ranges = line_ranges(&raw);
        let last_nonempty = ranges
            .iter()
            .rposition(|(start, end)| !trim_ascii(&raw[*start..*end]).is_empty());
        let mut events = Vec::new();
        let mut offsets = Vec::new();
        for (index, (start, end)) in ranges.iter().copied().enumerate() {
            let line = trim_ascii(&raw[start..end]);
            if line.is_empty() {
                continue;
            }
            let event = match serde_json::from_slice::<ConversationEvent>(line) {
                Ok(event) => event,
                Err(error) if Some(index) == last_nonempty => {
                    quarantine_tail(&path, &raw[start..]).await?;
                    let file = tokio::fs::OpenOptions::new()
                        .write(true)
                        .open(&path)
                        .await?;
                    file.set_len(start as u64).await?;
                    file.sync_all().await?;
                    tracing::warn!(conversation_id, error = %error, "recovered invalid conversation journal tail");
                    break;
                }
                Err(error) => {
                    return Err(AppError::InvalidRequest(format!(
                        "conversation {conversation_id} journal is corrupt at sequence {}: {error}",
                        events.len() + 1
                    )));
                }
            };
            validate_event(&event, conversation_id, events.len() as u64 + 1)?;
            offsets.push((start as u64, end as u64));
            events.push(event);
        }
        let metadata = tokio::fs::metadata(&path).await?;
        self.indexes.insert(
            conversation_id.to_owned(),
            JournalIndex {
                bytes: metadata.len(),
                modified: metadata.modified().ok(),
                offsets,
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

    async fn read_last_event(
        &self,
        conversation_id: &str,
    ) -> Result<Option<ConversationEvent>, AppError> {
        let path = self.directory(conversation_id)?.join(EVENTS_FILE);
        let metadata = match tokio::fs::metadata(&path).await {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(error.into()),
        };
        if let Some(tail) = self.tails.get(conversation_id) {
            if tail.bytes == metadata.len() && tail.modified == metadata.modified().ok() {
                return Ok(Some(tail.event.clone()));
            }
        }
        if metadata.len() > MAX_EVENTS_JOURNAL_BYTES {
            return Err(AppError::InvalidRequest(format!(
                "conversation {conversation_id} journal exceeds {MAX_EVENTS_JOURNAL_BYTES} bytes"
            )));
        }
        if metadata.len() == 0 {
            return Ok(None);
        }

        let window = (MAX_EVENT_PAYLOAD_BYTES as u64 + 64 * 1024).min(metadata.len());
        let mut file = tokio::fs::File::open(&path).await?;
        file.seek(std::io::SeekFrom::End(-(window as i64))).await?;
        let mut raw = Vec::with_capacity(window as usize);
        file.read_to_end(&mut raw).await?;
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
            return Ok(None);
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
        Ok(Some(event))
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

async fn write_atomic_json<T: Serialize>(path: &Path, value: &T) -> Result<(), AppError> {
    let parent = path
        .parent()
        .ok_or_else(|| AppError::InvalidRequest("persisted file has no parent".to_owned()))?;
    tokio::fs::create_dir_all(parent).await?;
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

async fn quarantine_tail(path: &Path, tail: &[u8]) -> Result<(), AppError> {
    if tail.is_empty() {
        return Ok(());
    }
    let quarantine = path.with_file_name(format!(
        "events.corrupt.{}.jsonl",
        Utc::now().format("%Y%m%dT%H%M%S%.3fZ")
    ));
    let mut file = tokio::fs::OpenOptions::new()
        .create_new(true)
        .write(true)
        .open(&quarantine)
        .await?;
    set_owner_only(&quarantine, false).await?;
    file.write_all(tail).await?;
    file.flush().await?;
    file.sync_all().await?;
    Ok(())
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

    #[tokio::test]
    async fn journal_rejects_corruption_before_a_valid_record() {
        let root = temp_dir("todex-conversation-middle");
        let workspace = root.join("workspace");
        fs::create_dir_all(&workspace).unwrap();
        let store = ConversationStore::new(root.clone()).await.unwrap();
        let manifest = store
            .create(ConversationManifest::new(
                ProviderKind::ClaudeCode,
                workspace,
                None,
                None,
            ))
            .await
            .unwrap();
        for index in 0..3 {
            store
                .append(&manifest.id, "provider.event", json!({ "index": index }))
                .await
                .unwrap();
        }
        let path = root
            .join("conversations")
            .join(&manifest.id)
            .join(EVENTS_FILE);
        let raw = fs::read_to_string(&path).unwrap();
        let mut lines = raw.lines().map(str::to_owned).collect::<Vec<_>>();
        lines[1] = "{".to_owned();
        fs::write(&path, format!("{}\n", lines.join("\n"))).unwrap();

        assert!(matches!(
            store.replay(&manifest.id, 0, 10).await,
            Err(AppError::InvalidRequest(_))
        ));
        assert_eq!(fs::read_to_string(path).unwrap().lines().count(), 3);
        let _ = fs::remove_dir_all(root);
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

    fn temp_dir(prefix: &str) -> PathBuf {
        std::env::temp_dir().join(format!("{prefix}-{}", Uuid::new_v4().simple()))
    }
}
