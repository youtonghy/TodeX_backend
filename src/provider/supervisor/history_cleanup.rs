//! History storage accounting and cleanup: per-conversation disk usage,
//! batch deletion of chosen conversations, and the age / capacity policy
//! sweep run by `/v2/history/cleanup-policy/run` and the background
//! scheduler (`crate::history_cleanup`).

use std::collections::HashSet;

use chrono::{DateTime, Utc};
use serde::Serialize;

use super::ConversationSupervisor;
use crate::conversation::{ConversationManifest, ConversationStatus, ProviderKind};
use crate::error::AppError;

/// Shutdown reason the provider driver sees for policy and batch removals.
const CLEANUP_REASON: &str = "conversation_cleanup";

/// A conversation with the bytes its directory holds on disk.
#[derive(Clone, Debug, Serialize)]
pub struct ConversationUsage {
    #[serde(flatten)]
    pub manifest: ConversationManifest,
    pub bytes: u64,
}

/// A conversation a cleanup left in place, with the error code the API
/// would have returned for it.
#[derive(Clone, Debug, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct CleanupSkip {
    pub conversation_id: String,
    pub code: String,
    pub message: String,
}

impl CleanupSkip {
    pub(crate) fn new(conversation_id: &str, error: &AppError) -> Self {
        Self {
            conversation_id: conversation_id.to_owned(),
            code: error.code().to_owned(),
            message: error.to_string(),
        }
    }
}

#[derive(Clone, Debug, Default, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CleanupReport {
    pub deleted: Vec<String>,
    pub skipped: Vec<CleanupSkip>,
    pub freed_bytes: u64,
}

/// Thresholds of a cleanup policy; `None` disables that rule.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct CleanupThresholds {
    pub max_age_days: Option<u32>,
    pub max_total_bytes: Option<u64>,
}

/// Input of [`plan_policy_deletions`]: one conversation of the tenant.
#[derive(Clone, Debug)]
pub(crate) struct PolicyCandidate {
    pub id: String,
    pub updated_at: DateTime<Utc>,
    pub bytes: u64,
    /// Running, waiting for a permission, or reserved by a starting turn:
    /// counts toward the total, never deleted.
    pub protected: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum PolicyRule {
    Age,
    Capacity,
}

/// Which conversations a policy removes, in deletion order. The age rule
/// removes every unprotected conversation last updated before
/// `now - max_age_days`; then, while the remaining total exceeds
/// `max_total_bytes`, the capacity rule removes the oldest remaining
/// unprotected conversations until it no longer does.
pub(crate) fn plan_policy_deletions(
    candidates: &[PolicyCandidate],
    thresholds: CleanupThresholds,
    now: DateTime<Utc>,
) -> Vec<(String, PolicyRule)> {
    let mut plan = Vec::new();
    let mut removed = HashSet::new();
    if let Some(days) = thresholds.max_age_days {
        let cutoff = now - chrono::Duration::days(i64::from(days));
        for candidate in candidates {
            if !candidate.protected && candidate.updated_at < cutoff {
                removed.insert(candidate.id.as_str());
                plan.push((candidate.id.clone(), PolicyRule::Age));
            }
        }
    }
    if let Some(limit) = thresholds.max_total_bytes {
        let mut total: u64 = candidates
            .iter()
            .filter(|candidate| !removed.contains(candidate.id.as_str()))
            .map(|candidate| candidate.bytes)
            .fold(0, u64::saturating_add);
        let mut remaining: Vec<&PolicyCandidate> = candidates
            .iter()
            .filter(|candidate| !candidate.protected && !removed.contains(candidate.id.as_str()))
            .collect();
        remaining.sort_by(|a, b| a.updated_at.cmp(&b.updated_at).then(a.id.cmp(&b.id)));
        for candidate in remaining {
            if total <= limit {
                break;
            }
            total = total.saturating_sub(candidate.bytes);
            plan.push((candidate.id.clone(), PolicyRule::Capacity));
        }
    }
    plan
}

/// A TodeX conversation that resumes a provider's native session.
#[derive(Clone, Debug)]
pub(crate) struct NativeSessionRef {
    pub owner_id: String,
    pub conversation_id: String,
    pub native_session_id: String,
    /// Running, waiting for a permission, or holding an active turn.
    pub active: bool,
}

fn is_running(manifest: &ConversationManifest) -> bool {
    matches!(
        manifest.status,
        ConversationStatus::Running | ConversationStatus::WaitingPermission
    )
}

impl ConversationSupervisor {
    /// Every conversation (of any owner) of `provider` with the native
    /// session it resumes: the recorded `nativeSessionId`, or — before one is
    /// recorded — the conversation id, which the Claude and Pi drivers pass
    /// as the session id of a first turn.
    pub(crate) async fn native_session_refs(
        &self,
        provider: ProviderKind,
    ) -> Result<Vec<NativeSessionRef>, AppError> {
        let active: HashSet<String> = self
            .active
            .iter()
            .map(|entry| entry.key().clone())
            .collect();
        let mut refs = Vec::new();
        for manifest in self.store.list().await? {
            if manifest.provider != provider {
                continue;
            }
            let native_session_id = match self.store.provider_state(&manifest.id).await {
                Ok(state) => state.native_session_id,
                Err(error) => {
                    tracing::debug!(
                        conversation_id = %manifest.id,
                        error = %error,
                        "could not read provider state for native history references"
                    );
                    None
                }
            }
            .unwrap_or_else(|| manifest.id.clone());
            refs.push(NativeSessionRef {
                active: active.contains(&manifest.id) || is_running(&manifest),
                owner_id: manifest.owner_id,
                conversation_id: manifest.id,
                native_session_id,
            });
        }
        Ok(refs)
    }

