//! Background journal maintenance: converting sealed plaintext files into
//! compressed segments, lazily migrating v2 journals to history v3 and,
//! with history encryption on, re-encrypting plaintext history
//! (`docs/history-encryption.md` §8), and deleting migration backups after
//! a week. One task does all of it, one conversation and one segment at a
//! time, so it never competes with turns for more than a core.

use std::collections::BTreeSet;
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use tokio::sync::Notify;
use tokio::task::JoinHandle;

use crate::error::AppError;

use super::record::{decode_journal_record, encrypted_content};
use super::segment;
use super::store::{journal_files, ConversationStore, JournalFile};
use super::ConversationStatus;

/// Directory inside a conversation holding hard links to its v2 journal
/// files from before migration.
pub(super) const BACKUP_DIR: &str = "journal-v2-backup";
const BACKUP_MARKER: &str = "backup.json";
const BACKUP_RETENTION: chrono::TimeDelta = chrono::TimeDelta::days(7);
/// A conversation is migrated only after this long without a journal write.
const MIGRATION_QUIET: chrono::TimeDelta = chrono::TimeDelta::minutes(2);
/// Startup does not migrate; the first scan waits this long.
const MIGRATION_FIRST_DELAY: Duration = Duration::from_secs(120);
const MIGRATION_INTERVAL: Duration = Duration::from_secs(600);
const BACKUP_CLEANUP_INTERVAL: Duration = Duration::from_secs(3600);
/// Pause between segments of a migration, leaving the disk to turns.
const MIGRATION_PAUSE: Duration = Duration::from_millis(500);
/// Wake-up interval without notifications (retries failed conversions).
const IDLE_TICK: Duration = Duration::from_secs(60);

/// Conversations with sealed plaintext files waiting for conversion.
#[derive(Default)]
pub(crate) struct MaintenanceQueue {
    pending: std::sync::Mutex<BTreeSet<String>>,
    notify: Notify,
    /// History encryption was just enabled: run a migration pass now.
    migrate_soon: AtomicBool,
}

impl MaintenanceQueue {
    pub fn request(&self, conversation_id: &str) {
        self.pending
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .insert(conversation_id.to_owned());
        self.notify.notify_one();
    }

    /// Run the next migration pass without waiting for its interval.
    pub fn request_migration(&self) {
        self.migrate_soon.store(true, Ordering::Release);
        self.notify.notify_one();
    }

    fn take(&self) -> Vec<String> {
        std::mem::take(
            &mut *self
                .pending
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner()),
        )
        .into_iter()
        .collect()
    }
}

#[derive(Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
struct BackupMarker {
    created_at: DateTime<Utc>,
}

/// Bytes available to unprivileged writes on the filesystem holding
/// `path`. Blocking.
pub(super) fn available_space(path: &Path) -> std::io::Result<u64> {
    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStrExt;
        let c_path = std::ffi::CString::new(path.as_os_str().as_bytes())
            .map_err(|error| std::io::Error::new(std::io::ErrorKind::InvalidInput, error))?;
        let mut stats: libc::statvfs = unsafe { std::mem::zeroed() };
        // SAFETY: `c_path` is NUL-terminated and `stats` is a valid out
        // pointer for the duration of the call.
        if unsafe { libc::statvfs(c_path.as_ptr(), &mut stats) } != 0 {
            return Err(std::io::Error::last_os_error());
        }
        #[allow(clippy::unnecessary_cast)]
        Ok((stats.f_bavail as u64).saturating_mul(stats.f_frsize as u64))
    }
    #[cfg(windows)]
    {
        use std::os::windows::ffi::OsStrExt;
        use windows::core::PCWSTR;
        use windows::Win32::Storage::FileSystem::GetDiskFreeSpaceExW;
        let wide: Vec<u16> = path.as_os_str().encode_wide().chain(Some(0)).collect();
        let mut available = 0u64;
        // SAFETY: `wide` is NUL-terminated and outlives the call; the out
        // pointer is valid.
        unsafe { GetDiskFreeSpaceExW(PCWSTR(wide.as_ptr()), Some(&mut available), None, None) }
            .map_err(|error| std::io::Error::other(error.to_string()))?;
        Ok(available)
    }
    #[cfg(not(any(unix, windows)))]
    {
        let _ = path;
        Ok(u64::MAX)
    }
}

