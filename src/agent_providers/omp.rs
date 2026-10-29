//! omp live config adapter.
//!
//! Additive mode, as for Pi (omp's fork parent): managed providers are
//! `providers.<key>` nodes in `<omp dir>/models.yml`. Activation sets the
//! `default` model role through omp's own `omp config set modelRoles`, so omp
//! merges it into `config.yml` under its own lock; TodeX only reads that
//! file. omp's login store (`agent.db`) is never touched.
//!
//! TodeX rewrites `models.yml` as plain YAML, which drops comments (omp's
//! own writer does too): a file that still has comments is backed up to
//! `models.yml.todex-backup-<UTC timestamp>` first, keeping the newest five.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use serde_json::{json, Map, Value};

use super::files;
use super::AgentDirs;
use crate::error::AppError;

const MODELS_LABEL: &str = "omp models";
const CONFIG_LABEL: &str = "omp config";
const BACKUP_PREFIX: &str = "models.yml.todex-backup-";
const BACKUPS_KEPT: usize = 5;
const ROLE: &str = "default";
/// `omp config get/set` start the whole CLI; a cold start takes about a
/// second.
const CLI_TIMEOUT: Duration = Duration::from_secs(30);

pub fn models_path(dirs: &AgentDirs) -> PathBuf {
    dirs.omp_dir().join("models.yml")
}

fn config_path(dirs: &AgentDirs) -> PathBuf {
    dirs.omp_dir().join("config.yml")
}

/// The YAML document at `path` as JSON, `None` when the file is missing.
fn read_yaml(path: &Path, label: &str) -> Result<Option<(String, Value)>, AppError> {
    let Some(text) = files::read_text(path, label)? else {
        return Ok(None);
    };
    let value = if text.trim().is_empty() {
        Value::Object(Map::new())
    } else {
        serde_yaml_ng::from_str::<Value>(&text).map_err(|error| {
            AppError::InvalidRequest(format!(
                "{label} file is not valid YAML ({}): {error}",
                path.display()
            ))
        })?
    };
    files::ensure_object(&value, path, label)?;
    Ok(Some((text, value)))
}

/// All `providers.*` nodes, managed or not.
pub fn provider_nodes(dirs: &AgentDirs) -> Result<Map<String, Value>, AppError> {
    let path = models_path(dirs);
    let Some((_, document)) = read_yaml(&path, MODELS_LABEL)? else {
        return Ok(Map::new());
    };
    match document.get("providers") {
        None | Some(Value::Null) => Ok(Map::new()),
        Some(Value::Object(providers)) => Ok(providers.clone()),
        Some(_) => Err(AppError::InvalidRequest(format!(
            "omp models 'providers' must be a mapping: {}",
            path.display()
        ))),
    }
}

/// Read-modify-write of `models.yml`, failing with Conflict when the file
/// changed between the read and the write.
fn modify_models(
    dirs: &AgentDirs,
    modify: impl FnOnce(&mut Map<String, Value>) -> Result<(), AppError>,
) -> Result<(), AppError> {
    let path = models_path(dirs);
    let (text, mut document) = read_yaml(&path, MODELS_LABEL)?
        .map(|(text, document)| (Some(text), document))
        .unwrap_or((None, Value::Object(Map::new())));
    let root = document.as_object_mut().expect("root validated as object");
    if !root.get("providers").is_some_and(Value::is_object) {
        root.insert("providers".to_owned(), json!({}));
    }
    modify(
        root.get_mut("providers")
            .and_then(Value::as_object_mut)
            .expect("providers normalized to object"),
    )?;
    let expected = text.as_deref().map(|text| files::revision(text.as_bytes()));
    match &expected {
        Some(revision) => files::ensure_revision(&path, MODELS_LABEL, revision)?,
        None if path.exists() => {
            return Err(AppError::Conflict(format!(
                "omp models file appeared while it was being written: {}",
                path.display()
            )))
        }
        None => {}
    }
    if let Some(text) = &text {
        if has_comments(text) {
            backup(&path, text.as_bytes())?;
        }
    }
    let yaml =
        serde_yaml_ng::to_string(&document).map_err(|error| AppError::Anyhow(error.into()))?;
    files::atomic_write_private(&path, yaml.as_bytes())
}

