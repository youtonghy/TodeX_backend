//! Per-tenant automatic history cleanup: the saved policy (maximum age and
//! maximum total size of a tenant's conversation history), the record of
//! its last run, and the background pass that applies enabled policies.
//!
//! State lives in `$data_dir/history-cleanup.json` (owner-only, replaced
//! atomically). The policy is independent of the config-wide
//! `history_retention_days` sweep; both can be active. The removal itself is
//! [`ConversationSupervisor::apply_cleanup_policy`].

use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use tokio::{sync::Notify, task::JoinHandle};

use crate::{
    app_state::AppState,
    error::AppError,
    provider::{CleanupReport, CleanupThresholds, ConversationSupervisor},
};

pub const HISTORY_CLEANUP_FILE: &str = "history-cleanup.json";
const FILE_VERSION: u32 = 1;
/// The first automatic pass runs this long after the daemon starts.
const FIRST_PASS_DELAY: Duration = Duration::from_secs(5 * 60);
/// Time between automatic passes.
const PASS_INTERVAL: Duration = Duration::from_secs(60 * 60);
/// The scheduler re-reads the wall clock at least this often, so a pass
/// stays on time across machine sleep (which pauses monotonic timers).
const CLOCK_CHECK: Duration = Duration::from_secs(60);

pub const MAX_AGE_DAYS: i64 = 3650;
pub const MIN_TOTAL_BYTES: i64 = 1024 * 1024;
pub const MAX_TOTAL_BYTES: i64 = 1 << 53;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CleanupPolicy {
    #[serde(default)]
    pub enabled: bool,
    #[serde(default)]
    pub max_age_days: Option<u32>,
    #[serde(default)]
    pub max_total_bytes: Option<u64>,
}

/// The policy object as a client sends it, before range checks.
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct CleanupPolicyInput {
    #[serde(default)]
    enabled: bool,
    #[serde(default)]
    max_age_days: Option<i64>,
    #[serde(default)]
    max_total_bytes: Option<i64>,
}

impl CleanupPolicy {
    /// Parses and validates a client policy; every failure is
    /// `INVALID_REQUEST`.
    pub fn from_request(value: Value) -> Result<Self, AppError> {
        if !value.is_object() {
            return Err(AppError::InvalidRequest(
                "the cleanup policy must be a JSON object".to_owned(),
            ));
        }
        let input: CleanupPolicyInput = serde_json::from_value(value).map_err(|error| {
            AppError::InvalidRequest(format!("invalid cleanup policy: {error}"))
        })?;
        let max_age_days = match input.max_age_days {
            None => None,
            Some(days) if (1..=MAX_AGE_DAYS).contains(&days) => Some(days as u32),
            Some(_) => {
                return Err(AppError::InvalidRequest(format!(
                    "maxAgeDays must be between 1 and {MAX_AGE_DAYS}"
                )))
            }
        };
        let max_total_bytes = match input.max_total_bytes {
            None => None,
            Some(bytes) if (MIN_TOTAL_BYTES..=MAX_TOTAL_BYTES).contains(&bytes) => {
                Some(bytes as u64)
            }
            Some(_) => {
                return Err(AppError::InvalidRequest(format!(
                    "maxTotalBytes must be between {MIN_TOTAL_BYTES} and {MAX_TOTAL_BYTES}"
                )))
            }
        };
        let policy = Self {
            enabled: input.enabled,
            max_age_days,
            max_total_bytes,
        };
        if policy.enabled && !policy.has_threshold() {
            return Err(AppError::InvalidRequest(
                "an enabled cleanup policy needs maxAgeDays or maxTotalBytes".to_owned(),
            ));
        }
        Ok(policy)
    }

    pub fn has_threshold(&self) -> bool {
        self.max_age_days.is_some() || self.max_total_bytes.is_some()
    }