impl ConversationStore {
    /// Start the maintenance task. It converts files sealed by appends as
    /// soon as they are sealed, migrates idle v2 conversations (largest
    /// first, starting a while after startup) and deletes week-old
    /// migration backups. Every step is committed atomically, so aborting
    /// the task at any point is safe.
    pub fn spawn_maintenance(&self) -> JoinHandle<()> {
        let store = self.clone();
        tokio::spawn(async move {
            let started = tokio::time::Instant::now();
            let mut next_migration = started + MIGRATION_FIRST_DELAY;
            let mut next_cleanup = started + MIGRATION_FIRST_DELAY;
            loop {
                for conversation_id in store.maintenance.take() {
                    store.seal_all_logged(&conversation_id).await;
                }
                let now = tokio::time::Instant::now();
                if store.maintenance.migrate_soon.swap(false, Ordering::AcqRel) {
                    next_migration = next_migration.min(now);
                }
                if now >= next_cleanup {
                    if let Err(error) = store.remove_expired_backups().await {
                        tracing::warn!(error = %error, "failed to clean up journal migration backups");
                    }
                    next_cleanup = now + BACKUP_CLEANUP_INTERVAL;
                }
                if now >= next_migration {
                    if let Err(error) = store.migrate_idle().await {
                        tracing::warn!(error = %error, "journal migration pass failed");
                    }
                    next_migration = tokio::time::Instant::now() + MIGRATION_INTERVAL;
                }
                let wake = next_migration.min(next_cleanup).min(now + IDLE_TICK);
                tokio::select! {
                    _ = store.maintenance.notify.notified() => {}
                    _ = tokio::time::sleep_until(wake) => {}
                }
            }
        })
    }

    /// Convert every sealed plaintext file of a conversation.
    async fn seal_all_logged(&self, conversation_id: &str) {
        loop {
            match self.seal_next(conversation_id).await {
                Ok(true) => tokio::task::yield_now().await,
                Ok(false) => return,
                Err(AppError::NotFound(_)) => return,
                Err(error) => {
                    // Retried by the next migration pass.
                    tracing::warn!(conversation_id, error = %error, "failed to seal journal segment");
                    return;
                }
            }
        }
    }

    /// Whether a conversation may be rewritten now: no turn running or
    /// waiting, no buffered stream text and no write for a while.
    async fn migration_idle(&self, conversation_id: &str) -> bool {
        if self.has_pending_delta(conversation_id) {
            return false;
        }
        match self.get(conversation_id).await {
            Ok(manifest) => {
                !matches!(
                    manifest.status,
                    ConversationStatus::Running | ConversationStatus::WaitingPermission
                ) && Utc::now() - manifest.updated_at >= MIGRATION_QUIET
            }
            Err(_) => false,
        }
    }

    /// One migration pass: every conversation with v2 data or sealed
    /// plaintext files, largest first, as long as each stays idle.
    pub(super) async fn migrate_idle(&self) -> Result<(), AppError> {
        let encrypting = self.history_encrypted()?;
        let mut candidates = Vec::new();
        for manifest in self.list().await? {
            let directory = self.directory(&manifest.id)?;
            let files = journal_files(&directory).await?;
            let waiting: u64 = files
                .iter()
                .filter(|file| !file.is_sealed_segment())
                .filter(|file| {
                    manifest.storage_version != Some(super::model::STORAGE_VERSION)
                        || file.number().is_some()
                })
                .map(|file| file.bytes)
                .sum();
            let pending = files
                .iter()
                .any(|file| !file.is_sealed_segment() && file.number().is_some());
            let unencrypted = encrypting && manifest.history_encrypted_at.is_none();
            if pending
                || unencrypted
                || manifest.storage_version != Some(super::model::STORAGE_VERSION)
            {
                candidates.push((waiting, manifest.id));
            }
        }
        candidates.sort_by(|left, right| right.cmp(left));
        for (_, conversation_id) in candidates {
            let migrated = match self.migrate_conversation(&conversation_id).await {
                Ok(migrated) => migrated,
                Err(AppError::NotFound(_)) => false,
                Err(error) => {
                    tracing::warn!(conversation_id, error = %error, "journal migration failed; will retry");
                    false
                }
            };
            if !migrated || !encrypting {
                continue;
            }
            match self.encrypt_conversation(&conversation_id).await {
                Ok(_) | Err(AppError::NotFound(_)) => {}
                Err(error) => {
                    tracing::warn!(conversation_id, error = %error, "history encryption migration failed; will retry");
                }
            }
        }
        Ok(())
    }