/// Whether a YAML file holds a comment line, which a rewrite would drop.
fn has_comments(text: &str) -> bool {
    text.lines().any(|line| line.trim_start().starts_with('#'))
}

/// Copy `bytes` to a new owner-only backup beside `path`, then keep only the
/// newest [`BACKUPS_KEPT`] backups.
fn backup(path: &Path, bytes: &[u8]) -> Result<PathBuf, AppError> {
    let dir = path.parent().ok_or_else(|| {
        AppError::InvalidRequest(format!("config path has no parent: {}", path.display()))
    })?;
    let stamp = chrono::Utc::now().format("%Y%m%dT%H%M%S%.3fZ").to_string();
    let mut target = dir.join(format!("{BACKUP_PREFIX}{stamp}"));
    let mut suffix = 1;
    while target.exists() {
        target = dir.join(format!("{BACKUP_PREFIX}{stamp}-{suffix}"));
        suffix += 1;
    }
    files::atomic_write_private(&target, bytes)?;
    let mut names: Vec<String> = std::fs::read_dir(dir)
        .map_err(AppError::Io)?
        .filter_map(Result::ok)
        .map(|entry| entry.file_name().to_string_lossy().into_owned())
        .filter(|name| name.starts_with(BACKUP_PREFIX))
        .collect();
    names.sort();
    let excess = names.len().saturating_sub(BACKUPS_KEPT);
    for name in &names[..excess] {
        files::remove_file_if_exists(&dir.join(name))?;
    }
    Ok(target)
}

pub fn upsert_provider(dirs: &AgentDirs, id: &str, node: &Value) -> Result<(), AppError> {
    if !node.is_object() {
        return Err(AppError::InvalidRequest(
            "omp provider settingsConfig must be an object".to_owned(),
        ));
    }
    let id = id.to_owned();
    let node = node.clone();
    modify_models(dirs, move |providers| {
        providers.insert(id, node);
        Ok(())
    })
}

pub fn remove_provider(dirs: &AgentDirs, id: &str) -> Result<(), AppError> {
    if !models_path(dirs).exists() {
        return Ok(());
    }
    let key = id.to_owned();
    modify_models(dirs, move |providers| {
        providers.remove(&key);
        Ok(())
    })?;
    if default_selection(dirs)?.0.as_deref() == Some(id) {
        let roles = model_roles(dirs)?;
        let mut roles = roles.as_object().cloned().unwrap_or_default();
        roles.remove(ROLE);
        set_model_roles(dirs, &Value::Object(roles))?;
    }
    Ok(())
}

pub fn set_default(dirs: &AgentDirs, id: &str, model_id: &str) -> Result<(), AppError> {
    let mut roles = model_roles(dirs)?.as_object().cloned().unwrap_or_default();
    roles.insert(ROLE.to_owned(), json!(format!("{id}/{model_id}")));
    set_model_roles(dirs, &Value::Object(roles))
}

/// `(provider, model)` of the global `modelRoles.default` (`provider/model`,
/// optionally with a `:thinking` suffix), read from `config.yml`.
pub fn default_selection(dirs: &AgentDirs) -> Result<(Option<String>, Option<String>), AppError> {
    let Some((_, config)) = read_yaml(&config_path(dirs), CONFIG_LABEL)? else {
        return Ok((None, None));
    };
    let Some(selector) = config
        .pointer("/modelRoles/default")
        .and_then(Value::as_str)
        .and_then(|selector| selector.split(',').next())
        .map(str::trim)
    else {
        return Ok((None, None));
    };
    let selector = selector.split(':').next().unwrap_or(selector);
    Ok(match selector.split_once('/') {
        Some((provider, model)) if !provider.is_empty() => {
            (Some(provider.to_owned()), Some(model.to_owned()))
        }
        _ => (None, None),
    })
}

