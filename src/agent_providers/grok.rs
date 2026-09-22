//! Grok Build live config adapter.
//!
//! Exclusive mode sharing the Codex `{auth, config}` shape: `auth` is the
//! whole `$GROK_HOME/auth.json` (`$GROK_AUTH_PATH` when set) and `config` is
//! the full `$GROK_HOME/config.toml` text. An official-subscription profile
//! carries the session scopes `grok login` writes into `auth.json`; an API
//! profile carries no `auth` and declares `[model.<id>]` with `api_key` (and
//! optionally `base_url`/`api_backend`) selected by `[models].default`, which
//! Grok resolves ahead of any session token.

use std::path::PathBuf;

use serde_json::{json, Map, Value};
use toml_edit::{DocumentMut, Item};

use super::auth_config::{self, PairedFiles};
use super::{AgentDirs, MASKED_SECRET};
use crate::error::AppError;

/// `extra_headers` values are sent verbatim (e.g. Anthropic `x-api-key`);
/// `env_http_headers` holds variable names and stays readable.
pub const HEADER_TABLES: &[&str] = &["extra_headers"];

/// Public xAI API endpoint Grok uses for built-in models and API keys.
const XAI_API_BASE_URL: &str = "https://api.x.ai/v1";

/// `auth.json` scope `grok login --api-key` writes a plain API key under.
const API_KEY_SCOPE: &str = "xai::api_key";

pub fn config_path(dirs: &AgentDirs) -> PathBuf {
    dirs.grok_home.join("config.toml")
}

fn paired_files(dirs: &AgentDirs) -> PairedFiles {
    PairedFiles {
        config: config_path(dirs),
        auth: dirs.grok_auth_path.clone(),
        agent_label: "Grok Build",
    }
}

pub fn read_live(dirs: &AgentDirs) -> Result<Option<Value>, AppError> {
    auth_config::read_live(&paired_files(dirs))
}

pub fn write_live(dirs: &AgentDirs, settings: &Value, remove_auth: bool) -> Result<(), AppError> {
    auth_config::write_live(&paired_files(dirs), settings, remove_auth)
}

/// Every `auth.json` scope stores its bearer under `key`, which the generic
/// field-name heuristic does not treat as secret.
pub fn masked_settings(settings: &Value) -> Value {
    let mut masked = auth_config::masked_settings(settings, HEADER_TABLES);
    if let Some(scopes) = masked.get_mut("auth").and_then(Value::as_object_mut) {
        for entry in scopes.values_mut() {
            if let Some(key) = entry.get_mut("key").filter(|key| key.is_string()) {
                *key = json!(MASKED_SECRET);
            }
        }
    }
    masked
}

/// Which accounts an `auth` object signs in, ignoring the session tokens and
/// timestamps Grok rotates in place during background refresh.
fn auth_identity(settings: &Value) -> Option<Map<String, Value>> {
    let scopes = settings.get("auth")?.as_object()?;
    Some(
        scopes
            .iter()
            .map(|(scope, entry)| {
                let mode = entry.get("auth_mode").cloned().unwrap_or(Value::Null);
                // A plain API key is its own identity; session keys rotate.
                let key = if mode == "api_key" {
                    entry.get("key").cloned().unwrap_or(Value::Null)
                } else {
                    Value::Null
                };
                let user = entry.get("user_id").cloned().unwrap_or(Value::Null);
                (
                    scope.clone(),
                    json!({ "authMode": mode, "userId": user, "key": key }),
                )
            })
            .collect(),
    )
}

/// The live files still describe this profile: same config document and the
/// same signed-in accounts (token refreshes alone are not drift).
pub fn live_matches(live: &Value, settings: &Value) -> bool {
    auth_config::config_matches(live, settings) && auth_identity(live) == auth_identity(settings)
}

/// When `settings` keeps the stored auth unchanged and the live `auth.json`
/// is signed in to the same accounts, carry the live (freshly refreshed)
/// tokens forward so rewriting the live files cannot resurrect stale or
/// already-rotated refresh tokens.
pub fn carry_live_auth(settings: &mut Value, stored: &Value, live: Option<&Value>) {
    let Some(live) = live else {
        return;
    };
    let Some(live_auth) = live.get("auth").filter(|auth| auth.is_object()) else {
        return;
    };
    if settings.get("auth") != stored.get("auth") || auth_identity(live) != auth_identity(stored) {
        return;
    }
    if let Some(object) = settings.as_object_mut() {
        object.insert("auth".to_owned(), live_auth.clone());
    }
}

fn item_str(item: Option<&Item>, key: &str) -> Option<String> {
    item.and_then(|item| item.get(key))
        .and_then(Item::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_owned)
}

