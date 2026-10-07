//! Background journal maintenance: converting sealed journal files into
//! compressed segments and deleting the v2 migration backups older versions
//! made, a week after they were made. One task does all of it, one
//! conversation and one segment at a time, so it never competes with turns
//! for more than a core.
//!
//! Legacy history is never rewritten: plaintext conversations stay as they
//! are (read-only, `docs/history-encryption.md` §8), so neither v2 → v3
//! migration nor re-encryption runs any more.

use std::collections::BTreeSet;
use std::path::Path;
use std::time::Duration;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use tokio::sync::Notify;
use tokio::task::JoinHandle;

use crate::error::AppError;

use super::store::ConversationStore;

/// Directory inside a conversation holding hard links to its v2 journal
/// files from before migration.
pub(super) const BACKUP_DIR: &str = "journal-v2-backup";
const BACKUP_MARKER: &str = "backup.json";
const BACKUP_RETENTION: chrono::TimeDelta = chrono::TimeDelta::days(7);
/// Startup does not clean up; the first pass waits this long.
const CLEANUP_FIRST_DELAY: Duration = Duration::from_secs(120);
const BACKUP_CLEANUP_INTERVAL: Duration = Duration::from_secs(3600);
/// Wake-up interval without notifications (retries failed conversions).
const IDLE_TICK: Duration = Duration::from_secs(60);

/// Conversations with sealed plaintext files waiting for conversion.
#[derive(Default)]
pub(crate) struct MaintenanceQueue {
    pending: std::sync::Mutex<BTreeSet<String>>,
    /// Conversions that failed, retried on the next idle tick.
    failed: std::sync::Mutex<BTreeSet<String>>,
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

    fn retry_later(&self, conversation_id: &str) {
        self.failed
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .insert(conversation_id.to_owned());
    }

    /// Queue every failed conversion again.
    fn retry_failed(&self) {
        let failed = std::mem::take(
            &mut *self
                .failed
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner()),
        );
        self.pending
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .extend(failed);
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
    /// soon as they are sealed (retrying failures every minute) and deletes
    /// week-old migration backups. Every step is committed atomically, so
    /// aborting the task at any point is safe.
    pub fn spawn_maintenance(&self) -> JoinHandle<()> {
        let store = self.clone();
        tokio::spawn(async move {
            let mut next_cleanup = tokio::time::Instant::now() + CLEANUP_FIRST_DELAY;
            let mut next_retry = tokio::time::Instant::now() + IDLE_TICK;
            loop {
                for conversation_id in store.maintenance.take() {
                    store.seal_all_logged(&conversation_id).await;
                }
                let now = tokio::time::Instant::now();
                if now >= next_retry {
                    store.maintenance.retry_failed();
                    next_retry = now + IDLE_TICK;
                }
                if now >= next_cleanup {
                    if let Err(error) = store.remove_expired_backups().await {
                        tracing::warn!(error = %error, "failed to clean up journal migration backups");
                    }
                    next_cleanup = now + BACKUP_CLEANUP_INTERVAL;
                }
                let wake = next_cleanup.min(next_retry);
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
                    tracing::warn!(conversation_id, error = %error, "failed to seal journal segment; will retry");
                    self.maintenance.retry_later(conversation_id);
                    return;
                }
            }
        }
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
