use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine as _;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use tokio::io::AsyncWriteExt;
use uuid::Uuid;

use crate::error::AppError;
use crate::workspace_paths::validate_workspace_directory_text;

use super::{
    redact_secrets, ConversationManifest, ConversationStore, ProviderKind, ProviderState,
    MAX_EVENT_PAYLOAD_BYTES,
};

const MIGRATION_SCHEMA_VERSION: u32 = 1;
const MAX_LEGACY_JOURNAL_BYTES: u64 = 64 * 1024 * 1024;
const MAX_LEGACY_STATE_BYTES: u64 = 8 * 1024 * 1024;
const MAX_LEGACY_EVENTS: usize = 100_000;
const MAX_LEGACY_SESSIONS: usize = 2_000;

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct LegacyMigrationReport {
    pub imported: usize,
    pub already_imported: usize,
    pub skipped: usize,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
struct MigrationMap {
    schema_version: u32,
    entries: BTreeMap<String, MigrationEntry>,
}

impl Default for MigrationMap {
    fn default() -> Self {
        Self {
            schema_version: MIGRATION_SCHEMA_VERSION,
            entries: BTreeMap::new(),
        }
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
struct MigrationEntry {
    conversation_id: String,
    complete: bool,
    #[serde(default)]
    source_hash: Option<String>,
}

#[derive(Clone, Debug, Deserialize)]
struct LegacyEventRecord {
    #[serde(default, alias = "sessionId")]
    session_id: String,
    #[serde(default, alias = "eventId")]
    event_id: String,
    cursor: u64,
    #[serde(rename = "type")]
    event_type: String,
    #[serde(default, alias = "codexThreadId")]
    codex_thread_id: Option<String>,
    #[serde(default, alias = "codexTurnId")]
    codex_turn_id: Option<String>,
    #[serde(default)]
    payload: Value,
}

#[derive(Clone, Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
struct LegacyWorkspaceSnapshot {
    #[serde(default)]
    workspaces: Vec<LegacyWorkspace>,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct LegacyWorkspace {
    #[serde(default)]
    name: String,
    #[serde(default)]
    path: String,
    #[serde(default)]
    session_id: String,
    #[serde(default)]
    thread_id: String,
}

struct LegacySource {
    source_key: String,
    source_hash: String,
    session_id: String,
    records: Vec<LegacyEventRecord>,
}

pub async fn migrate_legacy_codex_sessions(
    data_dir: &Path,
    workspace_root: &Path,
    store: &ConversationStore,
) -> Result<LegacyMigrationReport, AppError> {
    let legacy_root = data_dir.join("codex_gateway/sessions");
    let metadata = match tokio::fs::symlink_metadata(&legacy_root).await {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok(LegacyMigrationReport::default());
        }
        Err(error) => return Err(error.into()),
    };
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        tracing::warn!(path = %legacy_root.display(), "legacy Codex session root is not a regular directory; skipping migration");
        return Ok(LegacyMigrationReport {
            skipped: 1,
            ..LegacyMigrationReport::default()
        });
    }

    let mapping_path = data_dir.join("migrations/codex-gateway-v1.json");
    let mut mapping = load_mapping(&mapping_path).await?;
    let legacy_workspaces = load_legacy_workspaces(&data_dir.join("workspaces.json")).await;
    let mut entries = tokio::fs::read_dir(&legacy_root).await?;
    let mut paths = Vec::new();
    while let Some(entry) = entries.next_entry().await? {
        if paths.len() >= MAX_LEGACY_SESSIONS {
            tracing::warn!(
                limit = MAX_LEGACY_SESSIONS,
                "legacy Codex session migration limit reached"
            );
            break;
        }
        if entry.file_type().await?.is_dir() {
            paths.push(entry.path());
        }
    }
    paths.sort();

    let mut report = LegacyMigrationReport::default();
    for path in paths {
        let source = match read_legacy_source(path).await {
            Ok(Some(source)) => source,
            Ok(None) => {
                report.skipped += 1;
                continue;
            }
            Err(error) => {
                report.skipped += 1;
                tracing::warn!(error = %error, "skipping unreadable legacy Codex session");
                continue;
            }
        };

        let existing = mapping.entries.get(&source.source_key).cloned();
        if let Some(entry) = &existing {
            if entry.complete
                && entry.source_hash.as_deref() == Some(source.source_hash.as_str())
                && conversation_matches_source(store, &entry.conversation_id, &source.source_key)
                    .await?
            {
                report.already_imported += 1;
                continue;
            }
        }

        let conversation_id = if let Some(entry) = existing {
            if store.get(&entry.conversation_id).await.is_ok()
                && !conversation_matches_source(store, &entry.conversation_id, &source.source_key)
                    .await?
            {
                report.skipped += 1;
                tracing::warn!(conversation_id = %entry.conversation_id, "legacy migration mapping points to an unrelated conversation");
                continue;
            }
            entry.conversation_id
        } else if let Some(id) = find_imported_source(store, &source.source_key).await? {
            id
        } else {
            Uuid::new_v4().to_string()
        };

        mapping.entries.insert(
            source.source_key.clone(),
            MigrationEntry {
                conversation_id: conversation_id.clone(),
                complete: false,
                source_hash: Some(source.source_hash.clone()),
            },
        );
        write_mapping(&mapping_path, &mapping).await?;

        let workspace =
            legacy_workspace_for(&legacy_workspaces, &source.session_id, workspace_root);
        match import_source(
            store,
            &conversation_id,
            workspace,
            &legacy_workspaces,
            &source,
        )
        .await
        {
            Ok(()) => {
                if let Some(entry) = mapping.entries.get_mut(&source.source_key) {
                    entry.complete = true;
                    entry.source_hash = Some(source.source_hash.clone());
                }
                write_mapping(&mapping_path, &mapping).await?;
                report.imported += 1;
            }
            Err(error) => {
                report.skipped += 1;
                tracing::warn!(conversation_id, error = %error, "legacy Codex session migration is incomplete and will be retried");
            }
        }
    }
    Ok(report)
}

async fn import_source(
    store: &ConversationStore,
    conversation_id: &str,
    workspace: PathBuf,
    workspaces: &LegacyWorkspaceSnapshot,
    source: &LegacySource,
) -> Result<(), AppError> {
    let legacy_workspace = workspaces
        .workspaces
        .iter()
        .find(|workspace| workspace.session_id == source.session_id);
    let title = legacy_workspace
        .map(|workspace| workspace.name.trim())
        .filter(|name| !name.is_empty())
        .map(|name| name.chars().take(200).collect())
        .or_else(|| Some("Imported Codex conversation".to_owned()));

    let manifest = match store.get(conversation_id).await {
        Ok(manifest) => manifest,
        Err(AppError::NotFound(_)) => {
            let mut manifest =
                ConversationManifest::new(ProviderKind::Codex, workspace, title, None);
            manifest.id = conversation_id.to_owned();
            store.create(manifest).await?
        }
        Err(error) => return Err(error),
    };

    let expected_total = source.records.len() as u64 + 1;
    if manifest.last_sequence > expected_total {
        return Err(AppError::Conflict(format!(
            "conversation {conversation_id} contains events after its legacy import"
        )));
    }
    if manifest.last_sequence == 0 {
        store
            .append(
                conversation_id,
                "migration.imported",
                json!({
                    "source": "codex_gateway_v1",
                    "sourceKey": source.source_key,
                    "sourceHash": source.source_hash,
                    "legacyEventCount": source.records.len(),
                }),
            )
            .await?;
    } else if !conversation_matches_source(store, conversation_id, &source.source_key).await? {
        return Err(AppError::Conflict(format!(
            "conversation {conversation_id} does not match its legacy migration source"
        )));
    }

    let imported_records = validate_imported_prefix(store, conversation_id, source).await?;
    for record in source.records.iter().skip(imported_records) {
        store
            .append(
                conversation_id,
                "legacy.codex.event",
                legacy_event_payload(record),
            )
            .await?;
    }

    let native_session_id = source
        .records
        .iter()
        .rev()
        .find_map(|record| record.codex_thread_id.clone())
        .or_else(|| {
            legacy_workspace
                .map(|workspace| workspace.thread_id.trim().to_owned())
                .filter(|thread_id| !thread_id.is_empty())
        });
    let mut provider_state = ProviderState::new(ProviderKind::Codex);
    provider_state.recoverable = native_session_id.is_some();
    provider_state.native_session_id = native_session_id;
    store
        .save_provider_state(conversation_id, provider_state)
        .await
}

async fn validate_imported_prefix(
    store: &ConversationStore,
    conversation_id: &str,
    source: &LegacySource,
) -> Result<usize, AppError> {
    let imported_records = store
        .get(conversation_id)
        .await?
        .last_sequence
        .saturating_sub(1) as usize;
    if imported_records > source.records.len() {
        return Err(AppError::Conflict(format!(
            "conversation {conversation_id} contains events after its legacy import"
        )));
    }

    let mut after_sequence = 1;
    let mut index = 0;
    while index < imported_records {
        let replay = store.replay(conversation_id, after_sequence, 1000).await?;
        if replay.events.is_empty() {
            return Err(AppError::Conflict(format!(
                "conversation {conversation_id} legacy import prefix is incomplete"
            )));
        }
        for event in replay.events {
            if index >= imported_records {
                break;
            }
            let source_record = &source.records[index];
            let matches = event.event_type == "legacy.codex.event"
                && event.payload == legacy_event_payload(source_record);
            if !matches {
                return Err(AppError::Conflict(format!(
                    "conversation {conversation_id} contains non-legacy or changed events in its import prefix"
                )));
            }
            after_sequence = event.sequence;
            index += 1;
        }
    }
    Ok(imported_records)
}

fn legacy_event_payload(record: &LegacyEventRecord) -> Value {
    let mut data = record.payload.clone();
    redact_secrets(&mut data);
    if matches!(
        record.event_type.as_str(),
        "codex.serverRequest.resolved" | "codex.mcp.elicitation.resolved"
    ) {
        if let Some(object) = data.as_object_mut() {
            if object.remove("response").is_some() {
                object.insert("responseOmitted".to_owned(), Value::Bool(true));
            }
        }
    }
    let mut payload = json!({
        "legacyCursor": record.cursor,
        "legacyEventId": record.event_id,
        "legacyType": record.event_type,
        "codexThreadId": record.codex_thread_id,
        "codexTurnId": record.codex_turn_id,
        "data": data,
    });
    if serde_json::to_vec(&payload).map_or(true, |bytes| bytes.len() > MAX_EVENT_PAYLOAD_BYTES) {
        payload = json!({
            "legacyCursor": record.cursor,
            "legacyEventId": record.event_id,
            "legacyType": record.event_type,
            "codexThreadId": record.codex_thread_id,
            "codexTurnId": record.codex_turn_id,
            "dataOmitted": true,
            "reason": "legacy event exceeds the conversation payload limit",
        });
    }
    payload
}

fn legacy_workspace_for(
    snapshot: &LegacyWorkspaceSnapshot,
    session_id: &str,
    workspace_root: &Path,
) -> PathBuf {
    snapshot
        .workspaces
        .iter()
        .find(|workspace| workspace.session_id == session_id)
        .and_then(|workspace| {
            validate_workspace_directory_text(workspace_root, &workspace.path).ok()
        })
        .unwrap_or_else(|| workspace_root.to_path_buf())
}

async fn conversation_matches_source(
    store: &ConversationStore,
    conversation_id: &str,
    source_key: &str,
) -> Result<bool, AppError> {
    let replay = match store.replay(conversation_id, 0, 1).await {
        Ok(replay) => replay,
        Err(AppError::NotFound(_)) => return Ok(false),
        Err(error) => return Err(error),
    };
    Ok(replay.events.first().is_some_and(|event| {
        event.event_type == "migration.imported"
            && (event.payload.get("sourceKey").and_then(Value::as_str) == Some(source_key)
                || event.payload.get("sourceHash").and_then(Value::as_str) == Some(source_key))
    }))
}

async fn find_imported_source(
    store: &ConversationStore,
    source_key: &str,
) -> Result<Option<String>, AppError> {
    for manifest in store.list().await? {
        if conversation_matches_source(store, &manifest.id, source_key).await? {
            return Ok(Some(manifest.id));
        }
    }
    Ok(None)
}

async fn read_legacy_source(path: PathBuf) -> Result<Option<LegacySource>, AppError> {
    tokio::task::spawn_blocking(move || read_legacy_source_blocking(&path))
        .await
        .map_err(|error| AppError::Anyhow(error.into()))?
}

fn read_legacy_source_blocking(path: &Path) -> Result<Option<LegacySource>, AppError> {
    let metadata = std::fs::symlink_metadata(path)?;
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        return Ok(None);
    }
    let event_path = path.join("events.jsonl");
    let state_path = path.join("state.json");
    let component = path
        .file_name()
        .and_then(|name| name.to_str())
        .filter(|name| !name.is_empty())
        .ok_or_else(|| {
            AppError::InvalidRequest("legacy session directory is not UTF-8".to_owned())
        })?;
    let source_key = source_key(component);
    let event_bytes = regular_file_bytes(&event_path, MAX_LEGACY_JOURNAL_BYTES)?;
    let state_bytes = regular_file_bytes(&state_path, MAX_LEGACY_STATE_BYTES)?;
    let source_hash = source_hash(component, event_bytes.as_deref(), state_bytes.as_deref());
    let mut records = Vec::new();
    if let Some(raw) = event_bytes {
        let has_trailing_newline = raw.ends_with(b"\n");
        let nonempty = raw
            .split(|byte| *byte == b'\n')
            .filter(|line| !line.iter().all(u8::is_ascii_whitespace))
            .collect::<Vec<_>>();
        if nonempty.len() > MAX_LEGACY_EVENTS {
            return Err(AppError::InvalidRequest(format!(
                "legacy journal exceeds {MAX_LEGACY_EVENTS} events"
            )));
        }
        for (index, line) in nonempty.iter().enumerate() {
            match serde_json::from_slice::<LegacyEventRecord>(line) {
                Ok(record) => records.push(record),
                Err(error) if index + 1 == nonempty.len() && !has_trailing_newline => {
                    tracing::warn!(path = %event_path.display(), error = %error, "ignoring incomplete legacy journal tail");
                }
                Err(error) => {
                    return Err(AppError::InvalidRequest(format!(
                        "invalid legacy journal record {}: {error}",
                        index + 1
                    )));
                }
            }
        }
    }
    for (index, record) in records.iter().enumerate() {
        let expected = index as u64 + 1;
        if record.cursor != expected {
            return Err(AppError::InvalidRequest(format!(
                "legacy journal cursor gap: expected {expected}, found {}",
                record.cursor
            )));
        }
    }

    let state = state_bytes.and_then(|raw| serde_json::from_slice::<Value>(&raw).ok());
    let session_id = records
        .first()
        .map(|record| record.session_id.trim().to_owned())
        .filter(|session_id| !session_id.is_empty())
        .or_else(|| {
            state.as_ref().and_then(|state| {
                state
                    .get("session_id")
                    .or_else(|| state.get("sessionId"))
                    .and_then(Value::as_str)
                    .map(str::to_owned)
            })
        });
    let Some(session_id) = session_id else {
        return Ok(None);
    };
    if records
        .iter()
        .any(|record| !record.session_id.is_empty() && record.session_id != session_id)
    {
        return Err(AppError::InvalidRequest(
            "legacy journal contains multiple session ids".to_owned(),
        ));
    }
    Ok(Some(LegacySource {
        source_key,
        source_hash,
        session_id,
        records,
    }))
}

fn regular_file_bytes(path: &Path, max_bytes: u64) -> Result<Option<Vec<u8>>, AppError> {
    let metadata = match std::fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error.into()),
    };
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        return Ok(None);
    }
    if metadata.len() > max_bytes {
        return Err(AppError::InvalidRequest(format!(
            "legacy file exceeds {max_bytes} bytes"
        )));
    }
    Ok(Some(std::fs::read(path)?))
}

