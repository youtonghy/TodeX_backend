//! DeepSeek Harness live config adapter.
//!
//! Additive mode. A managed provider is one dsh model route in the `acp`
//! profile's patch layer (see [`crate::dsh_config`]):
//!
//! - `deepseek-official` is dsh's built-in DeepSeek route: its API key is the
//!   `DEEPSEEK_API_KEY` credential reference (shared with web search), and
//!   other fields (`baseURL`, …) are the `llm-deepseek` row's config.
//! - every other id is an `llm-pi-ai` route (`config.providers.<id>`), an
//!   OpenAI- or Anthropic-compatible endpoint whose key is the reference
//!   `TODEX_DSH_<ID>_API_KEY`. dsh scrubs `*KEY*` names from the environment
//!   of the commands it runs, so the key never reaches the agent's shell.
//!
//! A node is the route as dsh reads it plus a virtual `apiKey`, which lives
//! in `$DSH_HOME/.credentials.yaml` instead of the patch. Activation writes
//! the `acp` row's default `{provider, model}`.

use serde_json::{json, Map, Value};

use super::AgentDirs;
use crate::dsh_config::{
    credential_ref, edit_profile_patch, set_credential_refs, DshPaths, HomeLayer, PatchFile,
};
use crate::error::AppError;

/// dsh's built-in DeepSeek API route.
pub(crate) const DEEPSEEK_ROUTE: &str = "deepseek-official";
const DEEPSEEK_ROW: &str = "llm-deepseek";
const DEEPSEEK_KEY_REF: &str = "DEEPSEEK_API_KEY";
const ROUTES_ROW: &str = "llm-pi-ai";
const DEFAULT_ROW: &str = "acp";
/// Route ids dsh registers itself.
const RESERVED_ROUTES: [&str; 2] = [DEEPSEEK_ROUTE, "deepseek-account"];
/// Wire protocols `llm-pi-ai` routes may use.
pub(crate) const ROUTE_APIS: [&str; 3] = [
    "openai-completions",
    "openai-responses",
    "anthropic-messages",
];
/// Prefix of the credential references TodeX owns and may delete.
const OWNED_REF_PREFIX: &str = "TODEX_DSH_";

fn key_ref(id: &str) -> String {
    format!(
        "{OWNED_REF_PREFIX}{}_API_KEY",
        id.to_ascii_uppercase().replace('-', "_")
    )
}

fn valid_route_id(id: &str) -> bool {
    let mut chars = id.chars();
    chars.next().is_some_and(|first| first.is_ascii_lowercase())
        && chars.all(|char| char.is_ascii_lowercase() || char.is_ascii_digit() || char == '-')
}

/// The profile layer's entries, also when the file is not a plain block
/// list (YAML tags then read as plain values).
fn profile_entries(file: &PatchFile) -> Vec<Map<String, Value>> {
    if let Some(layout) = file.layout() {
        return layout.values().cloned().collect();
    }
    serde_yaml_ng::from_str::<serde_yaml_ng::Value>(&file.text)
        .ok()
        .and_then(|value| serde_json::to_value(value).ok())
        .and_then(|value| match value {
            Value::Array(entries) => Some(
                entries
                    .into_iter()
                    .filter_map(|entry| match entry {
                        Value::Object(map) => Some(map),
                        _ => None,
                    })
                    .collect(),
            ),
            _ => None,
        })
        .unwrap_or_default()
}

/// What the profile layer gives `key` of row `id` (the last patch wins).
fn row_value(entries: &[Map<String, Value>], id: &str, key: &str) -> Option<Value> {
    entries
        .iter()
        .rev()
        .filter(|entry| {
            !entry.contains_key("insert") && entry.get("id").and_then(Value::as_str) == Some(id)
        })
        .find_map(|entry| entry.get(key).cloned())
}

fn read_profile(dirs: &AgentDirs) -> Result<(DshPaths, Vec<Map<String, Value>>), AppError> {
    let paths = DshPaths::of(dirs);
    let file = PatchFile::read(&paths.profile_patch())?;
    let entries = profile_entries(&file);
    Ok((paths, entries))
}

