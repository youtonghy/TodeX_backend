//! Legacy plaintext history (`docs/history-encryption.md` §8).
//!
//! History has been end-to-end encrypted since encryption became mandatory;
//! conversations written before that may hold plaintext records, a
//! plaintext title or a plaintext request snapshot. They are never
//! rewritten. Instead they are marked `legacyPlaintext` and become
//! read-only: they can be read, archived and deleted, never written.
//!
//! Whether a conversation is legacy is decided once and persisted in its
//! manifest: `legacyPlaintext: true` when any plaintext was found,
//! otherwise `historyEncryptedAt` (the conversation is fully encrypted).
//! Conversations created since carry `historyEncryptedAt` from the start.
//! The decision is made by a one-time startup scan
//! ([`ConversationStore::spawn_legacy_scan`]) and, for a conversation the
//! scan has not reached yet, by the first write attempt
//! ([`ConversationStore::ensure_not_legacy`]), so a write can never slip
//! into undecided legacy history. Both are idempotent; the scan resumes
//! where it stopped because every decision is already on disk, and writes
//! a completion marker once every conversation is decided so later
//! startups skip it.

use std::path::{Path, PathBuf};
use std::time::Instant;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use tokio::task::JoinHandle;

use super::record::encrypted_content;
use super::segment::JOURNAL_RECORD_LOST_EVENT;
use super::store::{ConversationStore, ReplayDetail, MAX_REPLAY_LIMIT};
use crate::error::AppError;

/// `$DATA_DIR/<this>`: written once every conversation was classified.
pub(crate) const LEGACY_SCAN_MARKER: &str = "legacy-plaintext-scan.json";

#[derive(Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
struct ScanMarker {
    completed_at: DateTime<Utc>,
    conversations: usize,
    legacy: usize,
}

/// What one scan pass did.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct LegacyScan {
    /// Conversations listed.
    pub conversations: usize,
    /// Already decided before this pass (by an earlier, interrupted pass,
    /// a write attempt, or because they were created encrypted).
    pub already_decided: usize,
    /// Newly marked `legacyPlaintext`.
    pub marked_legacy: usize,
    /// Newly found fully encrypted.
    pub marked_encrypted: usize,
    /// Could not be classified; the pass will run again next startup.
    pub failed: usize,
    /// The marker file existed: nothing was scanned.
    pub skipped: bool,
}

impl ConversationStore {
    /// `HISTORY_READ_ONLY` when `conversation_id` is legacy plaintext
    /// history, deciding that now if the startup scan has not yet (see the
    /// module docs). Always `Ok` in a keyless test store.
    pub async fn ensure_not_legacy(&self, conversation_id: &str) -> Result<(), AppError> {
        if self.history.is_none() {
            return Ok(());
        }
        let manifest = self.get(conversation_id).await?;
        let legacy = if manifest.legacy_plaintext {
            true
        } else if manifest.history_encrypted_at.is_some() {
            false
        } else {
            self.classify_legacy(conversation_id).await?
        };
        if legacy {
            return Err(AppError::HistoryReadOnly);
        }
        Ok(())
    }

    /// Decide and persist whether `conversation_id` is legacy plaintext
    /// history. Returns whether it is.
    async fn classify_legacy(&self, conversation_id: &str) -> Result<bool, AppError> {
        // Nothing writes plaintext any more, so a result stays true while
        // the conversation keeps changing; the scan needs no lock.
        let legacy = self.has_plaintext(conversation_id).await?;
        let _guard = self.lock(conversation_id).await;
        let mut manifest = self.get_unlocked(conversation_id).await?;
        if manifest.legacy_plaintext || manifest.history_encrypted_at.is_some() {
            return Ok(manifest.legacy_plaintext);
        }
        if legacy {
            manifest.legacy_plaintext = true;
            tracing::info!(
                conversation_id,
                "conversation holds legacy plaintext history; it is read-only"
            );
        } else {
            manifest.history_encrypted_at = Some(Utc::now());
        }
        self.persist_manifest_locked(&manifest).await?;
        Ok(legacy)
    }