    /// End-to-end migration of one idle conversation (§8): plaintext
    /// records are sealed into encrypted segments under fresh DEKs, the
    /// title becomes `titleEnc` and `last-request.json` loses its prompt
    /// text. The active file is sealed first so its plaintext goes through
    /// the same conversion; every segment is committed on its own (as
    /// crash-safe as sealing), so a kill resumes where it stopped. The
    /// manifest's `historyEncryptedAt` marks the end. Returns whether the
    /// conversation is fully encrypted. The daemon sees this plaintext once
    /// more while re-encrypting it.
    pub(super) async fn encrypt_conversation(
        &self,
        conversation_id: &str,
    ) -> Result<bool, AppError> {
        if !self.history_encrypted()? {
            return Ok(false);
        }
        if !self.migration_idle(conversation_id).await {
            return Ok(false);
        }
        {
            let _guard = self.lock(conversation_id).await;
            if self
                .get_unlocked(conversation_id)
                .await?
                .history_encrypted_at
                .is_some()
            {
                return Ok(true);
            }
            if self.active_has_plaintext_locked(conversation_id).await? {
                self.seal_active_locked(conversation_id).await?;
            }
        }
        loop {
            if !self.migration_idle(conversation_id).await {
                return Ok(false);
            }
            if !self.seal_next(conversation_id).await? {
                break;
            }
            tokio::time::sleep(MIGRATION_PAUSE).await;
        }
        loop {
            if !self.migration_idle(conversation_id).await {
                return Ok(false);
            }
            let Some(number) = self.next_plain_segment(conversation_id).await? else {
                break;
            };
            if !self.reencrypt_segment(conversation_id, number).await? {
                return Ok(false);
            }
            tokio::time::sleep(MIGRATION_PAUSE).await;
        }
        let _guard = self.lock(conversation_id).await;
        if !self.history_encrypted()?
            || self
                .next_plain_segment_locked(conversation_id)
                .await?
                .is_some()
            || self.active_has_plaintext_locked(conversation_id).await?
        {
            // Encryption was switched off or plaintext arrived meanwhile.
            return Ok(false);
        }
        let mut manifest = self.get_unlocked(conversation_id).await?;
        if let Some(title) = manifest.title.clone() {
            self.apply_title(&mut manifest, Some(title)).await?;
        }
        self.seal_last_request_locked(conversation_id).await?;
        manifest.history_encrypted_at = Some(Utc::now());
        self.persist_manifest_locked(&manifest).await?;
        tracing::info!(conversation_id, "encrypted conversation history");
        Ok(true)
    }

    /// Whether the active file holds a plaintext record. Callers hold the
    /// conversation lock.
    async fn active_has_plaintext_locked(&self, conversation_id: &str) -> Result<bool, AppError> {
        let path = self
            .directory(conversation_id)?
            .join(super::store::EVENTS_FILE);
        let raw = match tokio::fs::read(&path).await {
            Ok(raw) => raw,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(false),
            Err(error) => return Err(error.into()),
        };
        for line in raw.split(|byte| *byte == b'\n') {
            let line = line.trim_ascii();
            if line.is_empty() {
                continue;
            }
            match decode_journal_record(line, conversation_id) {
                Ok(event) if encrypted_content(&event.payload).is_some() => {}
                // Unreadable lines are left to journal validation; sealing
                // the file routes them through it.
                _ => return Ok(true),
            }
        }
        Ok(false)
    }

    /// The oldest sealed segment still holding plaintext content.
    async fn next_plain_segment(&self, conversation_id: &str) -> Result<Option<u64>, AppError> {
        let _guard = self.lock(conversation_id).await;
        self.next_plain_segment_locked(conversation_id).await
    }

