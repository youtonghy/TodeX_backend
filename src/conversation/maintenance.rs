//! Background journal maintenance: converting sealed plaintext files into
//! compressed segments, lazily migrating v2 journals to history v3
//! (`docs/history-encryption.md` §8), and deleting migration backups after
//! a week. One task does all of it, one conversation and one segment at a
//! time, so it never competes with turns for more than a core.

use std::collections::BTreeSet;
use std::path::Path;
use std::time::Duration;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use tokio::sync::Notify;
use tokio::task::JoinHandle;

use crate::error::AppError;

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
}

impl MaintenanceQueue {
    pub fn request(&self, conversation_id: &str) {
        self.pending
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .insert(conversation_id.to_owned());
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
            if pending || manifest.storage_version != Some(super::model::STORAGE_VERSION) {
                candidates.push((waiting, manifest.id));
            }
        }
        candidates.sort_by(|left, right| right.cmp(left));
        for (_, conversation_id) in candidates {
            match self.migrate_conversation(&conversation_id).await {
                Ok(_) | Err(AppError::NotFound(_)) => {}
                Err(error) => {
                    tracing::warn!(conversation_id, error = %error, "journal migration failed; will retry");
                }
            }
        }
        Ok(())
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
