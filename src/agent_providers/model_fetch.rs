//! Server-side model catalog fetch for a stored provider. Keeps API keys on
//! the daemon: clients receive the model list without ever seeing the key.

use serde_json::{json, Value};
use std::sync::OnceLock;
use std::time::Duration;

use crate::conversation::ProviderKind;
use crate::error::AppError;

const MODELS_TIMEOUT: Duration = Duration::from_secs(15);
const MAX_MODELS: usize = 500;
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);

/// Process-wide client for model catalog requests, so each listing reuses the
/// connection pool and TLS setup. Redirects are not followed: reqwest only
/// strips `Authorization` on a cross-host redirect, not `x-api-key`. Callers
/// still set a per-request timeout.
pub(crate) fn http_client() -> Result<&'static reqwest::Client, AppError> {
    static CLIENT: OnceLock<reqwest::Client> = OnceLock::new();
    if let Some(client) = CLIENT.get() {
        return Ok(client);
    }
    let client = reqwest::Client::builder()
        .connect_timeout(CONNECT_TIMEOUT)
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .map_err(|error| {
            AppError::ProviderUnavailable(format!("HTTP client unavailable: {error}"))
        })?;
    Ok(CLIENT.get_or_init(|| client))
}

/// `(base_url, api_key)` for a stored profile's settingsConfig.
fn endpoint_credentials(agent: ProviderKind, settings: &Value) -> (Option<String>, Option<String>) {
    super::projection(agent)
        .map(|projection| projection.endpoint_credentials(settings))
        .unwrap_or((None, None))
}

/// Whether the endpoint speaks Anthropic's API: Claude Code, and DeepSeek
/// Harness routes on the Anthropic Messages protocol.
fn anthropic_style(agent: ProviderKind, settings: &Value) -> bool {
    match agent {
        ProviderKind::ClaudeCode => true,
        ProviderKind::DeepseekHarness => super::dsh::anthropic_style(settings),
        _ => false,
    }
}

fn models_url(anthropic: bool, base_url: &str) -> String {
    let base = base_url.trim_end_matches('/');
    // Anthropic-style endpoints mount the catalog at /v1/models; third-party
    // gateways usually already include the /v1 segment.
    if anthropic && !base.ends_with("/v1") {
        format!("{base}/v1/models")
    } else {
        format!("{base}/models")
    }
}

/// Fetch `GET {base}/models` with the profile's credentials and normalize the
/// response to `[{id, name}]`.
pub async fn fetch_models(agent: ProviderKind, settings: &Value) -> Result<Value, AppError> {
    let (base_url, api_key) = endpoint_credentials(agent, settings);
    let base_url = base_url
        .map(|value| value.trim().to_owned())
        .filter(|value| !value.is_empty())
        .ok_or_else(|| {
            AppError::InvalidRequest(format!(
                "{} provider has no base URL in settingsConfig",
                agent.as_str()
            ))
        })?;
    let anthropic = anthropic_style(agent, settings);
    let url = models_url(anthropic, &base_url);

    let mut request = http_client()?.get(&url);
    if let Some(key) = api_key.filter(|key| !key.is_empty()) {
        request = request
            .header("authorization", format!("Bearer {key}"))
            .header("x-api-key", &key);
    }
    if anthropic {
        request = request.header("anthropic-version", "2023-06-01");
    }
    let response = request
        .timeout(MODELS_TIMEOUT)
        .send()
        .await
        .map_err(|error| AppError::ProviderUnavailable(format!("model listing failed: {error}")))?;
    if !response.status().is_success() {
        return Err(AppError::ProviderUnavailable(format!(
            "model listing returned HTTP {}",
            response.status()
        )));
    }
    let payload: Value = response.json().await.map_err(|error| {
        AppError::ProviderUnavailable(format!("model listing response invalid: {error}"))
    })?;

    // Accept the common `{data: [{id, ...}]}` shape plus a bare `[{id}]` array.
    let entries = payload
        .get("data")
        .or_else(|| payload.get("models"))
        .unwrap_or(&payload)
        .as_array()
        .cloned()
        .unwrap_or_default();
    let models: Vec<Value> = entries
        .iter()
        .filter_map(|item| {
            let id = item.get("id").and_then(Value::as_str)?.to_owned();
            let name = item
                .get("display_name")
                .or_else(|| item.get("displayName"))
                .or_else(|| item.get("name"))
                .and_then(Value::as_str)
                .unwrap_or(&id)
                .to_owned();
            Some(json!({ "id": id, "name": name }))
        })
        .take(MAX_MODELS)
        .collect();
    Ok(json!({ "models": models }))
}

#[cfg(test)]
mod tests {
    use super::{anthropic_style, models_url};
    use crate::conversation::ProviderKind;
    use serde_json::json;

    #[test]
    fn claude_models_url_avoids_doubling_v1() {
        assert_eq!(
            models_url(true, "https://api.anthropic.com"),
            "https://api.anthropic.com/v1/models"
        );
        assert_eq!(
            models_url(true, "https://gw.example.com/v1/"),
            "https://gw.example.com/v1/models"
        );
        assert_eq!(
            models_url(false, "https://gw.example.com/v1"),
            "https://gw.example.com/v1/models"
        );
    }

    #[test]
    fn dsh_routes_follow_their_wire_protocol() {
        let route = |api: &str| json!({"api": api});
        assert!(anthropic_style(
            ProviderKind::DeepseekHarness,
            &route("anthropic-messages")
        ));
        assert!(!anthropic_style(
            ProviderKind::DeepseekHarness,
            &route("openai-completions")
        ));
        assert!(anthropic_style(ProviderKind::ClaudeCode, &json!({})));
        assert!(!anthropic_style(ProviderKind::Codex, &json!({})));
    }
}