    async fn next_plain_segment_locked(
        &self,
        conversation_id: &str,
    ) -> Result<Option<u64>, AppError> {
        let directory = self.directory(conversation_id)?;
        let files = self.files_locked(conversation_id, &directory).await?;
        let numbers: Vec<u64> = files
            .iter()
            .filter(|file| file.is_sealed_segment())
            .filter_map(JournalFile::number)
            .collect();
        tokio::task::spawn_blocking(move || {
            for number in numbers {
                match segment::load_index(&directory, number) {
                    Ok(loaded) if loaded.body.has_plain_content() => return Ok(Some(number)),
                    Ok(_) => {}
                    // Damage is repaired by the read path (segment salvage).
                    Err(error) => {
                        return Err(AppError::InvalidRequest(format!(
                            "journal segment {number} is unreadable: {error}"
                        )))
                    }
                }
            }
            Ok(None)
        })
        .await
        .map_err(|error| AppError::Anyhow(error.into()))?
    }

    /// Rewrite sealed segment `number` with its plaintext encrypted under a
    /// fresh DEK and swap it in (see [`segment::commit_replacement`]).
    /// Returns whether it was committed.
    pub(super) async fn reencrypt_segment(
        &self,
        conversation_id: &str,
        number: u64,
    ) -> Result<bool, AppError> {
        let policy = self.seal_policy(conversation_id).await;
        if policy.migrate.is_none() {
            return Err(AppError::Conflict(
                "no history key to encrypt the conversation with".to_owned(),
            ));
        }
        let directory = self.directory(conversation_id)?;
        let name = segment::segment_name(number);
        let current = |files: Vec<JournalFile>| files.into_iter().find(|file| file.name == name);
        let before = {
            let _guard = self.lock(conversation_id).await;
            current(self.files_locked(conversation_id, &directory).await?)
        };
        if before.is_none() {
            return Ok(false);
        }
        self.sealing.insert(conversation_id.to_owned(), ());
        let path = directory.clone();
        let id = conversation_id.to_owned();
        let built = tokio::task::spawn_blocking(move || {
            segment::rebuild_segment(&path, &id, number, &policy)
        })
        .await;
        let prepared = match built {
            Ok(Ok(prepared)) => prepared,
            Ok(Err(error)) => {
                self.sealing.remove(conversation_id);
                return Err(AppError::InvalidRequest(format!(
                    "conversation {conversation_id} journal segment {number} could not be re-encrypted: {error}"
                )));
            }
            Err(error) => {
                self.sealing.remove(conversation_id);
                return Err(AppError::Anyhow(error.into()));
            }
        };
        let committed = async {
            let _guard = self.lock(conversation_id).await;
            // Appends may go on; only the segment itself must be unchanged.
            if current(self.files_locked(conversation_id, &directory).await?) != before {
                return Ok(false);
            }
            let stop = self.commit_stop();
            let path = directory.clone();
            let temp_seg = prepared.temp_seg.clone();
            let temp_idx = prepared.temp_idx.clone();
            let body = prepared.body.clone();
            let swapped = tokio::task::spawn_blocking(move || {
                let prepared = segment::PreparedSegment {
                    number,
                    temp_seg,
                    temp_idx,
                    body,
                    repacked: Vec::new(),
                };
                segment::commit_replacement(&path, &prepared, stop)
            })
            .await;
            // Whatever happened, the replay index, digest and tail are
            // rebuilt and the directory reconciled (finishing or rolling
            // back an interrupted swap) on next access.
            self.forget_journal_caches_locked(conversation_id);
            swapped
                .map_err(|error| AppError::Anyhow(error.into()))?
                .map_err(|error| {
                    AppError::InvalidRequest(format!(
                        "conversation {conversation_id} journal segment {number} swap failed: {error}"
                    ))
                })?;
            Ok::<_, AppError>(stop.is_none())
        }
        .await;
        self.sealing.remove(conversation_id);
        match committed {
            Ok(true) => {
                if let Some(keys) = &self.history {
                    keys.deks()
                        .release_sealed(conversation_id, &prepared.repacked)
                        .await;
                }
                tracing::info!(
                    conversation_id,
                    segment = number,
                    "re-encrypted journal segment"
                );
                Ok(true)
            }
            Ok(false) => {
                prepared.discard();
                if self.commit_stop().is_some() {
                    // A simulated crash ends the caller like a killed process.
                    return Err(AppError::Conflict(
                        "simulated crash during segment replacement".to_owned(),
                    ));
                }
                Ok(false)
            }
            Err(error) => {
                prepared.discard();
                Err(error)
            }
        }
    }