fn source_key(component: &str) -> String {
    let digest = Sha256::digest(component.as_bytes());
    format!("legacy_{}", URL_SAFE_NO_PAD.encode(digest))
}

fn source_hash(component: &str, events: Option<&[u8]>, state: Option<&[u8]>) -> String {
    let mut hasher = Sha256::new();
    hasher.update(component.as_bytes());
    hasher.update([0]);
    if let Some(events) = events {
        hasher.update(events);
    }
    hasher.update([0]);
    if let Some(state) = state {
        hasher.update(state);
    }
    let digest = hasher.finalize();
    format!("legacy_{}", URL_SAFE_NO_PAD.encode(digest))
}

async fn load_legacy_workspaces(path: &Path) -> LegacyWorkspaceSnapshot {
    match tokio::fs::read(path).await {
        Ok(raw) => serde_json::from_slice(&raw).unwrap_or_else(|error| {
            tracing::warn!(error = %error, "legacy workspace snapshot is invalid; imported conversations will use the workspace root");
            LegacyWorkspaceSnapshot::default()
        }),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            LegacyWorkspaceSnapshot::default()
        }
        Err(error) => {
            tracing::warn!(error = %error, "legacy workspace snapshot is unreadable; imported conversations will use the workspace root");
            LegacyWorkspaceSnapshot::default()
        }
    }
}