    pub fn thresholds(&self) -> CleanupThresholds {
        CleanupThresholds {
            max_age_days: self.max_age_days,
            max_total_bytes: self.max_total_bytes,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum CleanupTrigger {
    Auto,
    Manual,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CleanupRun {
    pub at: DateTime<Utc>,
    pub trigger: CleanupTrigger,
    pub deleted: usize,
    pub freed_bytes: u64,
    pub error: Option<String>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TenantCleanup {
    #[serde(default)]
    pub policy: CleanupPolicy,
    #[serde(default)]
    pub last_run: Option<CleanupRun>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
struct CleanupFile {
    version: u32,
    #[serde(default)]
    tenants: BTreeMap<String, TenantCleanup>,
}

#[derive(Clone)]
pub struct HistoryCleanup {
    path: Arc<PathBuf>,
    file: Arc<tokio::sync::Mutex<CleanupFile>>,
    wake: Arc<Notify>,
    /// The scheduler's next planned pass; `None` until it runs.
    next_pass: Arc<std::sync::Mutex<Option<DateTime<Utc>>>>,
}

impl HistoryCleanup {
    /// Loads the saved policies. A missing file means every tenant has the
    /// default (disabled) policy; an unreadable or corrupt one is logged and
    /// treated the same, so it never blocks the daemon from starting.
    pub fn load(data_dir: &Path) -> Self {
        let path = data_dir.join(HISTORY_CLEANUP_FILE);
        let file = match std::fs::read(&path) {
            Ok(bytes) => match serde_json::from_slice::<CleanupFile>(&bytes) {
                Ok(file) => file,
                Err(error) => {
                    tracing::warn!(
                        path = %path.display(),
                        error = %error,
                        "history cleanup policy file is corrupt; using defaults"
                    );
                    CleanupFile::default()
                }
            },
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => CleanupFile::default(),
            Err(error) => {
                tracing::warn!(
                    path = %path.display(),
                    error = %error,
                    "history cleanup policy file is unreadable; using defaults"
                );
                CleanupFile::default()
            }
        };
        Self {
            path: Arc::new(path),
            file: Arc::new(tokio::sync::Mutex::new(file)),
            wake: Arc::new(Notify::new()),
            next_pass: Arc::default(),
        }
    }

    pub async fn tenant(&self, tenant_id: &str) -> TenantCleanup {
        self.file
            .lock()
            .await
            .tenants
            .get(tenant_id)
            .cloned()
            .unwrap_or_default()
    }

    /// Saves the tenant's policy. Enabling it asks the scheduler for a pass
    /// soon; this call never waits for one.
    pub async fn set_policy(
        &self,
        tenant_id: &str,
        policy: CleanupPolicy,
    ) -> Result<TenantCleanup, AppError> {
        let tenant = self
            .update(tenant_id, |tenant| tenant.policy = policy)
            .await?;
        if policy.enabled {
            self.request_pass();
        }
        Ok(tenant)
    }

    pub async fn record_run(
        &self,
        tenant_id: &str,
        run: CleanupRun,
    ) -> Result<TenantCleanup, AppError> {
        self.update(tenant_id, |tenant| tenant.last_run = Some(run))
            .await
    }

    async fn update(
        &self,
        tenant_id: &str,
        change: impl FnOnce(&mut TenantCleanup),
    ) -> Result<TenantCleanup, AppError> {
        let mut file = self.file.lock().await;
        let mut next = file.clone();
        next.version = FILE_VERSION;
        let tenant = next.tenants.entry(tenant_id.to_owned()).or_default();
        change(tenant);
        let tenant = tenant.clone();
        let bytes = serde_json::to_vec_pretty(&next)?;
        let path = self.path.clone();
        tokio::task::spawn_blocking(move || {
            crate::secure_fs::write_owner_only_atomic(&path, &bytes)
        })
        .await
        .map_err(|error| AppError::Anyhow(error.into()))??;
        *file = next;
        Ok(tenant)
    }

    async fn enabled_policies(&self) -> Vec<(String, CleanupPolicy)> {
        self.file
            .lock()
            .await
            .tenants
            .iter()
            .filter(|(_, tenant)| tenant.policy.enabled && tenant.policy.has_threshold())
            .map(|(id, tenant)| (id.clone(), tenant.policy))
            .collect()
    }

    pub fn next_pass_at(&self) -> Option<DateTime<Utc>> {
        *self
            .next_pass
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    fn set_next_pass(&self, at: DateTime<Utc>) {
        *self
            .next_pass
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(at);
    }

    /// Wakes the scheduler for a pass now. Only moves the planned time when
    /// a scheduler is running (one has planned a pass).
    fn request_pass(&self) {
        let mut next = self
            .next_pass
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if next.is_some() {
            *next = Some(Utc::now());
        }
        drop(next);
        self.wake.notify_one();
    }

    /// The `GET`/`PUT /v2/history/cleanup-policy` body.
    pub fn view(&self, tenant: &TenantCleanup) -> Value {
        let next_run_at = if tenant.policy.enabled {
            self.next_pass_at()
        } else {
            None
        };
        json!({
            "policy": tenant.policy,
            "lastRun": tenant.last_run,
            "nextRunAt": next_run_at,
        })
    }
}

/// Applies `policy`'s thresholds to the tenant's history now and records
/// the outcome as its `lastRun` (an error included). The caller makes sure
/// the policy has at least one threshold.
pub async fn run_policy(
    conversations: &ConversationSupervisor,
    cleanup: &HistoryCleanup,
    tenant_id: &str,
    policy: CleanupPolicy,
    trigger: CleanupTrigger,
) -> Result<(CleanupReport, CleanupRun), AppError> {
    let at = Utc::now();
    let outcome = conversations
        .apply_cleanup_policy(tenant_id, policy.thresholds(), at)
        .await;
    let run = match &outcome {
        Ok(report) => CleanupRun {
            at,
            trigger,
            deleted: report.deleted.len(),
            freed_bytes: report.freed_bytes,
            error: None,
        },
        Err(error) => CleanupRun {
            at,
            trigger,
            deleted: 0,
            freed_bytes: 0,
            error: Some(error.to_string()),
        },
    };
    if let Err(error) = cleanup.record_run(tenant_id, run.clone()).await {
        tracing::warn!(
            tenant_id = %tenant_id,
            error = %error,
            "could not record the history cleanup run"
        );
    }
    outcome.map(|report| (report, run))
}

/// Starts the automatic pass: first [`FIRST_PASS_DELAY`] after start, then
/// every [`PASS_INTERVAL`], and promptly after a policy is enabled.
pub fn spawn(state: AppState) -> JoinHandle<()> {
    tokio::spawn(async move {
        let cleanup = state.history_cleanup.clone();
        let mut due = Utc::now() + FIRST_PASS_DELAY;
        cleanup.set_next_pass(due);
        loop {
            let wait = (due - Utc::now())
                .to_std()
                .unwrap_or_default()
                .min(CLOCK_CHECK);
            tokio::select! {
                _ = tokio::time::sleep(wait) => {}
                _ = cleanup.wake.notified() => due = Utc::now(),
            }
            if Utc::now() < due {
                continue;
            }
            run_enabled(&state).await;
            due = Utc::now() + PASS_INTERVAL;
            cleanup.set_next_pass(due);
        }
    })
}

async fn run_enabled(state: &AppState) {
    for (tenant_id, policy) in state.history_cleanup.enabled_policies().await {
        match run_policy(
            &state.conversations,
            &state.history_cleanup,
            &tenant_id,
            policy,
            CleanupTrigger::Auto,
        )
        .await
        {
            Ok((report, _)) if !report.deleted.is_empty() || !report.skipped.is_empty() => {
                tracing::info!(
                    tenant_id = %tenant_id,
                    deleted = report.deleted.len(),
                    skipped = report.skipped.len(),
                    freed_bytes = report.freed_bytes,
                    "history cleanup policy removed conversations"
                )
            }
            Ok(_) => {}
            Err(error) => tracing::warn!(
                tenant_id = %tenant_id,
                error = %error,
                "history cleanup policy run failed"
            ),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(label: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("todex-{label}-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn invalid(value: Value) -> bool {
        matches!(
            CleanupPolicy::from_request(value),
            Err(AppError::InvalidRequest(_))
        )
    }

    #[test]
    fn policy_validation() {
        assert_eq!(
            CleanupPolicy::from_request(json!({
                "enabled": false, "maxAgeDays": null, "maxTotalBytes": null
            }))
            .unwrap(),
            CleanupPolicy::default()
        );
        assert_eq!(
            CleanupPolicy::from_request(json!({
                "enabled": true, "maxAgeDays": 3650, "maxTotalBytes": MIN_TOTAL_BYTES
            }))
            .unwrap(),
            CleanupPolicy {
                enabled: true,
                max_age_days: Some(3650),
                max_total_bytes: Some(MIN_TOTAL_BYTES as u64),
            }
        );
        assert!(CleanupPolicy::from_request(
            json!({"enabled": true, "maxTotalBytes": MAX_TOTAL_BYTES})
        )
        .is_ok());
        // Disabled policies may keep thresholds without being applied.
        assert!(CleanupPolicy::from_request(json!({"enabled": false, "maxAgeDays": 7})).is_ok());
        assert!(invalid(json!({"enabled": true})));
        assert!(invalid(
            json!({"enabled": true, "maxAgeDays": null, "maxTotalBytes": null})
        ));
        assert!(invalid(json!({"maxAgeDays": 0})));
        assert!(invalid(json!({"maxAgeDays": 3651})));
        assert!(invalid(json!({"maxAgeDays": -1})));
        assert!(invalid(json!({"maxAgeDays": 1.5})));
        assert!(invalid(json!({"maxTotalBytes": MIN_TOTAL_BYTES - 1})));
        assert!(invalid(json!({"maxTotalBytes": MAX_TOTAL_BYTES + 1})));
        assert!(invalid(json!({"enabled": "yes"})));
        assert!(invalid(json!([])));
    }

    #[tokio::test]
    async fn state_round_trips_and_survives_corruption() {
        let dir = temp_dir("history-cleanup-store");
        let cleanup = HistoryCleanup::load(&dir);
        assert_eq!(cleanup.tenant("local").await, TenantCleanup::default());
        let policy = CleanupPolicy {
            enabled: true,
            max_age_days: Some(30),
            max_total_bytes: None,
        };
        cleanup.set_policy("local", policy).await.unwrap();
        let run = CleanupRun {
            at: "2026-10-11T08:00:00Z".parse().unwrap(),
            trigger: CleanupTrigger::Manual,
            deleted: 2,
            freed_bytes: 4096,
            error: None,
        };
        cleanup.record_run("local", run.clone()).await.unwrap();

        let path = dir.join(HISTORY_CLEANUP_FILE);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
                0o600
            );
        }
        let reloaded = HistoryCleanup::load(&dir);
        let tenant = reloaded.tenant("local").await;
        assert_eq!(tenant.policy, policy);
        assert_eq!(tenant.last_run, Some(run));
        assert_eq!(reloaded.tenant("other").await, TenantCleanup::default());
        assert_eq!(
            reloaded.enabled_policies().await,
            vec![("local".to_owned(), policy)]
        );
        // No scheduler is running here, so no pass time is known.
        let view = reloaded.view(&tenant);
        assert_eq!(view["policy"]["maxAgeDays"], 30);
        assert_eq!(view["policy"]["maxTotalBytes"], Value::Null);
        assert_eq!(view["lastRun"]["trigger"], "manual");
        assert_eq!(view["lastRun"]["freedBytes"], 4096);
        assert_eq!(view["lastRun"]["error"], Value::Null);
        assert_eq!(view["nextRunAt"], Value::Null);

        std::fs::write(&path, b"{not json").unwrap();
        let corrupt = HistoryCleanup::load(&dir);
        assert_eq!(corrupt.tenant("local").await, TenantCleanup::default());
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[tokio::test]
    async fn next_run_is_reported_only_for_enabled_policies() {
        let dir = temp_dir("history-cleanup-next");
        let cleanup = HistoryCleanup::load(&dir);
        let planned = Utc::now() + chrono::Duration::days(1);
        cleanup.set_next_pass(planned);
        let disabled = cleanup.tenant("local").await;
        assert_eq!(cleanup.view(&disabled)["nextRunAt"], Value::Null);
        let enabled = cleanup
            .set_policy(
                "local",
                CleanupPolicy {
                    enabled: true,
                    max_age_days: None,
                    max_total_bytes: Some(MIN_TOTAL_BYTES as u64),
                },
            )
            .await
            .unwrap();
        // Enabling asks for a prompt pass instead of waiting for the planned one.
        let next = cleanup.next_pass_at().unwrap();
        assert!(next < planned);
        assert_eq!(cleanup.view(&enabled)["nextRunAt"], json!(next));
        std::fs::remove_dir_all(dir).unwrap();
    }
}