    /// Migrate one conversation as far as it stays idle. Before the first
    /// rewrite its v2 files are hard-linked into [`BACKUP_DIR`] (copied
    /// where links fail) and the active file is sealed; then plaintext
    /// files are converted one segment at a time, each committed on its
    /// own, so a kill resumes where it stopped. The manifest gains
    /// `storageVersion: 3` once nothing plaintext is sealed. Returns
    /// whether the conversation is fully migrated.
    pub(super) async fn migrate_conversation(
        &self,
        conversation_id: &str,
    ) -> Result<bool, AppError> {
        if !self.migration_idle(conversation_id).await {
            return Ok(false);
        }
        let directory = self.directory(conversation_id)?;
        {
            let _guard = self.lock(conversation_id).await;
            let manifest = self.get_unlocked(conversation_id).await?;
            if manifest.storage_version != Some(super::model::STORAGE_VERSION)
                && !tokio::fs::try_exists(directory.join(BACKUP_DIR).join(BACKUP_MARKER)).await?
            {
                self.seal_active_locked(conversation_id).await?;
                let files = self.files_locked(conversation_id, &directory).await?;
                back_up_v2_files(&directory, &files).await?;
            }
        }
        loop {
            if !self.migration_idle(conversation_id).await {
                return Ok(false);
            }
            if !self.seal_next(conversation_id).await? {
                break;
            }
            tokio::time::sleep(MIGRATION_PAUSE).await;
        }
        let _guard = self.lock(conversation_id).await;
        let files = self.files_locked(conversation_id, &directory).await?;
        if files
            .iter()
            .any(|file| !file.is_sealed_segment() && file.number().is_some())
        {
            return Ok(false);
        }
        let mut manifest = self.get_unlocked(conversation_id).await?;
        if manifest.storage_version != Some(super::model::STORAGE_VERSION) {
            manifest.storage_version = Some(super::model::STORAGE_VERSION);
            self.persist_manifest_locked(&manifest).await?;
            tracing::info!(
                conversation_id,
                "migrated conversation journal to history v3"
            );
        }
        Ok(true)
    }

    /// Delete migration backups older than [`BACKUP_RETENTION`].
    pub(super) async fn remove_expired_backups(&self) -> Result<(), AppError> {
        let mut entries = tokio::fs::read_dir(&self.root).await?;
        while let Some(entry) = entries.next_entry().await? {
            let backup = entry.path().join(BACKUP_DIR);
            let raw = match tokio::fs::read(backup.join(BACKUP_MARKER)).await {
                Ok(raw) => raw,
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
                Err(error) => return Err(error.into()),
            };
            // An unreadable marker is treated as expired: the backup is a
            // convenience copy, never the journal itself.
            let expired = serde_json::from_slice::<BackupMarker>(&raw).map_or(true, |marker| {
                Utc::now() - marker.created_at >= BACKUP_RETENTION
            });
            if expired {
                tokio::fs::remove_dir_all(&backup).await?;
                tracing::info!(path = %backup.display(), "removed expired journal migration backup");
            }
        }
        Ok(())
    }
}

/// Hard-link (or copy) every plaintext journal file into [`BACKUP_DIR`]
/// and write its marker last, so a marker means a complete backup.
async fn back_up_v2_files(directory: &Path, files: &[JournalFile]) -> Result<(), AppError> {
    let backup = directory.join(BACKUP_DIR);
    tokio::fs::create_dir_all(&backup).await?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        tokio::fs::set_permissions(&backup, std::fs::Permissions::from_mode(0o700)).await?;
    }
    for file in files
        .iter()
        .filter(|file| !file.is_sealed_segment() && file.bytes > 0)
    {
        let source = directory.join(&file.name);
        let target = backup.join(&file.name);
        if tokio::fs::try_exists(&target).await? {
            continue;
        }
        if let Err(error) = tokio::fs::hard_link(&source, &target).await {
            tracing::debug!(error = %error, "hard link failed; copying journal file into the migration backup");
            tokio::fs::copy(&source, &target).await?;
        }
    }
    let marker = serde_json::to_vec(&BackupMarker {
        created_at: Utc::now(),
    })?;
    let path = backup.join(BACKUP_MARKER);
    tokio::fs::write(&path, marker).await?;
    super::segment::set_owner_only_blocking(&path)?;
    super::segment::sync_directory_blocking(&backup)?;
    super::segment::sync_directory_blocking(directory)?;
    Ok(())
}