/// Extract `(base_url, api_key)` for the model-listing proxy call from the
/// `[models].default` model entry, its `model_provider`, `[endpoints]`, and
/// finally a stored `xai::api_key`. Session tokens are never forwarded: they
/// only authenticate against Grok's own chat proxy.
pub fn endpoint_credentials(settings: &Value) -> (Option<String>, Option<String>) {
    let doc = settings
        .get("config")
        .and_then(Value::as_str)
        .and_then(|text| text.parse::<DocumentMut>().ok());
    let doc = doc.as_ref();
    let default_model = doc.and_then(|doc| item_str(doc.get("models"), "default"));
    let model = default_model
        .as_deref()
        .and_then(|name| doc?.get("model")?.get(name));
    let provider =
        item_str(model, "model_provider").and_then(|name| doc?.get("model_providers")?.get(&name));

    let api_key = item_str(model, "api_key")
        .or_else(|| item_str(provider, "api_key"))
        .or_else(|| {
            settings
                .get("auth")
                .and_then(|auth| auth.get(API_KEY_SCOPE))
                .and_then(|scope| scope.get("key"))
                .and_then(Value::as_str)
                .filter(|key| !key.is_empty())
                .map(str::to_owned)
        });
    let base_url = item_str(model, "base_url")
        .or_else(|| item_str(provider, "base_url"))
        .or_else(|| doc.and_then(|doc| item_str(doc.get("endpoints"), "models_base_url")))
        .or_else(|| api_key.as_ref().map(|_| XAI_API_BASE_URL.to_owned()));
    (base_url, api_key)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn session_auth(user: &str, key: &str) -> Value {
        json!({
            "https://auth.x.ai": {
                "key": key,
                "auth_mode": "oidc",
                "create_time": "2026-09-01T00:00:00Z",
                "user_id": user,
                "email": format!("{user}@example.com"),
                "refresh_token": format!("refresh-{key}")
            }
        })
    }

    #[test]
    fn masks_scope_keys_refresh_tokens_and_toml_secrets() {
        let settings = json!({
            "auth": session_auth("u1", "tok-secret"),
            "config": "[models]\ndefault = \"claude\"\n\n[model.claude]\nmodel = \"claude-opus\"\napi_key = \"sk-model\"\nextra_headers = { \"x-api-key\" = \"sk-header\", \"anthropic-version\" = \"2023-06-01\" }\nenv_http_headers = { \"X-Tenant\" = \"TENANT_VAR\" }\nenv_key = \"ANTHROPIC_KEY\"\n"
        });
        let masked = masked_settings(&settings);
        let scope = &masked["auth"]["https://auth.x.ai"];
        assert_eq!(scope["key"], MASKED_SECRET);
        assert_eq!(scope["refresh_token"], MASKED_SECRET);
        assert_eq!(scope["email"], "u1@example.com");
        let config = masked["config"].as_str().unwrap();
        assert!(!config.contains("sk-model"));
        assert!(!config.contains("sk-header"));
        assert!(!config.contains("2023-06-01"));
        assert!(config.contains("TENANT_VAR"));
        assert!(config.contains("env_key = \"ANTHROPIC_KEY\""));

        let restored = auth_config::restore_config_text(
            config,
            settings["config"].as_str().unwrap(),
            HEADER_TABLES,
        );
        assert!(restored.contains("sk-model"));
        assert!(restored.contains("sk-header"));
    }

    #[test]
    fn token_refresh_is_not_drift_but_account_change_is() {
        let config = "[models]\ndefault = \"grok-build\"\n";
        let stored = json!({ "auth": session_auth("u1", "old"), "config": config });
        let refreshed = json!({ "auth": session_auth("u1", "new"), "config": config });
        let other = json!({ "auth": session_auth("u2", "new"), "config": config });
        assert!(live_matches(&refreshed, &stored));
        assert!(!live_matches(&other, &stored));
        let no_auth = json!({ "auth": null, "config": config });
        assert!(live_matches(&no_auth, &json!({ "config": config })));
    }

    #[test]
    fn carries_fresh_live_tokens_only_for_the_same_account() {
        let stored = json!({ "auth": session_auth("u1", "old"), "config": "" });
        let live = json!({ "auth": session_auth("u1", "new"), "config": "" });
        let mut edit = json!({ "auth": session_auth("u1", "old"), "config": "x = 1" });
        carry_live_auth(&mut edit, &stored, Some(&live));
        assert_eq!(edit["auth"]["https://auth.x.ai"]["key"], "new");

        let other = json!({ "auth": session_auth("u2", "other"), "config": "" });
        let mut edit = json!({ "auth": session_auth("u1", "old"), "config": "" });
        carry_live_auth(&mut edit, &stored, Some(&other));
        assert_eq!(edit["auth"]["https://auth.x.ai"]["key"], "old");

        // An explicit auth change in the edit always wins over live.
        let mut edit = json!({ "auth": null, "config": "" });
        carry_live_auth(&mut edit, &stored, Some(&live));
        assert!(edit["auth"].is_null());
    }

    #[test]
    fn endpoint_credentials_follow_default_model() {
        let settings = json!({
            "auth": null,
            "config": "[models]\ndefault = \"gw\"\n\n[model.gw]\nmodel = \"m\"\nmodel_provider = \"acme\"\napi_key = \"k-model\"\n\n[model_providers.acme]\nbase_url = \"https://acme.example.com/v1\"\n"
        });
        assert_eq!(
            endpoint_credentials(&settings),
            (
                Some("https://acme.example.com/v1".to_owned()),
                Some("k-model".to_owned())
            )
        );

        // A bare key override of a built-in model targets the xAI API.
        let settings = json!({
            "config": "[models]\ndefault = \"grok-4.7\"\n\n[model.\"grok-4.7\"]\napi_key = \"xai-k\"\n"
        });
        assert_eq!(
            endpoint_credentials(&settings),
            (Some(XAI_API_BASE_URL.to_owned()), Some("xai-k".to_owned()))
        );

        // Session-only profiles expose no public-API credentials.
        let settings = json!({ "auth": session_auth("u1", "tok"), "config": "" });
        assert_eq!(endpoint_credentials(&settings), (None, None));
    }
}
