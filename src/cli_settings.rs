//! User settings of the CLIs edited from the CLI manager: Claude Code's
//! `<claude_dir>/settings.json`, Codex's `<codex_home>/config.toml`, Pi's
//! `<pi_dir>/settings.json` and DeepSeek Harness's `acp` profile patch
//! (`$DSH_HOME/profiles/acp/cordis.patch.yml`, edited as text only). Every write names the revision it was based on,
//! so an edit made meanwhile by the CLI itself, a provider switch or a text
//! editor surfaces as `CONFLICT` instead of being overwritten. The settings
//! form sends an RFC 7386 merge patch (only the keys it changed); the raw
//! editor replaces the whole text, which is kept byte for byte. TOML patches
//! go through `toml_edit`, so comments and layout around untouched keys stay.

use std::{fs, io, path::PathBuf, sync::Mutex, time::Duration};

use serde::Serialize;
use serde_json::{Map, Value};
use sha2::{Digest, Sha256};
use toml_edit::{DocumentMut, InlineTable, Item, Table, TableLike};

use crate::error::AppError;

/// Largest settings file read or written.
pub(crate) const MAX_SETTINGS_BYTES: usize = 1024 * 1024;
/// The revision of a file that does not exist yet.
const MISSING_REVISION: &str = "missing";

/// Serializes this module's read-check-write cycles within the daemon. The
/// Claude provider projection (`agent_providers::claude`) also takes it
/// around its own read-check-write of `settings.json`, so a provider switch
/// and a settings-page save never interleave.
///
/// Lock order: a caller that also holds an `AgentProviderService` per-agent
/// lock (`lock_for(agent)`) takes that one first and this one second. Code
/// in this module never takes a per-agent lock, so the order cannot invert.
/// Never hold this guard across an `.await`.
pub(crate) static WRITE_LOCK: Mutex<()> = Mutex::new(());

/// A CLI whose user settings file the CLI manager edits.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum CliSettingsTarget {
    ClaudeCode,
    Codex,
    Pi,
    DeepseekHarness,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub(crate) enum SettingsFormat {
    Json,
    Toml,
    /// A dsh patch layer: a YAML list of entries, replaced as a whole text
    /// under dsh's profile lock (see [`crate::dsh_config`]).
    Yaml,
}

impl CliSettingsTarget {
    pub(crate) fn from_provider(provider: &str) -> Option<Self> {
        match provider {
            "claude-code" => Some(Self::ClaudeCode),
            "codex" => Some(Self::Codex),
            "pi" => Some(Self::Pi),
            "deepseek-harness" => Some(Self::DeepseekHarness),
            _ => None,
        }
    }

    pub(crate) fn file(self) -> SettingsFile {
        self.file_in(&crate::agent_providers::AgentDirs::detect())
    }

    pub(crate) fn file_in(self, dirs: &crate::agent_providers::AgentDirs) -> SettingsFile {
        match self {
            Self::ClaudeCode => SettingsFile::json(dirs.claude_dir().join("settings.json")),
            Self::Codex => SettingsFile {
                path: dirs.codex_home().join("config.toml"),
                format: SettingsFormat::Toml,
                pi_lock: false,
            },
            Self::Pi => SettingsFile {
                pi_lock: true,
                ..SettingsFile::json(crate::agent_providers::pi::settings_path(dirs))
            },
            Self::DeepseekHarness => SettingsFile {
                path: crate::dsh_config::DshPaths::of(dirs).profile_patch(),
                format: SettingsFormat::Yaml,
                pi_lock: false,
            },
        }
    }
}

/// A settings file and how to read and write it.
#[derive(Clone, Debug)]
pub(crate) struct SettingsFile {
    pub path: PathBuf,
    pub format: SettingsFormat,
    /// Take Pi's `proper-lockfile` lock (`<file>.lock` directory) around
    /// writes, as Pi does when it saves its own settings.
    pub pi_lock: bool,
}

