//! The permission mode Claude Code actually runs in, and why it may differ
//! from the one TodeX asked for.
//!
//! Claude Code quietly replaces a requested mode it will not honour: a
//! settings layer with `permissions.disableBypassPermissionsMode: "disable"`
//! turns `bypassPermissions` into the settings' `defaultMode`, and
//! `permissions.disableAutoMode: "disable"` (or a model/account without auto
//! mode) turns `auto` into another mode. Nothing reaches stderr. The
//! `initialize` response reports `current_permission_mode`; when it differs
//! from the requested mode, a `get_settings` control request names the
//! settings layer responsible so the error can point at it.

use serde_json::{json, Value};

/// `request_id` of the `get_settings` probe, at initialize or mid-turn.
pub(super) const SETTINGS_PROBE_ID: &str = "todex-get-settings";

/// What a process reported about its permission mode at start-up. Kept on
/// the process, so every turn on it (a prewarmed one included) can explain
/// a mode Claude refused.
#[derive(Clone, Debug, Default)]
pub(in crate::provider) struct ModeReport {
    /// `current_permission_mode` from the `initialize` response; CLI
    /// releases that predate it leave this unset.
    pub reported: Option<String>,
    /// Set once `get_settings` answered (or failed): the layers disabling
    /// bypass and auto mode.
    pub settings: Option<DisablingSources>,
}

impl ModeReport {
    /// Whether to ask `get_settings` now: the CLI reports its mode (so it
    /// understands the request) and nobody asked yet.
    pub fn can_probe(&self) -> bool {
        self.reported.is_some() && self.settings.is_none()
    }

    pub fn bypass_source(&self) -> Option<&str> {
        self.settings.as_ref()?.bypass.as_deref()
    }

    pub fn auto_source(&self) -> Option<&str> {
        self.settings.as_ref()?.auto.as_deref()
    }
}

/// The settings sources (Claude's names, e.g. `userSettings`,
/// `projectSettings`, `localSettings`, `policySettings`, `flagSettings`)
/// that disable a permission mode.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(in crate::provider) struct DisablingSources {
    pub bypass: Option<String>,
    pub auto: Option<String>,
}

impl DisablingSources {
    /// Reads a `get_settings` control response body (`{effective, sources:
    /// [{source, settings}], applied}`). Managed policy wins because no other
    /// layer can turn it off; otherwise the last (highest-precedence) layer.
    pub fn from_settings(response: &Value) -> Self {
        let sources = response
            .get("sources")
            .and_then(Value::as_array)
            .map(Vec::as_slice)
            .unwrap_or_default();
        let find = |key: &str| {
            let mut found: Option<&str> = None;
            for entry in sources {
                let Some(source) = entry.get("source").and_then(Value::as_str) else {
                    continue;
                };
                let disabled = entry
                    .pointer(&format!("/settings/permissions/{key}"))
                    .and_then(Value::as_str)
                    == Some("disable");
                if disabled && found != Some("policySettings") {
                    found = Some(source);
                }
            }
            found.map(str::to_owned)
        };
        Self {
            bypass: find("disableBypassPermissionsMode"),
            auto: find("disableAutoMode"),
        }
    }
}

/// Whether `initialize` reported a mode that needs explaining: Claude
/// refused the requested bypass or auto mode.
pub(super) fn needs_settings_probe(expected: Option<&str>, reported: &str) -> bool {
    matches!(expected, Some("auto" | "bypassPermissions")) && expected != Some(reported)
}

/// The `get_settings` control request.
pub(super) fn settings_probe() -> Value {
    json!({
        "type": "control_request",
        "request_id": SETTINGS_PROBE_ID,
        "request": { "subtype": "get_settings" },
    })
}

/// The answer to [`settings_probe`] inside a `control_response` frame, or
/// `None` for another frame. A failed probe names no source.
pub(super) fn settings_probe_answer(message: &Value) -> Option<DisablingSources> {
    if message.get("type").and_then(Value::as_str) != Some("control_response")
        || message
            .pointer("/response/request_id")
            .and_then(Value::as_str)
            != Some(SETTINGS_PROBE_ID)
    {
        return None;
    }
    if message.pointer("/response/subtype").and_then(Value::as_str) != Some("success") {
        return Some(DisablingSources::default());
    }
    Some(
        message
            .pointer("/response/response")
            .map(DisablingSources::from_settings)
            .unwrap_or_default(),
    )
}

/// Claude refused the requested full access. Never downgraded: the user
/// asked for a mode with no approvals, and silently running another one
/// would change what the turn may do without them knowing.
pub(super) fn bypass_mode_refused(expected: Option<&str>, actual: &str) -> bool {
    expected == Some("bypassPermissions") && actual != "bypassPermissions"
}

/// The error text (and event `reason`) for [`bypass_mode_refused`].
pub(super) fn bypass_refused_reason(source: Option<&str>, actual: &str) -> String {
    match source {
        Some(source) => format!(
            "Claude Code settings ({source}) disable full access (permissions.disableBypassPermissionsMode). Turn that setting off in CLI settings to use full access."
        ),
        None => format!(
            "Claude Code refused full access (bypassPermissions); reported '{actual}'. Check permissions.disableBypassPermissionsMode in your Claude Code settings or managed policy."
        ),
    }
}

/// The `provider.event` recorded before a turn fails on
/// [`bypass_mode_refused`].
pub(super) fn bypass_mode_disabled_event(actual: &str, source: Option<&str>) -> Value {
    json!({
        "provider": "claude-code",
        "providerMethod": "bypass_mode_disabled",
        "metadata": {
            "requestedMode": "full-access",
            "reportedMode": actual,
            "settingsSource": source,
            "reason": bypass_refused_reason(source, actual),
        },
    })
}