fn routes(entries: &[Map<String, Value>]) -> Map<String, Value> {
    row_value(entries, ROUTES_ROW, "config")
        .and_then(|config| config.get("providers").cloned())
        .and_then(|providers| match providers {
            Value::Object(providers) => Some(providers),
            _ => None,
        })
        .unwrap_or_default()
}

pub fn provider_nodes(dirs: &AgentDirs) -> Result<Map<String, Value>, AppError> {
    let (paths, entries) = read_profile(dirs)?;
    let mut nodes = Map::new();
    let deepseek_config =
        row_value(&entries, DEEPSEEK_ROW, "config").and_then(|config| config.as_object().cloned());
    let deepseek_key = credential_ref(&paths, DEEPSEEK_KEY_REF)?;
    if deepseek_config.is_some() || deepseek_key.is_some() {
        let mut node = deepseek_config.unwrap_or_default();
        if let Some(key) = deepseek_key {
            node.insert("apiKey".to_owned(), Value::String(key));
        }
        nodes.insert(DEEPSEEK_ROUTE.to_owned(), Value::Object(node));
    }
    for (id, route) in routes(&entries) {
        let Value::Object(mut node) = route else {
            continue;
        };
        if let Some(name) = node.get("apiKeyEnv").and_then(Value::as_str) {
            if let Some(key) = credential_ref(&paths, name)? {
                node.remove("apiKeyEnv");
                node.insert("apiKey".to_owned(), Value::String(key));
            }
        }
        nodes.insert(id, Value::Object(node));
    }
    Ok(nodes)
}

fn validate_route(id: &str, node: &Map<String, Value>) -> Result<(), AppError> {
    if !valid_route_id(id) || RESERVED_ROUTES.contains(&id) {
        return Err(AppError::InvalidRequest(format!(
            "DeepSeek Harness route id must be lowercase letters, digits and '-', not a built-in route: {id}"
        )));
    }
    let api = node.get("api").and_then(Value::as_str).unwrap_or_default();
    if !ROUTE_APIS.contains(&api) {
        return Err(AppError::InvalidRequest(format!(
            "DeepSeek Harness route api must be one of {}",
            ROUTE_APIS.join(", ")
        )));
    }
    let base = node
        .get("baseURL")
        .and_then(Value::as_str)
        .unwrap_or_default();
    if !(base.starts_with("https://") || base.starts_with("http://")) {
        return Err(AppError::InvalidRequest(
            "DeepSeek Harness route baseURL must be an http(s) URL".to_owned(),
        ));
    }
    let models = node.get("models").and_then(Value::as_array);
    let declared = models.is_some_and(|models| {
        !models.is_empty()
            && models.iter().all(|model| {
                model
                    .get("id")
                    .and_then(Value::as_str)
                    .is_some_and(|id| !id.trim().is_empty())
            })
    });
    if !declared {
        return Err(AppError::InvalidRequest(
            "DeepSeek Harness route needs at least one model, each with an id".to_owned(),
        ));
    }
    Ok(())
}

fn api_key(node: &Map<String, Value>) -> Option<String> {
    node.get("apiKey")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|key| !key.is_empty())
        .map(str::to_owned)
}

