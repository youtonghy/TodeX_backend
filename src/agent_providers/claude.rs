//! Claude Code live config adapter.
//!
//! Exclusive mode, merged into the live file: a provider profile owns only
//! the identity keys of `settings.json` (see [`is_managed_key`]); every other
//! key (hooks, permissions, status line, plugins, other `env` entries…) is
//! this machine's own configuration, and activating, saving or deleting a
//! profile never touches it. A projection reads the live file and its
//! revision, removes the managed keys, writes the profile's managed keys,
//! re-checks the revision (a concurrent edit fails with Conflict and leaves
//! the file as it was) and replaces the file atomically. Managed values that
//! a write would drop and that no profile holds are first copied into a
//! timestamped backup beside the file.

use std::path::{Path, PathBuf};

use serde_json::{Map, Value};

use super::files;
use crate::error::AppError;

use super::AgentDirs;

const LABEL: &str = "Claude settings";
/// Backups are `settings.json.todex-backup-<UTC timestamp>` beside the file.
const BACKUP_PREFIX: &str = "settings.json.todex-backup-";
const BACKUPS_KEPT: usize = 5;

/// Top-level keys a provider profile owns: the model choice and the helper
/// script that prints the API key.
const MANAGED_TOP_LEVEL_KEYS: [&str; 2] = ["model", "apiKeyHelper"];
/// `env` keys outside `ANTHROPIC_*` a profile owns: the switches that route
/// Claude Code to Bedrock, Vertex AI or Microsoft Foundry.
const MANAGED_ENV_SWITCHES: [&str; 3] = [
    "CLAUDE_CODE_USE_BEDROCK",
    "CLAUDE_CODE_USE_VERTEX",
    "CLAUDE_CODE_USE_FOUNDRY",
];

/// Whether an `env` key belongs to the provider profile: every
/// `ANTHROPIC_*` variable (endpoint, credentials, model ids, headers) plus
/// the cloud-provider switches.
fn is_managed_env_key(key: &str) -> bool {
    key.starts_with("ANTHROPIC_") || MANAGED_ENV_SWITCHES.contains(&key)
}

/// The single definition of which `settings.json` entries a Claude provider
/// profile manages. `path` is a top-level key, or `["env", <name>]` for an
/// environment variable. Everything else is the machine's live config.
fn is_managed_key(path: &[&str]) -> bool {
    match path {
        [key] => MANAGED_TOP_LEVEL_KEYS.contains(key),
        ["env", key] => is_managed_env_key(key),
        _ => false,
    }
}

pub fn settings_path(dirs: &AgentDirs) -> PathBuf {
    dirs.claude_dir.join("settings.json")
}

/// The live settings as one `settingsConfig` blob; `None` when no file exists.
pub fn read_live(dirs: &AgentDirs) -> Result<Option<Value>, AppError> {
    let path = settings_path(dirs);
    let Some(value) = files::read_json5(&path, LABEL)? else {
        return Ok(None);
    };
    files::ensure_object(&value, &path, LABEL)?;
    Ok(Some(value))
}

/// Replace the whole live file with `settings`, no merge and no revision
/// check (tests use it to stand in for an external edit).
#[cfg(test)]
pub fn write_live(dirs: &AgentDirs, settings: &Value) -> Result<(), AppError> {
    let path = settings_path(dirs);
    files::ensure_object(settings, &path, "Claude provider settings")?;
    files::write_json_pretty(&path, settings)
}

/// The managed entries of `settings` alone: `{env: {...}, model,
/// apiKeyHelper}`, with `env` left out when it has no managed variables.
pub fn managed_part(settings: &Value) -> Value {
    let mut out = Map::new();
    let Some(object) = settings.as_object() else {
        return Value::Object(out);
    };
    for (key, value) in object {
        if is_managed_key(&[key.as_str()]) {
            out.insert(key.clone(), value.clone());
        }
    }
    if let Some(env) = object.get("env").and_then(Value::as_object) {
        let managed: Map<String, Value> = env
            .iter()
            .filter(|(key, _)| is_managed_key(&["env", key.as_str()]))
            .map(|(key, value)| (key.clone(), value.clone()))
            .collect();
        if !managed.is_empty() {
            out.insert("env".to_owned(), Value::Object(managed));
        }
    }
    Value::Object(out)
}

