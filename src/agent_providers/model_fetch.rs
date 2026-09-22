//! Server-side model catalog fetch for a stored provider. Keeps API keys on
//! the daemon: clients receive the model list without ever seeing the key.

use serde_json::{json, Value};
use std::time::Duration;

use crate::conversation::ProviderKind;
use crate::error::AppError;

use super::{codex, grok, opencode, pi};

const MODELS_TIMEOUT: Duration = Duration::from_secs(15);
const MAX_MODELS: usize = 500;

/// `(base_url, api_key)` for a stored profile's settingsConfig.
fn endpoint_credentials(agent: ProviderKind, settings: &Value) -> (Option<String>, Option<String>) {
    match agent {
        ProviderKind::Codex => codex::endpoint_credentials(settings),
        ProviderKind::GrokBuild => grok::endpoint_credentials(settings),
        ProviderKind::Opencode => opencode::endpoint_credentials(settings),
        ProviderKind::Pi => pi::endpoint_credentials(settings),
        ProviderKind::ClaudeCode => {
            let env = settings.get("env");
            (
                env.and_then(|env| env.get("ANTHROPIC_BASE_URL"))
                    .and_then(Value::as_str)
                    .map(str::to_owned),
                env.and_then(|env| {
                    env.get("ANTHROPIC_AUTH_TOKEN")
                        .or_else(|| env.get("ANTHROPIC_API_KEY"))
                })
                .and_then(Value::as_str)
                .map(str::to_owned),
            )
        }
        _ => (None, None),
    }
}

fn models_url(agent: ProviderKind, base_url: &str) -> String {
    let base = base_url.trim_end_matches('/');
    match agent {
        // Anthropic-style endpoints mount the catalog at /v1/models; third-party
        // gateways usually already include the /v1 segment.
        ProviderKind::ClaudeCode if base.ends_with("/v1") => format!("{base}/models"),
        ProviderKind::ClaudeCode => format!("{base}/v1/models"),
        _ => format!("{base}/models"),
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
    let url = models_url(agent, &base_url);

    let mut request = reqwest::Client::new().get(&url);
    if let Some(key) = api_key.filter(|key| !key.is_empty()) {
        request = request
            .header("authorization", format!("Bearer {key}"))
            .header("x-api-key", &key);
    }
    if agent == ProviderKind::ClaudeCode {
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
    use super::models_url;
    use crate::conversation::ProviderKind;

    #[test]
    fn claude_models_url_avoids_doubling_v1() {
        assert_eq!(
            models_url(ProviderKind::ClaudeCode, "https://api.anthropic.com"),
            "https://api.anthropic.com/v1/models"
        );
        assert_eq!(
            models_url(ProviderKind::ClaudeCode, "https://gw.example.com/v1/"),
            "https://gw.example.com/v1/models"
        );
        assert_eq!(
            models_url(ProviderKind::Codex, "https://gw.example.com/v1"),
            "https://gw.example.com/v1/models"
        );
    }
}