pub fn upsert_provider(dirs: &AgentDirs, id: &str, node: &Value) -> Result<(), AppError> {
    let Some(node) = node.as_object() else {
        return Err(AppError::InvalidRequest(
            "DeepSeek Harness provider settingsConfig must be an object".to_owned(),
        ));
    };
    let paths = DshPaths::of(dirs);
    let home = HomeLayer::read(&paths);
    let key = api_key(node);
    let mut config = node.clone();
    config.remove("apiKey");
    if id == DEEPSEEK_ROUTE {
        home.ensure_not_overridden(&paths, DEEPSEEK_ROW, "config")?;
        config.remove("apiKeyEnv");
        edit_profile_patch(&paths, None, |layout| {
            let value = (!config.is_empty()).then(|| Value::Object(config.clone()));
            layout.set_patch_key(DEEPSEEK_ROW, "config", value)
        })?;
        return set_credential_refs(&paths, &[(DEEPSEEK_KEY_REF, key.as_deref())]);
    }
    validate_route(id, &config)?;
    home.ensure_not_overridden(&paths, ROUTES_ROW, "config")?;
    let owned_ref = key_ref(id);
    if key.is_some() {
        config.insert("apiKeyEnv".to_owned(), Value::String(owned_ref.clone()));
    }
    // A route without a key keeps the reference it names (the user's own
    // variable), else it would list no models.
    let drop_owned = key.is_none()
        && config
            .get("apiKeyEnv")
            .and_then(Value::as_str)
            .is_none_or(|name| name == owned_ref);
    edit_profile_patch(&paths, None, |layout| {
        let mut routes_config = layout
            .patch_value(ROUTES_ROW, "config")
            .and_then(Value::as_object)
            .cloned()
            .unwrap_or_default();
        let providers = routes_config
            .entry("providers")
            .or_insert_with(|| json!({}));
        if !providers.is_object() {
            *providers = json!({});
        }
        providers
            .as_object_mut()
            .expect("providers normalized to an object")
            .insert(id.to_owned(), Value::Object(config.clone()));
        layout.set_patch_key(ROUTES_ROW, "config", Some(Value::Object(routes_config)))
    })?;
    match key {
        Some(key) => set_credential_refs(&paths, &[(&owned_ref, Some(&key))]),
        None if drop_owned => set_credential_refs(&paths, &[(&owned_ref, None)]),
        None => Ok(()),
    }
}

pub fn remove_provider(dirs: &AgentDirs, id: &str) -> Result<(), AppError> {
    let paths = DshPaths::of(dirs);
    let home = HomeLayer::read(&paths);
    if id == DEEPSEEK_ROUTE {
        home.ensure_not_overridden(&paths, DEEPSEEK_ROW, "config")?;
        edit_profile_patch(&paths, None, |layout| {
            layout.set_patch_key(DEEPSEEK_ROW, "config", None)
        })?;
        set_credential_refs(&paths, &[(DEEPSEEK_KEY_REF, None)])?;
    } else {
        home.ensure_not_overridden(&paths, ROUTES_ROW, "config")?;
        let mut referenced = None;
        edit_profile_patch(&paths, None, |layout| {
            let Some(mut routes_config) = layout
                .patch_value(ROUTES_ROW, "config")
                .and_then(Value::as_object)
                .cloned()
            else {
                return Ok(());
            };
            let Some(providers) = routes_config
                .get_mut("providers")
                .and_then(Value::as_object_mut)
            else {
                return Ok(());
            };
            let Some(removed) = providers.remove(id) else {
                return Ok(());
            };
            referenced = removed
                .get("apiKeyEnv")
                .and_then(Value::as_str)
                .map(str::to_owned);
            let value = if providers.is_empty() && routes_config.len() == 1 {
                None
            } else {
                Some(Value::Object(routes_config))
            };
            layout.set_patch_key(ROUTES_ROW, "config", value)
        })?;
        if let Some(name) = referenced.filter(|name| name.starts_with(OWNED_REF_PREFIX)) {
            set_credential_refs(&paths, &[(&name, None)])?;
        }
    }
    clear_default_if(dirs, id)
}

/// The `acp` row's `{provider, model}`; `None` keeps dsh's own default.
fn write_default(dirs: &AgentDirs, selection: Option<(&str, &str)>) -> Result<(), AppError> {
    let paths = DshPaths::of(dirs);
    HomeLayer::read(&paths).ensure_not_overridden(&paths, DEFAULT_ROW, "config")?;
    edit_profile_patch(&paths, None, |layout| {
        layout.set_patch_key(
            DEFAULT_ROW,
            "config",
            selection.map(|(provider, model)| json!({ "provider": provider, "model": model })),
        )
    })
    .map(|_| ())
}

pub fn set_default(dirs: &AgentDirs, id: &str, model_id: &str) -> Result<(), AppError> {
    if model_id.trim().is_empty() {
        if id != DEEPSEEK_ROUTE {
            return Err(AppError::InvalidRequest(format!(
                "provider {id} needs a modelId"
            )));
        }
        return write_default(dirs, None);
    }
    write_default(dirs, Some((id, model_id)))
}