/// Remove every managed entry; an `env` emptied by it is removed too.
fn strip_managed(settings: &mut Map<String, Value>) {
    settings.retain(|key, _| !is_managed_key(&[key.as_str()]));
    if let Some(Value::Object(env)) = settings.get_mut("env") {
        let before = env.len();
        env.retain(|key, _| !is_managed_key(&["env", key.as_str()]));
        if env.is_empty() && before > 0 {
            settings.remove("env");
        }
    }
}

/// `settings` with its managed entries replaced by those of `source`.
fn with_managed_from(settings: &Value, source: &Value) -> Result<Value, AppError> {
    let mut out = settings.as_object().cloned().unwrap_or_default();
    strip_managed(&mut out);
    let Value::Object(managed) = managed_part(source) else {
        unreachable!("managed_part returns an object");
    };
    for (key, value) in managed {
        if key != "env" {
            out.insert(key, value);
            continue;
        }
        let Value::Object(vars) = value else {
            continue;
        };
        let env = out
            .entry("env")
            .or_insert_with(|| Value::Object(Map::new()));
        let Some(env) = env.as_object_mut() else {
            return Err(AppError::InvalidRequest(
                "Claude settings `env` must be a JSON object".to_owned(),
            ));
        };
        env.extend(vars);
    }
    Ok(Value::Object(out))
}

/// Every managed `(path, value)` pair of `settings`.
fn managed_pairs(settings: &Value) -> Vec<(Vec<String>, Value)> {
    let mut pairs = Vec::new();
    let Value::Object(managed) = managed_part(settings) else {
        return pairs;
    };
    for (key, value) in managed {
        match (key.as_str(), value) {
            ("env", Value::Object(vars)) => pairs.extend(
                vars.into_iter()
                    .map(|(name, value)| (vec!["env".to_owned(), name], value)),
            ),
            (_, value) => pairs.push((vec![key], value)),
        }
    }
    pairs
}

fn lookup<'a>(settings: &'a Value, path: &[String]) -> Option<&'a Value> {
    path.iter()
        .try_fold(settings, |value, key| value.get(key.as_str()))
}

/// Whether turning `before` into `after` drops a managed value that none of
/// the `known` profiles holds, i.e. a value only the live file has.
fn loses_unknown_values(before: &Value, after: &Value, known: &[&Value]) -> bool {
    managed_pairs(before).iter().any(|(path, value)| {
        lookup(after, path) != Some(value)
            && !known
                .iter()
                .any(|profile| lookup(profile, path) == Some(value))
    })
}

/// The live file's bytes, parsed document and revision. A missing file
/// reads as `{}` with the `missing` revision and no bytes.
fn read_revisioned(path: &Path) -> Result<(Option<Vec<u8>>, Value, String), AppError> {
    let bytes = match files::read_limited(path, LABEL) {
        Ok(bytes) => bytes,
        Err(AppError::Io(error)) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok((
                None,
                Value::Object(Map::new()),
                files::MISSING_REVISION.to_owned(),
            ))
        }
        Err(error) => return Err(error),
    };
    let revision = files::revision(&bytes);
    let text = std::str::from_utf8(&bytes).map_err(|error| {
        AppError::InvalidRequest(format!(
            "{LABEL} file must be UTF-8 ({}): {error}",
            path.display()
        ))
    })?;
    let value = files::parse_json5(text, path, LABEL)?;
    files::ensure_object(&value, path, LABEL)?;
    Ok((Some(bytes), value, revision))
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
    prune_backups(dir)?;
    Ok(target)
}