impl SettingsFile {
    pub(crate) fn json(path: PathBuf) -> Self {
        Self {
            path,
            format: SettingsFormat::Json,
            pi_lock: false,
        }
    }
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct SettingsDocument {
    pub path: String,
    pub format: SettingsFormat,
    pub exists: bool,
    pub revision: String,
    /// The file as stored; empty when it does not exist.
    pub text: String,
    /// The parsed object (TOML as its JSON equivalent, datetimes as
    /// strings); `None` when the text does not parse. Always empty for YAML,
    /// whose entries are a list.
    pub settings: Option<Map<String, Value>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub parse_error: Option<String>,
}

pub(crate) fn read(file: &SettingsFile) -> Result<SettingsDocument, AppError> {
    let (path, format) = (file.path.as_path(), file.format);
    let bytes = match fs::read(path) {
        Ok(bytes) => Some(bytes),
        Err(error) if error.kind() == io::ErrorKind::NotFound => None,
        Err(error) => return Err(error.into()),
    };
    let display = path.display().to_string();
    let Some(bytes) = bytes else {
        return Ok(SettingsDocument {
            path: display,
            format,
            exists: false,
            revision: MISSING_REVISION.to_owned(),
            text: String::new(),
            settings: Some(Map::new()),
            parse_error: None,
        });
    };
    if bytes.len() > MAX_SETTINGS_BYTES {
        return Err(AppError::InvalidRequest(format!(
            "{display} exceeds the 1 MiB limit"
        )));
    }
    let revision = revision(&bytes);
    let text = String::from_utf8(bytes)
        .map_err(|_| AppError::InvalidRequest(format!("{display} is not UTF-8")))?;
    let (settings, parse_error) = match parse_object(&text, format) {
        Ok(settings) => (Some(settings), None),
        Err(error) => (None, Some(error)),
    };
    Ok(SettingsDocument {
        path: display,
        format,
        exists: true,
        revision,
        text,
        settings,
        parse_error,
    })
}

/// Applies `patch` to the stored settings: `null` removes a key, objects
/// merge recursively, anything else (arrays included) replaces the value.
pub(crate) fn apply_patch(
    file: &SettingsFile,
    expected_revision: &str,
    patch: &Map<String, Value>,
) -> Result<SettingsDocument, AppError> {
    if file.format == SettingsFormat::Yaml {
        return Err(AppError::Unsupported(
            "dsh patch files are edited as text".to_owned(),
        ));
    }
    let _guard = WRITE_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let _file_lock = PiLock::acquire(file)?;
    let format = file.format;
    let current = checked(file, expected_revision)?;
    let Some(settings) = current.settings else {
        return Err(AppError::Conflict(format!(
            "{} does not parse; fix it in the raw editor first",
            current.path
        )));
    };
    let bytes = match format {
        SettingsFormat::Json => {
            let mut document = Value::Object(settings);
            merge_patch(&mut document, &Value::Object(patch.clone()));
            let mut bytes = serde_json::to_vec_pretty(&document)?;
            bytes.push(b'\n');
            bytes
        }
        SettingsFormat::Toml => {
            let mut document: DocumentMut = current.text.parse().map_err(|error| {
                AppError::Conflict(format!("{} is not valid TOML: {error}", current.path))
            })?;
            merge_patch_toml(document.as_table_mut(), TomlTableKind::Header, patch)?;
            let text = document.to_string();
            // Validates the result the way readers will see it.
            parse_object(&text, format).map_err(AppError::InvalidRequest)?;
            text.into_bytes()
        }
        SettingsFormat::Yaml => unreachable!("YAML is rejected above"),
    };
    write(file, &bytes)
}

/// Rewrites a JSON settings file through `edit`, under the same revision
/// check and locks as [`apply_patch`]; for edits a merge patch cannot
/// express, such as changing one entry of an array.
pub(crate) fn modify(
    file: &SettingsFile,
    expected_revision: &str,
    edit: impl FnOnce(&mut Map<String, Value>) -> Result<(), AppError>,
) -> Result<SettingsDocument, AppError> {
    if file.format != SettingsFormat::Json {
        return Err(AppError::Unsupported(
            "only JSON settings can be modified in place".to_owned(),
        ));
    }
    let _guard = WRITE_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let _file_lock = PiLock::acquire(file)?;
    let current = checked(file, expected_revision)?;
    let Some(mut settings) = current.settings else {
        return Err(AppError::Conflict(format!(
            "{} does not parse; fix it in the raw editor first",
            current.path
        )));
    };
    edit(&mut settings)?;
    let mut bytes = serde_json::to_vec_pretty(&Value::Object(settings))?;
    bytes.push(b'\n');
    write(file, &bytes)
}

/// Replaces the file with `text`, which must parse as an object/table.
pub(crate) fn replace_text(
    file: &SettingsFile,
    expected_revision: &str,
    text: &str,
) -> Result<SettingsDocument, AppError> {
    if text.len() > MAX_SETTINGS_BYTES {
        return Err(AppError::InvalidRequest(
            "settings text exceeds the 1 MiB limit".to_owned(),
        ));
    }
    parse_object(text, file.format).map_err(AppError::InvalidRequest)?;
    let _guard = WRITE_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    if file.format == SettingsFormat::Yaml {
        // dsh's own writers lock the profile through its manifest, beside
        // the patch; dsh reads the patch at startup, so running processes
        // restart before their next turn.
        let manifest = file.path.with_file_name("package.json");
        return crate::dsh_config::with_lock(&manifest, || {
            checked(file, expected_revision)?;
            let document = write(file, text.as_bytes())?;
            crate::agent_providers::bump_config_revision();
            Ok(document)
        });
    }
    let _file_lock = PiLock::acquire(file)?;
    checked(file, expected_revision)?;
    write(file, text.as_bytes())
}

fn checked(file: &SettingsFile, expected_revision: &str) -> Result<SettingsDocument, AppError> {
    let current = read(file)?;
    if current.revision != expected_revision {
        return Err(AppError::Conflict(format!(
            "{} changed on disk; reload and try again",
            current.path
        )));
    }
    Ok(current)
}

fn write(file: &SettingsFile, bytes: &[u8]) -> Result<SettingsDocument, AppError> {
    if let Some(parent) = file.path.parent() {
        fs::create_dir_all(parent)?;
    }
    crate::secure_fs::replace_keeping_mode(&file.path, bytes)?;
    read(file)
}

/// Pi's settings lock: `proper-lockfile` creates `<file>.lock` as a
/// directory and refreshes its mtime while held; one untouched for longer
/// than its 10 s stale window was left by a crashed process.
struct PiLock(Option<PathBuf>);

impl PiLock {
    const ATTEMPTS: u32 = 50;
    const RETRY: Duration = Duration::from_millis(20);
    const STALE: Duration = Duration::from_secs(10);