fn clear_default_if(dirs: &AgentDirs, id: &str) -> Result<(), AppError> {
    let (_, entries) = read_profile(dirs)?;
    let selected = row_value(&entries, DEFAULT_ROW, "config")
        .and_then(|config| config.get("provider").cloned())
        .is_some_and(|provider| provider == id);
    if selected {
        write_default(dirs, None)?;
    }
    Ok(())
}

pub fn default_selection(dirs: &AgentDirs) -> Result<Value, AppError> {
    let (_, entries) = read_profile(dirs)?;
    Ok(match row_value(&entries, DEFAULT_ROW, "config") {
        Some(config) => match config.get("provider").and_then(Value::as_str) {
            Some(provider) => json!({ "providerId": provider, "modelId": config.get("model") }),
            None => Value::Null,
        },
        None => Value::Null,
    })
}

pub fn first_model_id(node: &Value) -> Option<String> {
    node.get("models")
        .and_then(Value::as_array)
        .and_then(|models| {
            models
                .iter()
                .find_map(|model| model.get("id").and_then(Value::as_str))
        })
        .map(str::to_owned)
}

/// `(baseURL, apiKey)` for the model-listing call. The DeepSeek route lists
/// its models from dsh's own catalog, not from the endpoint.
pub fn endpoint_credentials(node: &Value) -> (Option<String>, Option<String>) {
    (
        node.get("baseURL")
            .and_then(Value::as_str)
            .map(str::to_owned),
        node.get("apiKey")
            .and_then(Value::as_str)
            .map(str::to_owned),
    )
}

/// Whether a node speaks the Anthropic Messages protocol, whose catalog
/// lives at `/v1/models`.
pub fn anthropic_style(node: &Value) -> bool {
    node.get("api").and_then(Value::as_str) == Some("anthropic-messages")
}

pub(super) struct Projection;

impl super::AdditiveProjection for Projection {
    fn provider_nodes(&self, dirs: &AgentDirs) -> Result<Map<String, Value>, AppError> {
        provider_nodes(dirs)
    }

    fn upsert_provider(&self, dirs: &AgentDirs, id: &str, node: &Value) -> Result<(), AppError> {
        upsert_provider(dirs, id, node)
    }

    fn remove_provider(&self, dirs: &AgentDirs, id: &str) -> Result<(), AppError> {
        remove_provider(dirs, id)
    }

    fn set_default(&self, dirs: &AgentDirs, id: &str, model_id: &str) -> Result<(), AppError> {
        set_default(dirs, id, model_id)
    }

    fn default_selection(&self, dirs: &AgentDirs) -> Result<Value, AppError> {
        default_selection(dirs)
    }

    fn first_model_id(&self, node: &Value) -> Option<String> {
        first_model_id(node)
    }

    fn model_optional(&self, id: &str) -> bool {
        id == DEEPSEEK_ROUTE
    }