/// Remove all but the newest [`BACKUPS_KEPT`] backups. Names sort by time:
/// the timestamp is fixed-width and a same-millisecond `-n` suffix sorts
/// after its base name.
fn prune_backups(dir: &Path) -> Result<(), AppError> {
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
    Ok(())
}

/// Read-modify-write of the live file. `next` maps the live document to the
/// new one. Takes `cli_settings::WRITE_LOCK`, so callers holding the
/// per-agent provider lock take it second (see that lock's docs). Returns
/// whether the file was written: an unchanged document is not rewritten,
/// and a missing file is only created when `create` is set.
fn merge_write(
    dirs: &AgentDirs,
    known: &[&Value],
    create: bool,
    next: impl FnOnce(&Value) -> Result<Value, AppError>,
    before_check: impl FnOnce(),
) -> Result<bool, AppError> {
    let path = settings_path(dirs);
    let _order = crate::cli_settings::WRITE_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let (bytes, live, revision) = read_revisioned(&path)?;
    let updated = next(&live)?;
    let unchanged = match bytes {
        Some(_) => updated == live,
        None => !create,
    };
    if unchanged {
        return Ok(false);
    }
    before_check();
    files::ensure_revision(&path, LABEL, &revision)?;
    if let Some(bytes) = bytes.as_deref() {
        if loses_unknown_values(&live, &updated, known) {
            backup(&path, bytes)?;
        }
    }
    files::write_json_pretty(&path, &updated)?;
    Ok(true)
}

/// Project `settings`' managed keys into the live file, keeping every other
/// live key. `known` holds every stored profile's settings.
pub fn project(dirs: &AgentDirs, settings: &Value, known: &[&Value]) -> Result<(), AppError> {
    project_with(dirs, settings, known, || {})
}

/// [`project`] with a hook that runs between the read and the revision
/// check, where tests simulate a concurrent writer.
fn project_with(
    dirs: &AgentDirs,
    settings: &Value,
    known: &[&Value],
    before_check: impl FnOnce(),
) -> Result<(), AppError> {
    files::ensure_object(settings, &settings_path(dirs), "Claude provider settings")?;
    merge_write(
        dirs,
        known,
        true,
        |live| with_managed_from(live, settings),
        before_check,
    )
    .map(|_| ())
}

/// Strip the managed keys from the live file (the current profile was
/// deleted). Returns whether the file changed.
pub fn release(dirs: &AgentDirs, known: &[&Value]) -> Result<bool, AppError> {
    merge_write(
        dirs,
        known,
        false,
        |live| with_managed_from(live, &Value::Object(Map::new())),
        || {},
    )
}

/// With no current profile: when the live file holds managed values that no
/// `known` profile has, back the file up and return its managed part for
/// import.
pub fn adopt_unknown_live(dirs: &AgentDirs, known: &[&Value]) -> Result<Option<Value>, AppError> {
    let path = settings_path(dirs);
    let _order = crate::cli_settings::WRITE_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let (Some(bytes), live, _) = read_revisioned(&path)? else {
        return Ok(None);
    };
    let managed = managed_part(&live);
    let empty = Value::Object(Map::new());
    if !loses_unknown_values(&live, &empty, known) {
        return Ok(None);
    }
    backup(&path, &bytes)?;
    Ok(Some(managed))
}

