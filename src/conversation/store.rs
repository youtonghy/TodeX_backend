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
    redact_secrets, status_after_event, ConversationEvent, ConversationManifest,
    ConversationReplay, ConversationSnapshot, ProviderState, CONVERSATION_SCHEMA_VERSION,
    MAX_EVENT_PAYLOAD_BYTES,
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
}

impl ConversationStore {
    pub async fn new(data_dir: PathBuf) -> Result<Self, AppError> {
        let root = data_dir.join("conversations");
        tokio::fs::create_dir_all(&root).await?;
        set_owner_only(&root, true).await?;
        Ok(Self {
            root,
            locks: Arc::new(DashMap::new()),
        })
    }

    pub async fn create(
        &self,
        manifest: ConversationManifest,
    ) -> Result<ConversationManifest, AppError> {
        validate_id(&manifest.id)?;
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
                &ProviderState::new(manifest.provider),
            )
            .await?;
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
        Ok(())
    }

    pub async fn cleanup_before(
        &self,
        cutoff: DateTime<Utc>,
    ) -> Result<Vec<ConversationManifest>, AppError> {
        let manifests = self.list().await?;
        let mut removed = Vec::new();
        for manifest in manifests {
            if manifest.updated_at >= cutoff
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
                removed.push(current);
            }
        }
        Ok(removed)
    }

    pub async fn append(
        &self,
        conversation_id: &str,
        event_type: impl Into<String>,
        mut payload: Value,
    ) -> Result<ConversationEvent, AppError> {
        let event_type = event_type.into();
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
            manifest.status = status_after_event(manifest.status, &last_event.event_type);
            manifest.updated_at = last_event.time;
        }
        let event = ConversationEvent::new(
            conversation_id,
            manifest.last_sequence.saturating_add(1),
            event_type,
            payload,
        );
        let mut line = serde_json::to_vec(&event)?;
        line.push(b'\n');
        let event_path = directory.join(EVENTS_FILE);
        let journal_bytes = match tokio::fs::metadata(&event_path).await {
            Ok(metadata) => metadata.len(),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => 0,
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

        manifest.last_sequence = event.sequence;
        manifest.status = status_after_event(manifest.status, &event.event_type);
        manifest.updated_at = event.time;
        write_atomic_json(&directory.join(MANIFEST_FILE), &manifest).await?;
        write_atomic_json(
            &directory.join(SNAPSHOT_FILE),
            &ConversationSnapshot::from_manifest(&manifest),
        )
        .await?;
        Ok(event)
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
        let events = self.read_and_recover_events(conversation_id).await?;
        let limit = limit.clamp(1, MAX_REPLAY_LIMIT);
        let mut matching = events
            .into_iter()
            .filter(|event| event.sequence > after_sequence);
        let events = matching.by_ref().take(limit).collect::<Vec<_>>();
        let has_more = matching.next().is_some();
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
            status = status_after_event(status, &event.event_type);
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
            events.push(event);
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
        self.locks
            .entry(conversation_id.to_owned())
            .or_insert_with(|| Arc::new(Mutex::new(())))
            .clone()
            .lock_owned()
            .await
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