    /// Disk usage of a conversation, `0` (with a warning) when it cannot be
    /// measured.
    async fn measured_bytes(&self, conversation_id: &str) -> u64 {
        match self.store.disk_usage(conversation_id).await {
            Ok(bytes) => bytes,
            Err(error) => {
                tracing::warn!(
                    conversation_id = %conversation_id,
                    error = %error,
                    "could not measure conversation storage"
                );
                0
            }
        }
    }

    /// The owner's conversations, newest first, with their disk usage.
    pub async fn storage_owned(&self, owner_id: &str) -> Result<Vec<ConversationUsage>, AppError> {
        let manifests = self.list_owned(owner_id).await?;
        let mut usage = Vec::with_capacity(manifests.len());
        for manifest in manifests {
            let bytes = self.measured_bytes(&manifest.id).await;
            usage.push(ConversationUsage { manifest, bytes });
        }
        Ok(usage)
    }

    /// Deletes the named conversations of the owner through
    /// [`Self::delete_owned`]; each one that cannot be deleted (active,
    /// missing, another owner's) is reported in `skipped` instead of failing
    /// the batch.
    pub async fn cleanup_owned(
        &self,
        owner_id: &str,
        conversation_ids: &[String],
    ) -> CleanupReport {
        let mut report = CleanupReport::default();
        let mut seen = HashSet::new();
        for conversation_id in conversation_ids {
            if !seen.insert(conversation_id.as_str()) {
                continue;
            }
            if let Err(error) = self.get_owned(owner_id, conversation_id).await {
                report
                    .skipped
                    .push(CleanupSkip::new(conversation_id, &error));
                continue;
            }
            let bytes = self.measured_bytes(conversation_id).await;
            match self.delete_owned(owner_id, conversation_id).await {
                Ok(manifest) => {
                    report.deleted.push(manifest.id);
                    report.freed_bytes = report.freed_bytes.saturating_add(bytes);
                }
                Err(error) => report
                    .skipped
                    .push(CleanupSkip::new(conversation_id, &error)),
            }
        }
        report
    }

