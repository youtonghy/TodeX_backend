//! Latest account-level plan quota snapshot per provider.
//!
//! Provider drivers emit `quota.updated` events during turns (Claude's
//! `rate_limit_event`, Codex's `account/rateLimits/updated`); the REST endpoint
//! `/v2/providers/quota` additionally refreshes Codex on demand. Both paths
//! land here so clients always read the newest known snapshot.

use std::sync::Arc;

use chrono::{DateTime, TimeZone, Utc};
use dashmap::DashMap;
use serde_json::{json, Value};

/// Milliseconds since epoch; matches the timestamp style used elsewhere in
/// event payloads.
fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_millis() as u64)
        .unwrap_or(0)
}

#[derive(Clone, Default)]
pub struct QuotaStore {
    entries: Arc<DashMap<String, Value>>,
    /// Reset instant of an exhausted window, per turn that observed it. Kept
    /// per turn rather than read from `entries`: conversations sharing the
    /// account hit the limit together, and each overwrites the provider's
    /// latest snapshot. The turn's end takes its entry.
    exhausted_turns: Arc<DashMap<String, DateTime<Utc>>>,
}

impl QuotaStore {
    /// Records a provider's newest quota snapshot, stamping it with the
    /// daemon's fetch time. Payloads without a `provider` string are ignored —
    /// they cannot be keyed.
    pub fn record(&self, payload: &Value) {
        let Some(provider) = payload.get("provider").and_then(Value::as_str) else {
            return;
        };
        if let Some(turn_id) = payload.get("turnId").and_then(Value::as_str) {
            match exhausted_until(payload) {
                Some(until) => {
                    self.exhausted_turns.insert(turn_id.to_owned(), until);
                }
                None => {
                    self.exhausted_turns.remove(turn_id);
                }
            }
        }
        let mut entry = payload.clone();
        entry["fetchedAt"] = json!(now_ms());
        entry["state"] = json!("ok");
        self.entries.insert(provider.to_owned(), entry);
    }

    pub fn get(&self, provider: &str) -> Option<Value> {
        self.entries.get(provider).map(|entry| entry.clone())
    }

    /// Removes and returns when the plan window that `turn_id` last saw
    /// exhausted resets; `None` when the turn's latest snapshot had room left.
    pub fn take_turn_exhaustion(&self, turn_id: &str) -> Option<DateTime<Utc>> {
        self.exhausted_turns.remove(turn_id).map(|(_, until)| until)
    }
}

/// Reset instant of a snapshot whose plan limit is reached: Claude reports
/// `status: "rejected"`, Codex a window at 100 %. The latest reset among the
/// full windows decides; a rejection without one falls back to the top-level
/// `resetsAt`. `None` when nothing is exhausted or no reset time is known.
fn exhausted_until(payload: &Value) -> Option<DateTime<Utc>> {
    let windows = payload
        .get("windows")
        .and_then(Value::as_array)
        .map(Vec::as_slice)
        .unwrap_or_default();
    let full_window_reset = windows
        .iter()
        .filter(|window| {
            window
                .get("usedPercent")
                .and_then(Value::as_f64)
                .is_some_and(|percent| percent >= 100.0)
        })
        .filter_map(|window| reset_instant(window.get("resetsAt")?))
        .max();
    let rejected = payload.get("status").and_then(Value::as_str) == Some("rejected");
    full_window_reset.or_else(|| {
        rejected
            .then(|| payload.pointer("/raw/resetsAt"))
            .flatten()
            .and_then(reset_instant)
    })
}

/// Providers report epoch seconds; accept milliseconds as well.
fn reset_instant(value: &Value) -> Option<DateTime<Utc>> {
    let raw = value.as_i64()?;
    let millis = if raw > 100_000_000_000 {
        raw
    } else {
        raw.checked_mul(1000)?
    };
    Utc.timestamp_millis_opt(millis).single()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn record_keys_by_provider_and_stamps_fetch_time() {
        let store = QuotaStore::default();
        store.record(&json!({"provider": "claude-code", "windows": []}));
        let entry = store.get("claude-code").expect("entry recorded");
        assert_eq!(entry["state"], "ok");
        assert!(entry["fetchedAt"].as_u64().unwrap_or(0) > 0);
        assert!(store.get("codex").is_none());
    }

    #[test]
    fn exhausted_windows_are_remembered_per_turn() {
        let store = QuotaStore::default();
        let snapshot = |turn: &str, status: &str, five_hour: f64| {
            json!({
                "provider": "claude-code",
                "turnId": turn,
                "status": status,
                "windows": [
                    { "id": "five_hour", "usedPercent": five_hour, "resetsAt": 1791279000 },
                    { "id": "seven_day", "usedPercent": 41.0, "resetsAt": 1791493200 },
                ],
                "raw": { "resetsAt": 1791279000 },
            })
        };
        store.record(&snapshot("turn-a", "rejected", 101.0));
        // Another conversation's snapshot replaces the provider entry but
        // not turn-a's exhaustion.
        store.record(&snapshot("turn-b", "rejected", 101.0));
        store.record(&snapshot("turn-c", "allowed_warning", 99.0));
        let reset = Utc.timestamp_opt(1791279000, 0).single();
        assert_eq!(store.take_turn_exhaustion("turn-a"), reset);
        assert_eq!(store.take_turn_exhaustion("turn-a"), None);
        assert_eq!(store.take_turn_exhaustion("turn-c"), None);
        // A later snapshot with room left clears the turn's entry.
        store.record(&snapshot("turn-b", "allowed", 10.0));
        assert_eq!(store.take_turn_exhaustion("turn-b"), None);
    }

    #[test]
    fn exhaustion_reads_full_windows_and_rejections() {
        let full_codex = json!({
            "windows": [{ "id": "primary", "usedPercent": 100.0, "resetsAt": 1792694011 }],
        });
        assert_eq!(
            exhausted_until(&full_codex),
            Utc.timestamp_opt(1792694011, 0).single()
        );
        let bare_rejection = json!({
            "status": "rejected",
            "windows": [{ "id": "five_hour", "resetsAt": 1791279000 }],
            "raw": { "resetsAt": 1_791_279_000_000_i64 },
        });
        assert_eq!(
            exhausted_until(&bare_rejection),
            Utc.timestamp_opt(1791279000, 0).single()
        );
        assert_eq!(
            exhausted_until(&json!({ "status": "rejected", "windows": [] })),
            None
        );
        assert_eq!(
            exhausted_until(&json!({
                "windows": [{ "id": "five_hour", "usedPercent": 99.0, "resetsAt": 1 }],
            })),
            None
        );
    }

    #[test]
    fn record_ignores_unkeyed_payloads() {
        let store = QuotaStore::default();
        store.record(&json!({"windows": []}));
        assert!(store.get("claude-code").is_none());
    }
}