/// The TodeX mode a turn continues in when Claude replaced the requested
/// `auto` with `actual`, or `None` when it cannot continue.
///
/// Claude falls back to the settings' `defaultMode`. `default` and
/// `acceptEdits` both ask through `can_use_tool` for anything they do not
/// allow on their own, so they are never broader than `auto` and the turn
/// continues (TodeX's own tools ask, as in `ask`). `plan` would silently turn
/// an implement turn into a planning one and `dontAsk` denies every tool not
/// pre-allowed without asking, so those fail the turn instead, as does
/// `bypassPermissions`, which is broader than what was requested.
pub(super) fn auto_fallback_mode(actual: &str) -> Option<&'static str> {
    match actual {
        "default" => Some("ask"),
        "acceptEdits" => Some("accept-edits"),
        _ => None,
    }
}

/// The error for a requested mode Claude did not activate and TodeX cannot
/// continue in.
pub(super) fn mode_mismatch_reason(
    expected: Option<&str>,
    actual: &str,
    auto_source: Option<&str>,
) -> String {
    match (expected, auto_source) {
        (Some("auto"), Some(source)) => format!(
            "Claude Code settings ({source}) disable auto mode (permissions.disableAutoMode) and it fell back to '{actual}'. Turn that setting off in CLI settings, or choose another permission mode."
        ),
        _ => format!(
            "Claude Code did not activate requested permission mode '{}'; reported '{}'. Check model, account and organization permissions.",
            expected.unwrap_or("default"),
            actual
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_a_refused_bypass_counts_as_refused() {
        assert!(bypass_mode_refused(Some("bypassPermissions"), "auto"));
        assert!(bypass_mode_refused(Some("bypassPermissions"), "default"));
        assert!(!bypass_mode_refused(
            Some("bypassPermissions"),
            "bypassPermissions"
        ));
        assert!(!bypass_mode_refused(Some("auto"), "default"));
        assert!(!bypass_mode_refused(None, "default"));
    }

    #[test]
    fn a_probe_is_needed_only_for_a_refused_bypass_or_auto() {
        assert!(needs_settings_probe(Some("bypassPermissions"), "auto"));
        assert!(needs_settings_probe(Some("auto"), "default"));
        assert!(!needs_settings_probe(Some("auto"), "auto"));
        assert!(!needs_settings_probe(Some("default"), "auto"));
        assert!(!needs_settings_probe(Some("plan"), "default"));
        assert!(!needs_settings_probe(None, "default"));
    }

    #[test]
    fn auto_continues_only_in_modes_that_still_ask() {
        assert_eq!(auto_fallback_mode("default"), Some("ask"));
        assert_eq!(auto_fallback_mode("acceptEdits"), Some("accept-edits"));
        for actual in ["plan", "dontAsk", "bypassPermissions", "auto", "bogus"] {
            assert_eq!(auto_fallback_mode(actual), None, "{actual}");
        }
    }

    #[test]
    fn the_disabling_layer_is_found_with_policy_first() {
        let settings = json!({
            "effective": {},
            "sources": [
                { "source": "userSettings", "settings": { "permissions": { "disableAutoMode": "disable" } } },
                { "source": "policySettings", "settings": { "permissions": { "disableAutoMode": "disable" } } },
                { "source": "projectSettings", "settings": { "permissions": {
                    "disableAutoMode": "disable",
                    "disableBypassPermissionsMode": "disable",
                } } },
                { "source": "localSettings", "settings": { "permissions": { "disableBypassPermissionsMode": "enable" } } },
            ],
        });
        assert_eq!(
            DisablingSources::from_settings(&settings),
            DisablingSources {
                bypass: Some("projectSettings".to_owned()),
                auto: Some("policySettings".to_owned()),
            }
        );
        assert_eq!(
            DisablingSources::from_settings(&json!({})),
            DisablingSources::default()
        );
    }

    #[test]
    fn probe_answers_are_matched_by_request_id() {
        let ok = json!({ "type": "control_response", "response": {
            "subtype": "success", "request_id": SETTINGS_PROBE_ID,
            "response": { "sources": [{ "source": "flagSettings", "settings": {
                "permissions": { "disableBypassPermissionsMode": "disable" } } }] },
        } });
        assert_eq!(
            settings_probe_answer(&ok).unwrap().bypass.as_deref(),
            Some("flagSettings")
        );
        let failed = json!({ "type": "control_response", "response": {
            "subtype": "error", "request_id": SETTINGS_PROBE_ID, "error": "unknown",
        } });
        assert_eq!(
            settings_probe_answer(&failed),
            Some(DisablingSources::default())
        );
        let other = json!({ "type": "control_response", "response": {
            "subtype": "success", "request_id": "todex-initialize", "response": {},
        } });
        assert_eq!(settings_probe_answer(&other), None);
    }

    #[test]
    fn the_bypass_event_carries_the_source_and_the_error_text() {
        let event = bypass_mode_disabled_event("auto", Some("userSettings"));
        assert_eq!(
            event,
            json!({
                "provider": "claude-code",
                "providerMethod": "bypass_mode_disabled",
                "metadata": {
                    "requestedMode": "full-access",
                    "reportedMode": "auto",
                    "settingsSource": "userSettings",
                    "reason": "Claude Code settings (userSettings) disable full access (permissions.disableBypassPermissionsMode). Turn that setting off in CLI settings to use full access.",
                },
            })
        );
        let unknown = bypass_mode_disabled_event("default", None);
        assert!(unknown["metadata"]["settingsSource"].is_null());
        assert_eq!(
            unknown["metadata"]["reason"],
            "Claude Code refused full access (bypassPermissions); reported 'default'. Check permissions.disableBypassPermissionsMode in your Claude Code settings or managed policy."
        );
    }
}