    fn endpoint_credentials(&self, node: &Value) -> (Option<String>, Option<String>) {
        endpoint_credentials(node)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn dirs() -> (PathBuf, AgentDirs) {
        let root =
            std::env::temp_dir().join(format!("todex-dsh-accounts-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&root).unwrap();
        (
            root.clone(),
            AgentDirs::for_tests_with_dsh_home(root.join("dsh")),
        )
    }

    fn gateway() -> Value {
        json!({
            "displayName": "My Gateway",
            "api": "openai-completions",
            "baseURL": "https://gw.example/v1",
            "apiKey": "sk-gw",
            "models": [{"id": "m1", "contextWindow": 100000}]
        })
    }

    #[test]
    fn routes_live_in_the_profile_patch_with_keys_in_credentials() {
        let (root, dirs) = dirs();
        upsert_provider(&dirs, "my-gw", &gateway()).unwrap();
        let paths = DshPaths::of(&dirs);
        let patch = std::fs::read_to_string(paths.profile_patch()).unwrap();
        assert!(patch.contains("- id: llm-pi-ai\n  config:\n    providers:\n      my-gw:\n"));
        assert!(patch.contains("apiKeyEnv: TODEX_DSH_MY_GW_API_KEY"));
        assert!(!patch.contains("sk-gw"));
        assert_eq!(
            credential_ref(&paths, "TODEX_DSH_MY_GW_API_KEY")
                .unwrap()
                .as_deref(),
            Some("sk-gw")
        );
        // The node reads back as it was written, key included.
        let nodes = provider_nodes(&dirs).unwrap();
        assert_eq!(nodes["my-gw"], gateway());

        set_default(&dirs, "my-gw", "m1").unwrap();
        assert_eq!(
            default_selection(&dirs).unwrap(),
            json!({"providerId": "my-gw", "modelId": "m1"})
        );
        remove_provider(&dirs, "my-gw").unwrap();
        assert!(provider_nodes(&dirs).unwrap().is_empty());
        assert_eq!(default_selection(&dirs).unwrap(), Value::Null);
        assert_eq!(
            credential_ref(&paths, "TODEX_DSH_MY_GW_API_KEY").unwrap(),
            None
        );
        assert_eq!(
            std::fs::read_to_string(paths.profile_patch()).unwrap(),
            "[]\n"
        );
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn the_deepseek_route_uses_its_native_key_reference() {
        let (root, dirs) = dirs();
        let paths = DshPaths::of(&dirs);
        upsert_provider(&dirs, DEEPSEEK_ROUTE, &json!({"apiKey": "sk-ds"})).unwrap();
        assert_eq!(
            credential_ref(&paths, DEEPSEEK_KEY_REF).unwrap().as_deref(),
            Some("sk-ds")
        );
        // Only a key: no llm-deepseek override is written.
        assert!(
            !paths.profile_patch().exists()
                || !std::fs::read_to_string(paths.profile_patch())
                    .unwrap()
                    .contains("llm-deepseek")
        );
        upsert_provider(
            &dirs,
            DEEPSEEK_ROUTE,
            &json!({"apiKey": "sk-ds", "baseURL": "https://proxy.example/anthropic"}),
        )
        .unwrap();
        assert_eq!(
            provider_nodes(&dirs).unwrap()[DEEPSEEK_ROUTE],
            json!({"baseURL": "https://proxy.example/anthropic", "apiKey": "sk-ds"})
        );
        // Activating without a model restores dsh's own default.
        set_default(&dirs, DEEPSEEK_ROUTE, "deepseek-v4-pro").unwrap();
        set_default(&dirs, DEEPSEEK_ROUTE, "").unwrap();
        assert_eq!(default_selection(&dirs).unwrap(), Value::Null);
        remove_provider(&dirs, DEEPSEEK_ROUTE).unwrap();
        assert!(provider_nodes(&dirs).unwrap().is_empty());
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn invalid_routes_and_home_overrides_are_refused() {
        let (root, dirs) = dirs();
        let mut bad = gateway();
        bad["api"] = json!("grpc");
        assert!(upsert_provider(&dirs, "gw", &bad).is_err());
        assert!(upsert_provider(&dirs, "Bad_Id", &gateway()).is_err());
        assert!(upsert_provider(&dirs, "deepseek-account", &gateway()).is_err());
        let mut no_models = gateway();
        no_models["models"] = json!([]);
        assert!(upsert_provider(&dirs, "gw", &no_models).is_err());
        let paths = DshPaths::of(&dirs);
        std::fs::create_dir_all(paths.home()).unwrap();
        std::fs::write(paths.home_patch(), "- id: llm-pi-ai\n  config: {}\n").unwrap();
        assert!(matches!(
            upsert_provider(&dirs, "gw", &gateway()),
            Err(AppError::Conflict(_))
        ));
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn routes_without_a_key_keep_the_users_reference() {
        let (root, dirs) = dirs();
        let mut node = gateway();
        node.as_object_mut().unwrap().remove("apiKey");
        node["apiKeyEnv"] = json!("MY_OWN_KEY");
        upsert_provider(&dirs, "gw", &node).unwrap();
        assert_eq!(
            provider_nodes(&dirs).unwrap()["gw"]["apiKeyEnv"],
            "MY_OWN_KEY"
        );
        std::fs::remove_dir_all(root).unwrap();
    }
}