/// The global `modelRoles`, as `omp config get` reports it.
fn model_roles(dirs: &AgentDirs) -> Result<Value, AppError> {
    let output = run_cli(dirs, &["config", "get", "modelRoles", "--json"])?;
    let reply: Value = serde_json::from_str(&output).map_err(|error| {
        AppError::ProviderUnavailable(format!("unexpected `omp config get` output: {error}"))
    })?;
    Ok(reply.get("value").cloned().unwrap_or(json!({})))
}

fn set_model_roles(dirs: &AgentDirs, roles: &Value) -> Result<(), AppError> {
    let roles = serde_json::to_string(roles)?;
    run_cli(dirs, &["config", "set", "modelRoles", &roles, "--json"]).map(|_| ())
}

/// Runs the configured omp binary without a shell, bounded by
/// [`CLI_TIMEOUT`]. Pi's `PI_CODING_AGENT_DIR` would point omp at Pi's
/// directory, so it is not passed on.
fn run_cli(dirs: &AgentDirs, args: &[&str]) -> Result<String, AppError> {
    let mut child = Command::new(dirs.omp_bin())
        .args(args)
        .env_remove("PI_CODING_AGENT_DIR")
        .env("NO_COLOR", "1")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|error| {
            AppError::ProviderUnavailable(format!(
                "could not run omp ('{}'): {error}",
                dirs.omp_bin()
            ))
        })?;
    let started = Instant::now();
    loop {
        if child.try_wait().map_err(AppError::Io)?.is_some() {
            break;
        }
        if started.elapsed() > CLI_TIMEOUT {
            let _ = child.kill();
            let _ = child.wait();
            return Err(AppError::ProviderUnavailable(format!(
                "`omp {}` timed out",
                args[..2].join(" ")
            )));
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    let output = child.wait_with_output().map_err(AppError::Io)?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        let line = stderr
            .lines()
            .map(str::trim)
            .find(|line| !line.is_empty())
            .unwrap_or("no error output");
        return Err(AppError::ProviderUnavailable(format!(
            "`omp {}` failed: {}",
            args[..2].join(" "),
            line.chars().take(300).collect::<String>()
        )));
    }
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

/// First declared model id inside a provider node (`models[].id`), if any.
pub fn first_model_id(node: &Value) -> Option<String> {
    super::pi::first_model_id(node)
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
        let (provider, model) = default_selection(dirs)?;
        Ok(match provider {
            Some(provider) => json!({ "providerId": provider, "modelId": model }),
            None => Value::Null,
        })
    }

    fn first_model_id(&self, node: &Value) -> Option<String> {
        first_model_id(node)
    }

    fn endpoint_credentials(&self, node: &Value) -> (Option<String>, Option<String>) {
        super::pi::endpoint_credentials(node)
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    /// A stand-in for `omp config get/set modelRoles`: it keeps `config.yml`
    /// as JSON (valid YAML) and logs its arguments.
    const FAKE_OMP: &str = r#"#!/usr/bin/env python3
import json, os, sys
config = os.path.join(os.environ["FAKE_OMP_DIR"], "config.yml")
open(os.path.join(os.environ["FAKE_OMP_DIR"], "cli.log"), "a").write(json.dumps(sys.argv[1:]) + "\n")
data = json.load(open(config)) if os.path.exists(config) else {}
if sys.argv[1:4] == ["config", "get", "modelRoles"]:
    print(json.dumps({"key": "modelRoles", "value": data.get("modelRoles", {})}))
elif sys.argv[1:4] == ["config", "set", "modelRoles"]:
    if os.environ.get("FAKE_OMP_FAIL"):
        sys.stderr.write("Invalid record JSON\n"); sys.exit(1)
    data["modelRoles"] = json.loads(sys.argv[4])
    json.dump(data, open(config, "w"))
    print(json.dumps({"key": "modelRoles", "value": data["modelRoles"]}))
"#;

    fn dirs(root: &Path) -> AgentDirs {
        use std::os::unix::fs::PermissionsExt;
        let mut dirs = AgentDirs::for_tests_with_pi_dir(root.join("pi"));
        dirs.omp_dir = root.join("omp");
        std::fs::create_dir_all(&dirs.omp_dir).unwrap();
        let bin = root.join("omp-fake");
        std::fs::write(&bin, FAKE_OMP).unwrap();
        std::fs::set_permissions(&bin, std::fs::Permissions::from_mode(0o755)).unwrap();
        dirs.omp_bin = bin.to_string_lossy().into_owned();
        dirs
    }

    fn root() -> PathBuf {
        std::env::temp_dir().join(format!("todex-omp-providers-{}", uuid::Uuid::new_v4()))
    }

    #[test]
    fn providers_land_in_models_yml_and_commented_files_are_backed_up_first() {
        let root = root();
        let dirs = dirs(&root);
        std::fs::write(
            models_path(&dirs),
            "# my providers\nproviders:\n  mine:\n    baseUrl: https://mine.test/v1\nmodelOverrides:\n  some-model:\n    name: Renamed\n",
        )
        .unwrap();
        let node = json!({"baseUrl": "https://acme.test/v1", "apiKey": "ACME_KEY", "api": "openai-completions",
                          "models": [{"id": "m1", "name": "M1"}]});
        upsert_provider(&dirs, "acme", &node).unwrap();
        let nodes = provider_nodes(&dirs).unwrap();
        assert_eq!(nodes["acme"], node);
        assert_eq!(nodes["mine"]["baseUrl"], "https://mine.test/v1");
        let written: Value =
            serde_yaml_ng::from_str(&std::fs::read_to_string(models_path(&dirs)).unwrap()).unwrap();
        assert_eq!(written["modelOverrides"]["some-model"]["name"], "Renamed");
        let backups = || {
            std::fs::read_dir(&dirs.omp_dir)
                .unwrap()
                .filter_map(Result::ok)
                .filter(|entry| {
                    entry
                        .file_name()
                        .to_string_lossy()
                        .starts_with(BACKUP_PREFIX)
                })
                .count()
        };
        assert_eq!(backups(), 1);
        // The rewritten file has no comments left: no further backups.
        upsert_provider(&dirs, "other", &json!({"models": []})).unwrap();
        assert_eq!(backups(), 1);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn activation_sets_only_the_default_role_through_omp() {
        let root = root();
        let dirs = dirs(&root);
        std::env::set_var("FAKE_OMP_DIR", &dirs.omp_dir);
        std::fs::write(
            config_path(&dirs),
            r#"{"modelRoles": {"smol": "openai/gpt-mini"}, "compaction": {"enabled": false}}"#,
        )
        .unwrap();
        assert_eq!(default_selection(&dirs).unwrap(), (None, None));
        set_default(&dirs, "acme", "m1").unwrap();
        assert_eq!(
            default_selection(&dirs).unwrap(),
            (Some("acme".to_owned()), Some("m1".to_owned()))
        );
        let config: Value =
            serde_json::from_str(&std::fs::read_to_string(config_path(&dirs)).unwrap()).unwrap();
        assert_eq!(config["modelRoles"]["smol"], "openai/gpt-mini");
        assert_eq!(config["compaction"]["enabled"], false);

        upsert_provider(&dirs, "acme", &json!({"models": [{"id": "m1"}]})).unwrap();
        remove_provider(&dirs, "acme").unwrap();
        let config: Value =
            serde_json::from_str(&std::fs::read_to_string(config_path(&dirs)).unwrap()).unwrap();
        assert_eq!(config["modelRoles"], json!({"smol": "openai/gpt-mini"}));
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn selectors_with_thinking_suffixes_and_fallbacks_resolve_to_the_provider() {
        let root = root();
        let dirs = dirs(&root);
        std::fs::write(
            config_path(&dirs),
            "modelRoles:\n  default: acme/m1:high, other/m2\n",
        )
        .unwrap();
        assert_eq!(
            default_selection(&dirs).unwrap(),
            (Some("acme".to_owned()), Some("m1".to_owned()))
        );
        std::fs::remove_dir_all(root).unwrap();
    }
}