async fn load_mapping(path: &Path) -> Result<MigrationMap, AppError> {
    let raw = match tokio::fs::read(path).await {
        Ok(raw) => raw,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok(MigrationMap::default());
        }
        Err(error) => return Err(error.into()),
    };
    let mapping: MigrationMap = serde_json::from_slice(&raw)?;
    if mapping.schema_version != MIGRATION_SCHEMA_VERSION {
        return Err(AppError::InvalidRequest(format!(
            "unsupported legacy migration map schema {}",
            mapping.schema_version
        )));
    }
    Ok(mapping)
}

async fn write_mapping(path: &Path, mapping: &MigrationMap) -> Result<(), AppError> {
    let parent = path
        .parent()
        .ok_or_else(|| AppError::InvalidRequest("migration map has no parent".to_owned()))?;
    tokio::fs::create_dir_all(parent).await?;
    set_owner_only(parent, true).await?;
    let temporary = parent.join(format!(".codex-gateway-v1.{}.tmp", Uuid::new_v4().simple()));
    let mut bytes = serde_json::to_vec_pretty(mapping)?;
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
    set_owner_only(path, false).await
}

async fn set_owner_only(path: &Path, directory: bool) -> Result<(), AppError> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = if directory { 0o700 } else { 0o600 };
        tokio::fs::set_permissions(path, std::fs::Permissions::from_mode(mode)).await?;
    }
    #[cfg(not(unix))]
    let _ = directory;
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::{fs, io::Write};

    use serde_json::json;

    use super::*;

    #[tokio::test]
    async fn legacy_migration_is_copy_only_redacted_and_idempotent() {
        let root = temp_dir("todex-legacy-migration");
        let workspace_root = root.join("workspaces");
        let workspace = workspace_root.join("project");
        let legacy = root.join("codex_gateway/sessions/legacy-session");
        fs::create_dir_all(&workspace).unwrap();
        fs::create_dir_all(&legacy).unwrap();
        let records = [
            json!({
                "session_id": "legacy-session",
                "event_id": "old-1",
                "cursor": 1,
                "type": "codex.item.agentMessage.delta",
                "codex_thread_id": "thread-native-1",
                "payload": { "delta": "hello" },
            }),
            json!({
                "session_id": "legacy-session",
                "event_id": "old-2",
                "cursor": 2,
                "type": "codex.serverRequest.resolved",
                "codex_thread_id": "thread-native-1",
                "payload": {
                    "requestId": "permission-1",
                    "response": { "refreshToken": "legacy-secret" },
                },
            }),
        ];
        let original = format!(
            "{}\n",
            records
                .iter()
                .map(Value::to_string)
                .collect::<Vec<_>>()
                .join("\n")
        );
        fs::write(legacy.join("events.jsonl"), &original).unwrap();
        fs::write(
            root.join("workspaces.json"),
            serde_json::to_vec_pretty(&json!({
                "workspaces": [{
                    "name": "Legacy project",
                    "path": workspace,
                    "sessionId": "legacy-session",
                    "threadId": "thread-native-1",
                }],
                "updatedAt": 1,
            }))
            .unwrap(),
        )
        .unwrap();

        let store = ConversationStore::new(root.clone()).await.unwrap();
        let first = migrate_legacy_codex_sessions(&root, &workspace_root, &store)
            .await
            .unwrap();
        assert_eq!(first.imported, 1);
        assert_eq!(first.skipped, 0);
        assert_eq!(
            fs::read_to_string(legacy.join("events.jsonl")).unwrap(),
            original
        );

        let manifests = store.list().await.unwrap();
        assert_eq!(manifests.len(), 1);
        assert_eq!(manifests[0].title.as_deref(), Some("Legacy project"));
        assert_eq!(
            manifests[0].workspace,
            fs::canonicalize(&workspace).unwrap()
        );
        let replay = store.replay(&manifests[0].id, 0, 10).await.unwrap();
        assert_eq!(replay.events.len(), 3);
        let persisted = serde_json::to_string(&replay).unwrap();
        assert!(!persisted.contains("legacy-secret"));
        assert_eq!(
            replay.events[2].payload["data"]["responseOmitted"],
            json!(true)
        );
        let provider_state = store.provider_state(&manifests[0].id).await.unwrap();
        assert_eq!(
            provider_state.native_session_id.as_deref(),
            Some("thread-native-1")
        );
        assert!(provider_state.recoverable);

        let second = migrate_legacy_codex_sessions(&root, &workspace_root, &store)
            .await
            .unwrap();
        assert_eq!(second.imported, 0);
        assert_eq!(second.already_imported, 1);
        assert_eq!(store.list().await.unwrap().len(), 1);

        let appended = json!({
            "session_id": "legacy-session",
            "event_id": "old-3",
            "cursor": 3,
            "type": "codex.item.agentMessage.delta",
            "codex_thread_id": "thread-native-1",
            "payload": { "delta": "continued" },
        });
        fs::OpenOptions::new()
            .append(true)
            .open(legacy.join("events.jsonl"))
            .unwrap()
            .write_all(format!("{}\n", appended).as_bytes())
            .unwrap();
        let third = migrate_legacy_codex_sessions(&root, &workspace_root, &store)
            .await
            .unwrap();
        assert_eq!(third.imported, 1);
        let manifests_after_append = store.list().await.unwrap();
        assert_eq!(manifests_after_append.len(), 1);
        assert_eq!(
            store
                .replay(&manifests_after_append[0].id, 0, 10)
                .await
                .unwrap()
                .events
                .len(),
            4
        );
        assert!(root.join("migrations/codex-gateway-v1.json").is_file());
        let _ = fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn legacy_increment_rejects_a_non_legacy_event_in_the_import_prefix() {
        let root = temp_dir("todex-legacy-migration-prefix");
        let workspace_root = root.join("workspaces");
        let workspace = workspace_root.join("project");
        let legacy = root.join("codex_gateway/sessions/legacy-session");
        fs::create_dir_all(&workspace).unwrap();
        fs::create_dir_all(&legacy).unwrap();
        let first_records = [
            json!({
                "session_id": "legacy-session",
                "event_id": "old-1",
                "cursor": 1,
                "type": "codex.item.agentMessage.delta",
                "payload": { "delta": "one" },
            }),
            json!({
                "session_id": "legacy-session",
                "event_id": "old-2",
                "cursor": 2,
                "type": "codex.item.agentMessage.delta",
                "payload": { "delta": "two" },
            }),
        ];
        fs::write(
            legacy.join("events.jsonl"),
            format!(
                "{}\n",
                first_records
                    .iter()
                    .map(Value::to_string)
                    .collect::<Vec<_>>()
                    .join("\n")
            ),
        )
        .unwrap();

        let store = ConversationStore::new(root.clone()).await.unwrap();
        assert_eq!(
            migrate_legacy_codex_sessions(&root, &workspace_root, &store)
                .await
                .unwrap()
                .imported,
            1
        );
        let conversation_id = store.list().await.unwrap()[0].id.clone();
        store
            .append(
                &conversation_id,
                "message.created",
                json!({ "role": "user", "content": "new v2 event" }),
            )
            .await
            .unwrap();
        fs::OpenOptions::new()
            .append(true)
            .open(legacy.join("events.jsonl"))
            .unwrap()
            .write_all(
                format!(
                    "{}\n",
                    json!({
                        "session_id": "legacy-session",
                        "event_id": "old-3",
                        "cursor": 3,
                        "type": "codex.item.agentMessage.delta",
                        "payload": { "delta": "three" },
                    })
                )
                .as_bytes(),
            )
            .unwrap();

        let report = migrate_legacy_codex_sessions(&root, &workspace_root, &store)
            .await
            .unwrap();
        assert_eq!(report.imported, 0);
        assert_eq!(report.skipped, 1);
        let replay = store.replay(&conversation_id, 0, 10).await.unwrap();
        assert_eq!(replay.events.len(), 4);
        assert_eq!(replay.events[3].event_type, "message.created");
        let _ = fs::remove_dir_all(root);
    }

    fn temp_dir(prefix: &str) -> PathBuf {
        std::env::temp_dir().join(format!("{prefix}-{}", Uuid::new_v4().simple()))
    }
}