/// `(base_url, api_key)` from the `env` block; `ANTHROPIC_AUTH_TOKEN` wins
/// over `ANTHROPIC_API_KEY`, as in Claude Code itself.
pub fn endpoint_credentials(settings: &Value) -> (Option<String>, Option<String>) {
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

pub(super) struct Projection;

impl super::ExclusiveProjection for Projection {
    fn read_live(&self, dirs: &AgentDirs) -> Result<Option<Value>, AppError> {
        read_live(dirs)
    }

    fn write_live(
        &self,
        dirs: &AgentDirs,
        settings: &Value,
        _remove_auth: bool,
    ) -> Result<(), AppError> {
        project(dirs, settings, &[])
    }

    fn project(
        &self,
        dirs: &AgentDirs,
        settings: &Value,
        _remove_auth: bool,
        known: &[&Value],
    ) -> Result<(), AppError> {
        project(dirs, settings, known)
    }

    fn release_live(&self, dirs: &AgentDirs, known: &[&Value]) -> Result<bool, AppError> {
        release(dirs, known)
    }

    fn adopt_unknown_live(
        &self,
        dirs: &AgentDirs,
        known: &[&Value],
    ) -> Result<Option<Value>, AppError> {
        adopt_unknown_live(dirs, known)
    }

    fn owned_part(&self, live: &Value) -> Value {
        managed_part(live)
    }

    fn backfilled(&self, live: &Value, stored: &Value) -> Result<Value, AppError> {
        with_managed_from(stored, live)
    }

    fn live_matches(&self, live: &Value, settings: &Value) -> bool {
        managed_part(live) == managed_part(settings)
    }

    fn endpoint_credentials(&self, settings: &Value) -> (Option<String>, Option<String>) {
        endpoint_credentials(settings)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn dirs_in(root: &Path) -> AgentDirs {
        let mut dirs = AgentDirs::for_tests_with_pi_dir(root.join("pi"));
        dirs.claude_dir = root.join("claude");
        dirs
    }

    fn temp_root() -> PathBuf {
        let root = std::env::temp_dir().join(format!(
            "todex-claude-projection-{}",
            uuid::Uuid::new_v4().simple()
        ));
        std::fs::create_dir_all(&root).unwrap();
        root
    }

    fn backups(dirs: &AgentDirs) -> Vec<String> {
        let mut names: Vec<String> = std::fs::read_dir(&dirs.claude_dir)
            .unwrap()
            .filter_map(Result::ok)
            .map(|entry| entry.file_name().to_string_lossy().into_owned())
            .filter(|name| name.starts_with(BACKUP_PREFIX))
            .collect();
        names.sort();
        names
    }

    #[test]
    fn managed_keys_are_the_identity_keys_only() {
        let settings = json!({
            "model": "opus",
            "apiKeyHelper": "~/bin/key.sh",
            "hooks": {},
            "permissions": {"allow": ["Bash"]},
            "env": {
                "ANTHROPIC_BASE_URL": "https://x",
                "ANTHROPIC_CUSTOM_HEADERS": "a: b",
                "CLAUDE_CODE_USE_BEDROCK": "1",
                "CLAUDE_CODE_USE_VERTEX": "1",
                "CLAUDE_CODE_USE_FOUNDRY": "1",
                "CLAUDE_CODE_MAX_OUTPUT_TOKENS": "64000",
                "AWS_REGION": "us-east-1"
            }
        });
        assert_eq!(
            managed_part(&settings),
            json!({
                "model": "opus",
                "apiKeyHelper": "~/bin/key.sh",
                "env": {
                    "ANTHROPIC_BASE_URL": "https://x",
                    "ANTHROPIC_CUSTOM_HEADERS": "a: b",
                    "CLAUDE_CODE_USE_BEDROCK": "1",
                    "CLAUDE_CODE_USE_VERTEX": "1",
                    "CLAUDE_CODE_USE_FOUNDRY": "1"
                }
            })
        );
        assert_eq!(managed_part(&json!({"hooks": {}})), json!({}));
    }

    #[test]
    fn projection_replaces_managed_keys_and_keeps_the_rest() {
        let root = temp_root();
        let dirs = dirs_in(&root);
        write_live(
            &dirs,
            &json!({
                "model": "old",
                "hooks": {"Stop": []},
                "env": {"ANTHROPIC_AUTH_TOKEN": "old", "ANTHROPIC_MODEL": "m", "KEEP": "1"}
            }),
        )
        .unwrap();
        let profile = json!({
            "env": {"ANTHROPIC_AUTH_TOKEN": "new"},
            "permissions": {"allow": ["Bash"]}
        });
        let old =
            json!({"model": "old", "env": {"ANTHROPIC_AUTH_TOKEN": "old", "ANTHROPIC_MODEL": "m"}});
        project(&dirs, &profile, &[&old]).unwrap();
        assert_eq!(
            read_live(&dirs).unwrap().unwrap(),
            json!({
                "hooks": {"Stop": []},
                "env": {"KEEP": "1", "ANTHROPIC_AUTH_TOKEN": "new"}
            }),
            "stale managed keys go, unmanaged profile keys are not projected"
        );
        assert!(
            backups(&dirs).is_empty(),
            "every dropped value is in a profile"
        );
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn concurrent_write_between_read_and_write_is_a_conflict() {
        let root = temp_root();
        let dirs = dirs_in(&root);
        let path = settings_path(&dirs);
        write_live(&dirs, &json!({"env": {"ANTHROPIC_AUTH_TOKEN": "a"}})).unwrap();
        let external = "{\"env\": {\"ANTHROPIC_AUTH_TOKEN\": \"a\"}, \"hooks\": {}}\n";
        let result = project_with(
            &dirs,
            &json!({"env": {"ANTHROPIC_AUTH_TOKEN": "b"}}),
            &[],
            || std::fs::write(&path, external).unwrap(),
        );
        assert!(matches!(result, Err(AppError::Conflict(_))), "{result:?}");
        assert_eq!(std::fs::read_to_string(&path).unwrap(), external);
        assert!(
            backups(&dirs).is_empty(),
            "no backup for a write that never happened"
        );
        let _ = std::fs::remove_dir_all(root);
    }

    #[cfg(unix)]
    #[test]
    fn backups_are_private_and_only_the_newest_five_are_kept() {
        use std::os::unix::fs::PermissionsExt;
        let root = temp_root();
        let dirs = dirs_in(&root);
        let mut contents = Vec::new();
        for index in 0..7 {
            let token = format!("hand-{index}");
            let text = format!("{{\"env\": {{\"ANTHROPIC_AUTH_TOKEN\": \"{token}\"}}}}\n");
            std::fs::create_dir_all(&dirs.claude_dir).unwrap();
            std::fs::write(settings_path(&dirs), &text).unwrap();
            contents.push(text);
            // The value is in no profile, so overwriting it backs it up.
            project(&dirs, &json!({"env": {"ANTHROPIC_AUTH_TOKEN": "p"}}), &[]).unwrap();
        }
        let names = backups(&dirs);
        assert_eq!(names.len(), BACKUPS_KEPT);
        let kept: Vec<String> = names
            .iter()
            .map(|name| std::fs::read_to_string(dirs.claude_dir.join(name)).unwrap())
            .collect();
        assert_eq!(
            kept,
            contents[2..].to_vec(),
            "oldest two pruned, order kept"
        );
        for name in &names {
            let mode = std::fs::metadata(dirs.claude_dir.join(name))
                .unwrap()
                .permissions()
                .mode();
            assert_eq!(mode & 0o777, 0o600);
        }
        // Re-projecting the same profile drops nothing unknown: no new backup.
        project(&dirs, &json!({"env": {"ANTHROPIC_AUTH_TOKEN": "p"}}), &[]).unwrap();
        assert_eq!(backups(&dirs), names);
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn release_strips_managed_keys_and_never_creates_the_file() {
        let root = temp_root();
        let dirs = dirs_in(&root);
        assert!(!release(&dirs, &[]).unwrap());
        assert!(!settings_path(&dirs).exists());
        write_live(
            &dirs,
            &json!({"model": "m", "env": {"ANTHROPIC_AUTH_TOKEN": "t"}, "hooks": {}}),
        )
        .unwrap();
        let deleted = json!({"model": "m", "env": {"ANTHROPIC_AUTH_TOKEN": "t"}});
        assert!(release(&dirs, &[&deleted]).unwrap());
        assert_eq!(read_live(&dirs).unwrap().unwrap(), json!({"hooks": {}}));
        assert!(backups(&dirs).is_empty());
        let _ = std::fs::remove_dir_all(root);
    }
}
