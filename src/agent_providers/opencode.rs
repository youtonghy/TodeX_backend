//! OpenCode live config adapter.
//!
//! Additive mode: every managed provider lives as a `provider.<id>` node inside
//! `~/.config/opencode/opencode.json` (JSONC); activation only moves the
//! top-level `model` pointer to `"<providerId>/<modelId>"`.

use std::path::PathBuf;

use serde_json::{json, Map, Value};

use super::files;
use super::AgentDirs;
use crate::error::AppError;

pub fn config_path(dirs: &AgentDirs) -> PathBuf {
    dirs.opencode_dir.join("opencode.json")
}

fn read_config(dirs: &AgentDirs) -> Result<Value, AppError> {
    let path = config_path(dirs);
    match files::read_json5(&path, "OpenCode config")? {
        Some(value) => {
            files::ensure_object(&value, &path, "OpenCode config")?;
            Ok(value)
        }
        None => Ok(json!({ "$schema": "https://opencode.ai/config.json" })),
    }
}

/// All `provider.*` nodes, managed or not.
pub fn provider_nodes(dirs: &AgentDirs) -> Result<Map<String, Value>, AppError> {
    Ok(read_config(dirs)?
        .get("provider")
        .and_then(Value::as_object)
        .cloned()
        .unwrap_or_default())
}

pub fn upsert_provider(dirs: &AgentDirs, id: &str, node: &Value) -> Result<(), AppError> {
    if !node.is_object() {
        return Err(AppError::InvalidRequest(
            "OpenCode provider settingsConfig must be an object".to_owned(),
        ));
    }
    let path = config_path(dirs);
    let modify_path = path.clone();
    let id = id.to_owned();
    let node = node.clone();
    files::modify_json5_file(&path, "OpenCode config", move |config| {
        files::ensure_object(config, &modify_path, "OpenCode config")?;
        if !config.get("provider").is_some_and(Value::is_object) {
            config
                .as_object_mut()
                .expect("root validated as object")
                .insert("provider".to_owned(), json!({}));
        }
        config["provider"]
            .as_object_mut()
            .expect("provider normalized to object")
            .insert(id, node);
        Ok(())
    })
}

pub fn remove_provider(dirs: &AgentDirs, id: &str) -> Result<(), AppError> {
    let path = config_path(dirs);
    if !path.exists() {
        return Ok(());
    }
    let id = id.to_owned();
    let modify_path = path.clone();
    files::modify_json5_file(&path, "OpenCode config", move |config| {
        files::ensure_object(config, &modify_path, "OpenCode config")?;
        if let Some(providers) = config.get_mut("provider").and_then(Value::as_object_mut) {
            providers.remove(&id);
        }
        // A dangling `"model": "<id>/…"` would point at a deleted provider.
        let model_is_orphaned = config
            .get("model")
            .and_then(Value::as_str)
            .is_some_and(|model| model.split('/').next() == Some(id.as_str()));
        if model_is_orphaned {
            config
                .as_object_mut()
                .expect("root is object")
                .remove("model");
        }
        Ok(())
    })
}

/// Set the top-level `model` pointer, e.g. `"<providerId>/<modelId>"`.
pub fn set_default(dirs: &AgentDirs, id: &str, model_id: &str) -> Result<(), AppError> {
    let path = config_path(dirs);
    let modify_path = path.clone();
    let selection = format!("{id}/{model_id}");
    files::modify_json5_file(&path, "OpenCode config", move |config| {
        files::ensure_object(config, &modify_path, "OpenCode config")?;
        config
            .as_object_mut()
            .expect("root is object")
            .insert("model".to_owned(), json!(selection));
        Ok(())
    })
}

/// The live `model` pointer split into `(provider_id, model_id)`.
pub fn default_selection(dirs: &AgentDirs) -> Result<Option<(String, String)>, AppError> {
    let config = read_config(dirs)?;
    Ok(config
        .get("model")
        .and_then(Value::as_str)
        .and_then(|model| {
            let (provider, model) = model.split_once('/')?;
            Some((provider.to_owned(), model.to_owned()))
        }))
}

/// First declared model id inside a provider node, if any.
pub fn first_model_id(node: &Value) -> Option<String> {
    node.get("models")
        .and_then(Value::as_object)
        .and_then(|models| models.keys().next())
        .cloned()
}

/// Extract `(base_url, api_key)` for the model-listing proxy call.
pub fn endpoint_credentials(node: &Value) -> (Option<String>, Option<String>) {
    let options = node.get("options");
    (
        options
            .and_then(|options| options.get("baseURL").or_else(|| options.get("baseUrl")))
            .and_then(Value::as_str)
            .map(str::to_owned),
        options
            .and_then(|options| options.get("apiKey"))
            .and_then(Value::as_str)
            .map(str::to_owned),
    )
}