    /// Whether the title, the request snapshot or any journal record of
    /// `conversation_id` is stored as plaintext. `journal.recordLost`
    /// placeholders from journal repair are always plaintext and say
    /// nothing about the conversation's content, so they do not count.
    async fn has_plaintext(&self, conversation_id: &str) -> Result<bool, AppError> {
        let manifest = self.get(conversation_id).await?;
        if manifest.title.is_some() {
            return Ok(true);
        }
        if let Some(saved) = self.last_request(conversation_id).await? {
            if saved
                .get("request")
                .is_some_and(|request| request.get("textMac").is_none())
            {
                return Ok(true);
            }
        }
        let mut after = 0;
        loop {
            let page = self
                .replay_detail(
                    conversation_id,
                    after,
                    MAX_REPLAY_LIMIT,
                    ReplayDetail::Summary,
                )
                .await?;
            for event in &page.events {
                if event.event_type != JOURNAL_RECORD_LOST_EVENT
                    && encrypted_content(&event.payload).is_none()
                {
                    return Ok(true);
                }
            }
            match page.events.last() {
                Some(last) if page.has_more => after = last.sequence,
                _ => return Ok(false),
            }
        }
    }

    /// Start the one-time legacy plaintext scan in the background (see the
    /// module docs). It never blocks reads; its duration and outcome are
    /// logged. Nothing to do in a keyless test store.
    pub fn spawn_legacy_scan(&self, data_dir: PathBuf) -> Option<JoinHandle<()>> {
        self.history.as_ref()?;
        let store = self.clone();
        Some(tokio::spawn(async move {
            if let Err(error) = store.scan_legacy(&data_dir).await {
                tracing::warn!(error = %error, "legacy plaintext history scan failed; it runs again on the next start");
            }
        }))
    }

    /// One pass of the legacy plaintext scan; see [`Self::spawn_legacy_scan`].
    pub(crate) async fn scan_legacy(&self, data_dir: &Path) -> Result<LegacyScan, AppError> {
        let marker = data_dir.join(LEGACY_SCAN_MARKER);
        if tokio::fs::try_exists(&marker).await? {
            tracing::debug!("legacy plaintext history scan already completed");
            return Ok(LegacyScan {
                skipped: true,
                ..LegacyScan::default()
            });
        }
        let started = Instant::now();
        let mut scan = LegacyScan::default();
        let mut legacy_total = 0;
        for manifest in self.list().await? {
            scan.conversations += 1;
            if manifest.legacy_plaintext || manifest.history_encrypted_at.is_some() {
                scan.already_decided += 1;
                legacy_total += usize::from(manifest.legacy_plaintext);
                continue;
            }
            match self.classify_legacy(&manifest.id).await {
                Ok(true) => {
                    scan.marked_legacy += 1;
                    legacy_total += 1;
                }
                Ok(false) => scan.marked_encrypted += 1,
                // Deleted meanwhile.
                Err(AppError::NotFound(_)) => scan.conversations -= 1,
                Err(error) => {
                    scan.failed += 1;
                    tracing::warn!(
                        conversation_id = %manifest.id,
                        error = %error,
                        "could not check conversation for legacy plaintext history"
                    );
                }
            }
        }
        let elapsed_ms = started.elapsed().as_millis() as u64;
        tracing::info!(
            conversations = scan.conversations,
            already_decided = scan.already_decided,
            marked_legacy = scan.marked_legacy,
            marked_encrypted = scan.marked_encrypted,
            legacy_total,
            failed = scan.failed,
            elapsed_ms,
            "legacy plaintext history scan finished"
        );
        if scan.failed == 0 {
            let body = serde_json::to_vec_pretty(&ScanMarker {
                completed_at: Utc::now(),
                conversations: scan.conversations,
                legacy: legacy_total,
            })?;
            crate::secure_fs::write_owner_only_atomic(&marker, &body)?;
        }
        Ok(scan)
    }
}
