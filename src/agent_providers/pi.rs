//! Pi live config adapter.
//!
//! Additive mode, mirroring cc-switch's Pi support: managed providers are
//! `providers.<key>` nodes in `<agent dir>/models.json` (JSONC); activation
//! writes `defaultProvider`/`defaultModel` into `settings.json`. Pi's own
//! `auth.json` login credentials are never touched.

use std::path::PathBuf;

use serde_json::{json, Map, Value};

use super::files;
use super::AgentDirs;
use crate::error::AppError;

pub fn models_path(dirs: &AgentDirs) -> PathBuf {
    dirs.pi_dir.join("models.json")
}

pub fn settings_path(dirs: &AgentDirs) -> PathBuf {
    dirs.pi_dir.join("settings.json")
}

/// All `providers.*` nodes, managed or not.
pub fn provider_nodes(dirs: &AgentDirs) -> Result<Map<String, Value>, AppError> {
    let path = models_path(dirs);
    let Some(document) = files::read_json5(&path, "Pi models")? else {
        return Ok(Map::new());
    };
    files::ensure_object(&document, &path, "Pi models")?;
    match document.get("providers") {
        None => Ok(Map::new()),
        Some(Value::Object(providers)) => Ok(providers.clone()),
        Some(_) => Err(AppError::InvalidRequest(format!(
            "Pi models 'providers' must be an object: {}",
            path.display()
        ))),
    }
}

pub fn upsert_provider(dirs: &AgentDirs, id: &str, node: &Value) -> Result<(), AppError> {
    if !node.is_object() {
        return Err(AppError::InvalidRequest(
            "Pi provider settingsConfig must be an object".to_owned(),
        ));
    }
    let path = models_path(dirs);
    let modify_path = path.clone();
    let id = id.to_owned();
    let node = node.clone();
    files::modify_json5_file(&path, "Pi models", move |document| {
        files::ensure_object(document, &modify_path, "Pi models")?;
        if !document.get("providers").is_some_and(Value::is_object) {
            document
                .as_object_mut()
                .expect("root validated as object")
                .insert("providers".to_owned(), json!({}));
        }
        document["providers"]
            .as_object_mut()
            .expect("providers normalized to object")
            .insert(id, node);
        Ok(())
    })
}

pub fn remove_provider(dirs: &AgentDirs, id: &str) -> Result<(), AppError> {
    let path = models_path(dirs);
    if !path.exists() {
        return Ok(());
    }
    let id = id.to_owned();
    let modify_path = path.clone();
    let closure_id = id.clone();
    files::modify_json5_file(&path, "Pi models", move |document| {
        files::ensure_object(document, &modify_path, "Pi models")?;
        if let Some(providers) = document.get_mut("providers").and_then(Value::as_object_mut) {
            providers.remove(&closure_id);
        }
        Ok(())
    })?;
    clear_default_if(dirs, &id)
}

pub fn set_default(dirs: &AgentDirs, id: &str, model_id: &str) -> Result<(), AppError> {
    let path = settings_path(dirs);
    let modify_path = path.clone();
    let id = id.to_owned();
    let model_id = model_id.to_owned();
    files::modify_json5_file(&path, "Pi settings", move |settings| {
        files::ensure_object(settings, &modify_path, "Pi settings")?;
        let root = settings.as_object_mut().expect("root is object");
        root.insert("defaultProvider".to_owned(), json!(id));
        root.insert("defaultModel".to_owned(), json!(model_id));
        Ok(())
    })
}

/// `(defaultProvider, defaultModel)` from settings.json.
pub fn default_selection(dirs: &AgentDirs) -> Result<(Option<String>, Option<String>), AppError> {
    let path = settings_path(dirs);
    let Some(settings) = files::read_json5(&path, "Pi settings")? else {
        return Ok((None, None));
    };
    files::ensure_object(&settings, &path, "Pi settings")?;
    Ok((
        settings
            .get("defaultProvider")
            .and_then(Value::as_str)
            .map(str::to_owned),
        settings
            .get("defaultModel")
            .and_then(Value::as_str)
            .map(str::to_owned),
    ))
}

fn clear_default_if(dirs: &AgentDirs, id: &str) -> Result<(), AppError> {
    let path = settings_path(dirs);
    if !path.exists() {
        return Ok(());
    }
    let id = id.to_owned();
    let modify_path = path.clone();
    files::modify_json5_file(&path, "Pi settings", move |settings| {
        files::ensure_object(settings, &modify_path, "Pi settings")?;
        let selected = settings
            .get("defaultProvider")
            .and_then(Value::as_str)
            .is_some_and(|provider| provider == id);
        if selected {
            let root = settings.as_object_mut().expect("root is object");
            root.remove("defaultProvider");
            root.remove("defaultModel");
        }
        Ok(())
    })
}

/// First declared model id inside a provider node (`models[].id`), if any.
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

/// Extract `(base_url, api_key)` for the model-listing proxy call.
pub fn endpoint_credentials(node: &Value) -> (Option<String>, Option<String>) {
    (
        node.get("baseUrl")
            .and_then(Value::as_str)
            .map(str::to_owned),
        node.get("apiKey")
            .and_then(Value::as_str)
            .map(str::to_owned),
    )
}