    /// Applies cleanup thresholds to the owner's conversations (see
    /// [`plan_policy_deletions`]). Like [`Self::cleanup_expired`] it holds
    /// the CLI execution gate for writing, so no turn or fork reservation
    /// starts between choosing the conversations and removing them, and
    /// running or reserved conversations are never removed.
    pub async fn apply_cleanup_policy(
        &self,
        owner_id: &str,
        thresholds: CleanupThresholds,
        now: DateTime<Utc>,
    ) -> Result<CleanupReport, AppError> {
        // Measure before taking the gate: walking every directory can take a
        // while, and the gate blocks turn starts and CLI discovery meanwhile.
        let mut measured = std::collections::HashMap::new();
        for manifest in self.list_owned(owner_id).await? {
            let bytes = self.measured_bytes(&manifest.id).await;
            measured.insert(manifest.id, bytes);
        }
        let _cli_gate = self.cli_execution_gate.write().await;
        let active: HashSet<String> = self
            .active
            .iter()
            .map(|entry| entry.key().clone())
            .collect();
        let mut candidates = Vec::new();
        for manifest in self.list_owned(owner_id).await? {
            let bytes = match measured.get(&manifest.id) {
                Some(bytes) => *bytes,
                None => self.measured_bytes(&manifest.id).await,
            };
            candidates.push(PolicyCandidate {
                protected: active.contains(&manifest.id) || is_running(&manifest),
                id: manifest.id,
                updated_at: manifest.updated_at,
                bytes,
            });
        }
        let bytes_of = |id: &str| {
            candidates
                .iter()
                .find(|candidate| candidate.id == id)
                .map_or(0, |candidate| candidate.bytes)
        };
        let age_cutoff = thresholds
            .max_age_days
            .map(|days| now - chrono::Duration::days(i64::from(days)));
        let mut report = CleanupReport::default();
        for (conversation_id, rule) in plan_policy_deletions(&candidates, thresholds, now) {
            let still_eligible = |current: &ConversationManifest| {
                current.owner_id == owner_id
                    && match (rule, age_cutoff) {
                        (PolicyRule::Age, Some(cutoff)) => current.updated_at < cutoff,
                        _ => true,
                    }
            };
            match self.store.delete_if(&conversation_id, still_eligible).await {
                Ok(Some(manifest)) => {
                    self.release_removed(&manifest, Some(CLEANUP_REASON)).await;
                    report.freed_bytes = report.freed_bytes.saturating_add(bytes_of(&manifest.id));
                    report.deleted.push(manifest.id);
                }
                Ok(None) => report.skipped.push(CleanupSkip::new(
                    &conversation_id,
                    &AppError::Conflict(
                        "conversation changed or became active during cleanup".to_owned(),
                    ),
                )),
                Err(error) => {
                    tracing::warn!(
                        conversation_id = %conversation_id,
                        error = %error,
                        "history cleanup could not delete a conversation"
                    );
                    report
                        .skipped
                        .push(CleanupSkip::new(&conversation_id, &error));
                }
            }
        }
        Ok(report)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn candidate(id: &str, age_days: i64, bytes: u64, protected: bool) -> PolicyCandidate {
        PolicyCandidate {
            id: id.to_owned(),
            updated_at: now() - chrono::Duration::days(age_days),
            bytes,
            protected,
        }
    }

    fn now() -> DateTime<Utc> {
        "2026-10-11T08:00:00Z".parse().unwrap()
    }

    fn ids(plan: &[(String, PolicyRule)]) -> Vec<&str> {
        plan.iter().map(|(id, _)| id.as_str()).collect()
    }

    #[test]
    fn age_rule_removes_only_old_unprotected_conversations() {
        let candidates = [
            candidate("fresh", 1, 10, false),
            candidate("old", 40, 10, false),
            candidate("old-active", 40, 10, true),
        ];
        let plan = plan_policy_deletions(
            &candidates,
            CleanupThresholds {
                max_age_days: Some(30),
                max_total_bytes: None,
            },
            now(),
        );
        assert_eq!(plan, vec![("old".to_owned(), PolicyRule::Age)]);
    }

    #[test]
    fn capacity_rule_removes_oldest_first_and_counts_but_keeps_active() {
        let candidates = [
            candidate("newest", 1, 100, false),
            candidate("active-oldest", 50, 400, true),
            candidate("older", 10, 100, false),
            candidate("oldest", 20, 100, false),
            candidate("middle", 5, 100, false),
        ];
        // Total 800; active 400 stays. Need <= 550: drop oldest (700), older (600), middle (500).
        let plan = plan_policy_deletions(
            &candidates,
            CleanupThresholds {
                max_age_days: None,
                max_total_bytes: Some(550),
            },
            now(),
        );
        assert_eq!(ids(&plan), vec!["oldest", "older", "middle"]);
        assert!(plan.iter().all(|(_, rule)| *rule == PolicyRule::Capacity));

        // A limit below the protected bytes removes every unprotected one.
        let plan = plan_policy_deletions(
            &candidates,
            CleanupThresholds {
                max_age_days: None,
                max_total_bytes: Some(10),
            },
            now(),
        );
        assert_eq!(ids(&plan), vec!["oldest", "older", "middle", "newest"]);
    }

    #[test]
    fn capacity_rule_runs_after_age_rule() {
        let candidates = [
            candidate("ancient", 100, 500, false),
            candidate("recent", 2, 300, false),
            candidate("latest", 1, 300, false),
        ];
        let plan = plan_policy_deletions(
            &candidates,
            CleanupThresholds {
                max_age_days: Some(30),
                max_total_bytes: Some(400),
            },
            now(),
        );
        assert_eq!(
            plan,
            vec![
                ("ancient".to_owned(), PolicyRule::Age),
                ("recent".to_owned(), PolicyRule::Capacity),
            ]
        );
        // Already under the limit after the age rule: nothing more.
        let plan = plan_policy_deletions(
            &candidates,
            CleanupThresholds {
                max_age_days: Some(30),
                max_total_bytes: Some(600),
            },
            now(),
        );
        assert_eq!(ids(&plan), vec!["ancient"]);
    }
}
