//! Latest account-level plan quota snapshot per provider.
//!
//! Provider drivers emit `quota.updated` events during turns (Claude's
//! `rate_limit_event`, Codex's `account/rateLimits/updated`); the REST endpoint
//! `/v2/providers/quota` additionally refreshes Codex on demand. Both paths
//! land here so clients always read the newest known snapshot.

use std::sync::Arc;

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
}

impl QuotaStore {
    /// Records a provider's newest quota snapshot, stamping it with the
    /// daemon's fetch time. Payloads without a `provider` string are ignored —
    /// they cannot be keyed.
    pub fn record(&self, payload: &Value) {
        let Some(provider) = payload.get("provider").and_then(Value::as_str) else {
            return;
        };
        let mut entry = payload.clone();
        entry["fetchedAt"] = json!(now_ms());
        entry["state"] = json!("ok");
        self.entries.insert(provider.to_owned(), entry);
    }

    pub fn get(&self, provider: &str) -> Option<Value> {
        self.entries.get(provider).map(|entry| entry.clone())
    }
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
    fn record_ignores_unkeyed_payloads() {
        let store = QuotaStore::default();
        store.record(&json!({"windows": []}));
        assert!(store.get("claude-code").is_none());
    }
}