    fn acquire(file: &SettingsFile) -> Result<Self, AppError> {
        if !file.pi_lock {
            return Ok(Self(None));
        }
        let mut lock = file.path.clone().into_os_string();
        lock.push(".lock");
        let lock = PathBuf::from(lock);
        if let Some(parent) = file.path.parent() {
            fs::create_dir_all(parent)?;
        }
        for _ in 0..Self::ATTEMPTS {
            match fs::create_dir(&lock) {
                Ok(()) => return Ok(Self(Some(lock))),
                Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
                    let stale = fs::metadata(&lock)
                        .and_then(|meta| meta.modified())
                        .ok()
                        .and_then(|modified| modified.elapsed().ok())
                        .is_some_and(|age| age > Self::STALE);
                    if stale {
                        let _ = fs::remove_dir(&lock);
                        continue;
                    }
                    std::thread::sleep(Self::RETRY);
                }
                Err(error) => return Err(error.into()),
            }
        }
        Err(AppError::Conflict(format!(
            "{} is locked by Pi; try again",
            file.path.display()
        )))
    }
}

impl Drop for PiLock {
    fn drop(&mut self) {
        if let Some(lock) = &self.0 {
            let _ = fs::remove_dir(lock);
        }
    }
}

fn parse_object(text: &str, format: SettingsFormat) -> Result<Map<String, Value>, String> {
    match format {
        SettingsFormat::Json => match serde_json::from_str::<Value>(text) {
            Ok(Value::Object(settings)) => Ok(settings),
            Ok(_) => Err("settings.json must contain a JSON object".to_owned()),
            Err(error) => Err(format!("settings.json is not valid JSON: {error}")),
        },
        SettingsFormat::Toml => match text.parse::<toml::Table>() {
            Ok(table) => Ok(toml_table_to_json(table)),
            Err(error) => Err(format!("config.toml is not valid TOML: {error}")),
        },
        SettingsFormat::Yaml => match serde_yaml_ng::from_str::<serde_yaml_ng::Value>(text) {
            Ok(serde_yaml_ng::Value::Sequence(_) | serde_yaml_ng::Value::Null) => Ok(Map::new()),
            Ok(_) => Err("the dsh patch must be a YAML list of entries".to_owned()),
            Err(error) => Err(format!("the dsh patch is not valid YAML: {error}")),
        },
    }
}

fn toml_table_to_json(table: toml::Table) -> Map<String, Value> {
    table
        .into_iter()
        .map(|(key, value)| (key, toml_to_json(value)))
        .collect()
}

fn toml_to_json(value: toml::Value) -> Value {
    match value {
        toml::Value::String(value) => Value::String(value),
        toml::Value::Integer(value) => Value::from(value),
        toml::Value::Float(value) => serde_json::Number::from_f64(value)
            .map(Value::Number)
            .unwrap_or_else(|| Value::String(value.to_string())),
        toml::Value::Boolean(value) => Value::Bool(value),
        toml::Value::Datetime(value) => Value::String(value.to_string()),
        toml::Value::Array(values) => Value::Array(values.into_iter().map(toml_to_json).collect()),
        toml::Value::Table(table) => Value::Object(toml_table_to_json(table)),
    }
}

pub(crate) fn revision(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

/// How a TOML table is written, which decides how new children are added.
#[derive(Clone, Copy)]
enum TomlTableKind {
    /// `[a]` header (or the document root).
    Header,
    /// `a.b = 1` keys.
    Dotted,
    /// `a = { b = 1 }`.
    Inline,
}

fn toml_table_kind(item: &Item) -> Option<TomlTableKind> {
    match item {
        Item::Table(table) if table.is_dotted() => Some(TomlTableKind::Dotted),
        Item::Table(_) => Some(TomlTableKind::Header),
        Item::Value(toml_edit::Value::InlineTable(_)) => Some(TomlTableKind::Inline),
        _ => None,
    }
}

/// RFC 7386 over a TOML table. Replaced values keep their surrounding
/// comments and spacing; a new nested object follows its parent's style
/// (`[a.b]` under a header table, dotted keys under dotted ones, inline
/// under inline). A table emptied by the patch is removed.
fn merge_patch_toml(
    table: &mut dyn TableLike,
    kind: TomlTableKind,
    patch: &Map<String, Value>,
) -> Result<(), AppError> {
    for (key, value) in patch {
        match value {
            Value::Null => {
                table.remove(key);
            }
            Value::Object(child_patch) => {
                let existing = table.get(key).and_then(toml_table_kind);
                let child_kind = match existing {
                    Some(child_kind) => child_kind,
                    None => {
                        let (fresh, child_kind) = match kind {
                            TomlTableKind::Inline => (
                                Item::Value(toml_edit::Value::InlineTable(InlineTable::new())),
                                TomlTableKind::Inline,
                            ),
                            TomlTableKind::Dotted => {
                                let mut child = Table::new();
                                child.set_dotted(true);
                                (Item::Table(child), TomlTableKind::Dotted)
                            }
                            TomlTableKind::Header => {
                                let mut child = Table::new();
                                child.set_implicit(true);
                                (Item::Table(child), TomlTableKind::Header)
                            }
                        };
                        table.insert(key, fresh);
                        child_kind
                    }
                };
                let child = table
                    .get_mut(key)
                    .and_then(Item::as_table_like_mut)
                    .expect("a table was ensured above");
                merge_patch_toml(child, child_kind, child_patch)?;
                if child.is_empty() {
                    table.remove(key);
                }
            }
            _ => {
                let mut next = crate::catalog::json_to_toml_value(value)?;
                // Assigning through the existing entry keeps the key's own
                // decor (comments above it); the value keeps its trailing one.
                match table.get_mut(key) {
                    Some(item) => {
                        if let Some(previous) = item.as_value() {
                            *next.decor_mut() = previous.decor().clone();
                        }
                        *item = Item::Value(next);
                    }
                    None => {
                        table.insert(key, Item::Value(next));
                    }
                }
            }
        }
    }
    Ok(())
}

pub(crate) fn merge_patch(target: &mut Value, patch: &Value) {
    let Value::Object(patch) = patch else {
        *target = patch.clone();
        return;
    };
    if !target.is_object() {
        *target = Value::Object(Map::new());
    }
    let Value::Object(target) = target else {
        unreachable!()
    };
    for (key, value) in patch {
        if value.is_null() {
            target.remove(key);
        } else {
            merge_patch(target.entry(key.clone()).or_insert(Value::Null), value);
        }
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn temp_dir() -> PathBuf {
        let dir = std::env::temp_dir().join(format!("todex-cli-settings-{}", uuid::Uuid::new_v4()));
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn object(value: Value) -> Map<String, Value> {
        let Value::Object(map) = value else {
            panic!("not an object")
        };
        map
    }

    #[test]
    fn dsh_patch_is_replaced_as_text_and_only_as_a_list() {
        let dir = temp_dir();
        let dirs = crate::agent_providers::AgentDirs::for_tests_with_dsh_home(dir.join("dsh"));
        let file = CliSettingsTarget::DeepseekHarness.file_in(&dirs);
        assert_eq!(file.format, SettingsFormat::Yaml);
        assert!(file.path.ends_with("dsh/profiles/acp/cordis.patch.yml"));
        let missing = read(&file).unwrap();
        assert!(!missing.exists);
        let text = "# mine\n- id: web\n  config:\n    root: !!js process.cwd()\n";
        let written = replace_text(&file, &missing.revision, text).unwrap();
        // Kept byte for byte, tags included.
        assert_eq!(written.text, text);
        assert_eq!(written.settings, Some(Map::new()));
        assert!(!file.path.with_file_name("package.json.lock").exists());
        assert!(matches!(
            replace_text(&file, &written.revision, "id: web\n"),
            Err(AppError::InvalidRequest(_))
        ));
        assert!(matches!(
            replace_text(&file, &missing.revision, "[]\n"),
            Err(AppError::Conflict(_))
        ));
        assert!(matches!(
            apply_patch(&file, &written.revision, &Map::new()),
            Err(AppError::Unsupported(_))
        ));
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn patch_merges_and_removes_keys_keeping_the_rest() {
        let dir = temp_dir();
        let path = dir.join("settings.json");
        let file = SettingsFile::json(path.clone());
        fs::write(
            &path,
            r#"{"model":"opus","permissions":{"allow":["Bash(ls)"],"defaultMode":"plan"},"hooks":{"Stop":[]}}"#,
        )
        .unwrap();
        let current = read(&file).unwrap();
        let saved = apply_patch(
            &file,
            &current.revision,
            &object(json!({
                "permissions": {"defaultMode": null, "deny": ["Read(.env)"]},
                "theme": "dark",
            })),
        )
        .unwrap();
        assert_eq!(
            Value::Object(saved.settings.unwrap()),
            json!({
                "model": "opus",
                "permissions": {"allow": ["Bash(ls)"], "deny": ["Read(.env)"]},
                "hooks": {"Stop": []},
                "theme": "dark",
            })
        );
        assert_ne!(saved.revision, current.revision);
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn stale_revisions_conflict_and_leave_the_file_alone() {
        let dir = temp_dir();
        let path = dir.join("settings.json");
        let file = SettingsFile::json(path.clone());
        let missing = read(&file).unwrap();
        assert!(!missing.exists);
        apply_patch(&file, &missing.revision, &object(json!({"theme": "light"}))).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                fs::metadata(&path).unwrap().permissions().mode() & 0o777,
                0o600
            );
        }
        // Someone else edits the file after our read.
        fs::write(&path, r#"{"theme": "dark"}"#).unwrap();
        for result in [
            apply_patch(&file, &missing.revision, &object(json!({"verbose": true}))),
            replace_text(&file, &missing.revision, "{}"),
        ] {
            assert!(matches!(result, Err(AppError::Conflict(_))));
        }
        assert_eq!(fs::read_to_string(&path).unwrap(), r#"{"theme": "dark"}"#);
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn raw_text_must_be_an_object_and_is_kept_verbatim() {
        let dir = temp_dir();
        let path = dir.join("settings.json");
        let file = SettingsFile::json(path.clone());
        fs::write(&path, "{broken").unwrap();
        let current = read(&file).unwrap();
        assert!(current.settings.is_none() && current.parse_error.is_some());
        assert!(matches!(
            apply_patch(&file, &current.revision, &object(json!({"theme": "dark"}))),
            Err(AppError::Conflict(_))
        ));
        for text in ["[]", "{\"a\":", ""] {
            assert!(matches!(
                replace_text(&file, &current.revision, text),
                Err(AppError::InvalidRequest(_))
            ));
        }
        let text = "{\n    \"theme\": \"dark\"\n}";
        let saved = replace_text(&file, &current.revision, text).unwrap();
        assert_eq!(saved.text, text);
        assert_eq!(fs::read_dir(&dir).unwrap().count(), 1, "no temp files left");
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn toml_patch_keeps_comments_and_layout() {
        let dir = temp_dir();
        let path = dir.join("config.toml");
        let file = SettingsFile {
            path: path.clone(),
            format: SettingsFormat::Toml,
            pi_lock: false,
        };
        let original = "# my config\nmodel = \"gpt-5\" # pinned\napproval_policy = \"on-request\"\nfeatures.web_search_request = true\n\n[tui]\n# desktop pings\nnotifications = true\n\n[mcp_servers.docs]\ncommand = \"docs-mcp\"\n";
        fs::write(&path, original).unwrap();
        let current = read(&file).unwrap();
        assert_eq!(current.format, SettingsFormat::Toml);
        assert_eq!(
            Value::Object(current.settings.clone().unwrap())["tui"],
            json!({"notifications": true})
        );
        let saved = apply_patch(
            &file,
            &current.revision,
            &object(json!({
                "model": "gpt-6",
                "approval_policy": null,
                "features": {"shell_snapshot": true},
                "tui": {"notifications": null},
                "history": {"persistence": "none"},
                "sandbox_workspace_write": {"network_access": true, "writable_roots": ["/tmp/a"]},
            })),
        )
        .unwrap();
        assert_eq!(
            saved.text,
            "# my config\nmodel = \"gpt-6\" # pinned\nfeatures.web_search_request = true\nfeatures.shell_snapshot = true\n\n[mcp_servers.docs]\ncommand = \"docs-mcp\"\n\n[history]\npersistence = \"none\"\n\n[sandbox_workspace_write]\nnetwork_access = true\nwritable_roots = [\"/tmp/a\"]\n"
        );
        assert!(matches!(
            replace_text(&file, &saved.revision, "model = "),
            Err(AppError::InvalidRequest(_))
        ));
        assert!(matches!(
            apply_patch(
                &file,
                &saved.revision,
                &object(json!({"model": null, "x": [1, null]}))
            ),
            Err(AppError::InvalidRequest(_))
        ));
        assert_eq!(read(&file).unwrap().revision, saved.revision);
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn pi_writes_wait_for_its_lock_and_release_it() {
        let dir = temp_dir();
        let path = dir.join("settings.json");
        let file = SettingsFile {
            pi_lock: true,
            ..SettingsFile::json(path.clone())
        };
        let lock = dir.join("settings.json.lock");
        fs::create_dir(&lock).unwrap();
        let missing = read(&file).unwrap();
        assert!(matches!(
            apply_patch(&file, &missing.revision, &object(json!({"theme": "dark"}))),
            Err(AppError::Conflict(_))
        ));
        assert!(!path.exists());
        fs::remove_dir(&lock).unwrap();
        apply_patch(&file, &missing.revision, &object(json!({"theme": "dark"}))).unwrap();
        assert!(!lock.exists());
        fs::remove_dir_all(dir).unwrap();
    }
}
