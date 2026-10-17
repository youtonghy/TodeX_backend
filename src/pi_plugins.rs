//! Pi packages and the configuration files of known Pi extensions, for the
//! "Plugins" tab of the Pi settings.
//!
//! The package list comes from the `packages` and `extensions` arrays of
//! Pi's `settings.json`, with install paths reported by `pi list` and the
//! name, version and description read from each package's `package.json`.
//! Pi has no "disabled" flag for a package; the documented way to keep one
//! installed but load nothing from it is the object form with every
//! resource filter empty, which this module writes and reads back as
//! disabled. Any other object form carries filters set through `pi config`
//! and is reported as such, never rewritten.
//!
//! Extension configuration files are reachable only through
//! [`PLUGIN_CONFIGS`]: a fixed path under the Pi agent directory per known
//! extension. Values that look like credentials are masked on read and kept
//! as stored when a patch sends the mask back.

use std::{
    collections::HashMap,
    fs,
    path::{Path, PathBuf},
    time::Duration,
};

use serde::Serialize;
use serde_json::{Map, Value};

use crate::{
    agent_providers::AgentDirs,
    cli_settings::{self, SettingsDocument, SettingsFile},
    error::AppError,
    provider::process::{run_bounded_command, CommandSpec},
};

/// Pi's built-in extensions, toggled through `-builtin:<id>` entries.
pub(crate) const BUILTIN_EXTENSIONS: [&str; 4] = ["mcp", "codemode", "tool-search", "llama.cpp"];

/// Stands in for a stored credential in responses; a patch that sends it
/// back leaves the stored value alone.
pub(crate) const SECRET_PLACEHOLDER: &str = "__todex_secret__";

const RESOURCE_KINDS: [&str; 4] = ["extensions", "skills", "prompts", "themes"];
const MAX_PACKAGE_JSON_BYTES: u64 = 256 * 1024;
const LIST_TIMEOUT: Duration = Duration::from_secs(20);
const LIST_OUTPUT_BYTES: usize = 256 * 1024;
const MAX_SOURCE_CHARS: usize = 500;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub(crate) enum PackageState {
    Enabled,
    Disabled,
    /// Object form with resource filters the user set; read only here.
    Filtered,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct PiPackage {
    pub source: String,
    pub state: PackageState,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub installed_path: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub version: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct PiBuiltinExtension {
    pub id: String,
    pub enabled: bool,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct PiPluginList {
    pub settings_path: String,
    /// Revision of `settings.json`, for [`set_enabled`].
    pub revision: String,
    pub packages: Vec<PiPackage>,
    pub builtins: Vec<PiBuiltinExtension>,
    /// Why install paths could not be read from `pi list`, when they could not.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub list_error: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub parse_error: Option<String>,
}

/// What [`set_enabled`] switches.
pub(crate) enum ToggleTarget {
    Package(String),
    Builtin(String),
}

/// A known extension's configuration file.
pub(crate) struct PluginConfigSpec {
    pub id: &'static str,
    /// Path segments under the Pi agent directory.
    pub path: &'static [&'static str],
    /// Top-level keys served and accepted; `None` for the whole file.
    pub visible: Option<&'static [&'static str]>,
    pub secrets: SecretRule,
}

#[derive(Clone, Copy)]
pub(crate) enum SecretRule {
    None,
    /// Top-level credential keys (`*ApiKey`, `*Token`, ...) and every string
    /// under `*Headers` / `authFetch`, as in pi-web-access's `web-search.json`.
    WebAccess,
}

pub(crate) const PLUGIN_CONFIGS: &[PluginConfigSpec] = &[
    PluginConfigSpec {
        id: "permission-system",
        path: &["extensions", "pi-permission-system", "config.json"],
        visible: None,
        secrets: SecretRule::None,
    },
    PluginConfigSpec {
        id: "mcp-adapter",
        path: &["mcp-adapter.json"],
        visible: Some(&["settings"]),
        secrets: SecretRule::None,
    },
    PluginConfigSpec {
        id: "web-access",
        path: &["web-search.json"],
        visible: None,
        secrets: SecretRule::WebAccess,
    },
    PluginConfigSpec {
        id: "exa",
        path: &["pi-exa.json"],
        visible: None,
        secrets: SecretRule::None,
    },
    PluginConfigSpec {
        id: "fff",
        path: &["pi-fff.json"],
        visible: None,
        secrets: SecretRule::None,
    },
    PluginConfigSpec {
        id: "subagents",
        path: &["subagents.json"],
        visible: None,
        secrets: SecretRule::None,
    },
    PluginConfigSpec {
        id: "goal",
        path: &["pi-goal.json"],
        visible: None,
        secrets: SecretRule::None,
    },
    PluginConfigSpec {
        id: "plan-mode",
        path: &["pi-plan-mode.json"],
        visible: None,
        secrets: SecretRule::None,
    },
    PluginConfigSpec {
        id: "tasks",
        path: &["tasks-config.json"],
        visible: None,
        secrets: SecretRule::None,
    },
    PluginConfigSpec {
        id: "context-prune",
        path: &["context-prune", "settings.json"],
        visible: None,
        secrets: SecretRule::None,
    },
    PluginConfigSpec {
        id: "cache-optimizer",
        path: &["pi-cache-optimizer-config.json"],
        visible: None,
        secrets: SecretRule::None,
    },
];

/// A known extension's configuration as served: no raw text, masked secrets.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct PluginConfigDocument {
    pub plugin: String,
    pub path: String,
    pub exists: bool,
    pub revision: String,
    pub settings: Option<Map<String, Value>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub parse_error: Option<String>,
}

pub(crate) fn settings_file(dirs: &AgentDirs) -> SettingsFile {
    crate::cli_settings::CliSettingsTarget::Pi.file_in(dirs)
}

/// Lists the configured packages and built-in extensions. `installed` maps a
/// source to its install path, as `pi list` reports it.
pub(crate) fn list(
    dirs: &AgentDirs,
    installed: Result<HashMap<String, String>, String>,
) -> Result<PiPluginList, AppError> {
    let document = cli_settings::read(&settings_file(dirs))?;
    let settings = document.settings.clone().unwrap_or_default();
    let (installed, list_error) = match installed {
        Ok(paths) => (paths, None),
        Err(error) => (HashMap::new(), Some(error)),
    };
    let packages = settings
        .get("packages")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(package_entry)
        .map(|(source, state)| {
            let path = installed
                .get(&source)
                .map(PathBuf::from)
                .or_else(|| fallback_install_path(dirs, &source));
            let manifest = path.as_deref().and_then(read_package_json);
            let text = |key: &str| {
                manifest
                    .as_ref()
                    .and_then(|manifest| manifest.get(key))
                    .and_then(Value::as_str)
                    .map(str::to_owned)
            };
            PiPackage {
                name: text("name"),
                version: text("version"),
                description: text("description"),
                installed_path: path
                    .filter(|path| path.exists())
                    .map(|path| path.display().to_string()),
                source,
                state,
            }
        })
        .collect();
    let disabled = |id: &str| {
        settings
            .get("extensions")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(Value::as_str)
            .any(|entry| entry == format!("-builtin:{id}"))
    };
    Ok(PiPluginList {
        settings_path: document.path,
        revision: document.revision,
        packages,
        builtins: BUILTIN_EXTENSIONS
            .iter()
            .map(|id| PiBuiltinExtension {
                id: (*id).to_owned(),
                enabled: !disabled(id),
            })
            .collect(),
        list_error,
        parse_error: document.parse_error,
    })
}

fn package_entry(entry: &Value) -> Option<(String, PackageState)> {
    match entry {
        Value::String(source) => Some((source.clone(), PackageState::Enabled)),
        Value::Object(object) => {
            let source = object.get("source")?.as_str()?.to_owned();
            let state = if is_disabled_object(object) {
                PackageState::Disabled
            } else {
                PackageState::Filtered
            };
            Some((source, state))
        }
        _ => None,
    }
}

/// `{source, extensions: [], skills: [], prompts: [], themes: []}` exactly.
fn is_disabled_object(object: &Map<String, Value>) -> bool {
    object.len() == 1 + RESOURCE_KINDS.len()
        && RESOURCE_KINDS.iter().all(|kind| {
            object
                .get(*kind)
                .and_then(Value::as_array)
                .is_some_and(Vec::is_empty)
        })
}

fn disabled_object(source: &str) -> Value {
    let mut object = Map::new();
    object.insert("source".to_owned(), Value::String(source.to_owned()));
    for kind in RESOURCE_KINDS {
        object.insert(kind.to_owned(), Value::Array(Vec::new()));
    }
    Value::Object(object)
}

/// `<agent dir>/npm/node_modules/<name>` for an `npm:` source, used when
/// `pi list` could not run.
fn fallback_install_path(dirs: &AgentDirs, source: &str) -> Option<PathBuf> {
    let spec = source.strip_prefix("npm:")?;
    // A version follows the last `@` that is not the scope's leading one.
    let name = match spec[1..].rfind('@') {
        Some(at) => &spec[..at + 1],
        None => spec,
    };
    if name.is_empty() || name.split('/').any(|part| part.is_empty() || part == "..") {
        return None;
    }
    Some(dirs.pi_dir().join("npm").join("node_modules").join(name))
}

fn read_package_json(dir: &Path) -> Option<Map<String, Value>> {
    let path = dir.join("package.json");
    if fs::metadata(&path).ok()?.len() > MAX_PACKAGE_JSON_BYTES {
        return None;
    }
    match serde_json::from_slice(&fs::read(path).ok()?).ok()? {
        Value::Object(manifest) => Some(manifest),
        _ => None,
    }
}

/// Runs `pi list` and maps each user package source to its install path.
pub(crate) async fn installed_paths(
    pi_bin: &str,
    cwd: &Path,
) -> Result<HashMap<String, String>, String> {
    let mut spec = CommandSpec::new(pi_bin, cwd);
    spec.args = vec!["list".to_owned(), "--no-approve".to_owned()];
    spec.env.insert("NO_COLOR".to_owned(), "1".to_owned());
    spec.env.insert("FORCE_COLOR".to_owned(), "0".to_owned());
    let output = run_bounded_command(&spec, LIST_OUTPUT_BYTES, LIST_TIMEOUT)
        .await
        .map_err(|error| error.to_string())?;
    if !output.success {
        let stderr = strip_ansi(&String::from_utf8_lossy(&output.stderr));
        let reason = stderr
            .lines()
            .rev()
            .find(|line| !line.trim().is_empty())
            .unwrap_or("pi list failed");
        return Err(reason.trim().to_owned());
    }
    Ok(parse_pi_list(&strip_ansi(&String::from_utf8_lossy(
        &output.stdout,
    ))))
}

/// Parses the `User packages:` section of `pi list`: a source indented by
/// two spaces (` (filtered)` appended for object entries), then its install
/// path indented by four when it is installed.
pub(crate) fn parse_pi_list(text: &str) -> HashMap<String, String> {
    let mut paths = HashMap::new();
    let mut in_user = false;
    let mut source: Option<String> = None;
    for line in text.lines() {
        if !line.starts_with(' ') {
            in_user = line.trim_end() == "User packages:";
            source = None;
            continue;
        }
        if !in_user {
            continue;
        }
        if let Some(path) = line.strip_prefix("    ") {
            if let Some(source) = source.take() {
                paths.insert(source, path.trim().to_owned());
            }
        } else if let Some(entry) = line.strip_prefix("  ") {
            let entry = entry.trim();
            source = Some(
                entry
                    .strip_suffix(" (filtered)")
                    .unwrap_or(entry)
                    .to_owned(),
            );
        }
    }
    paths
}

pub(crate) fn strip_ansi(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut chars = text.chars().peekable();
    while let Some(ch) = chars.next() {
        if ch == '\u{1b}' {
            if chars.peek() == Some(&'[') {
                chars.next();
                for next in chars.by_ref() {
                    if next.is_ascii_alphabetic() {
                        break;
                    }
                }
            }
            continue;
        }
        out.push(ch);
    }
    out
}

/// Enables or disables one configured package or built-in extension in
/// `settings.json`, if it is still at `expected_revision`.
pub(crate) fn set_enabled(
    dirs: &AgentDirs,
    expected_revision: &str,
    target: &ToggleTarget,
    enabled: bool,
) -> Result<SettingsDocument, AppError> {
    cli_settings::modify(
        &settings_file(dirs),
        expected_revision,
        |settings| match target {
            ToggleTarget::Package(source) => {
                let packages = settings
                    .get_mut("packages")
                    .and_then(Value::as_array_mut)
                    .ok_or_else(|| AppError::NotFound(format!("{source} is not configured")))?;
                let entry = packages
                    .iter_mut()
                    .find(|entry| package_entry(entry).is_some_and(|(found, _)| &found == source))
                    .ok_or_else(|| AppError::NotFound(format!("{source} is not configured")))?;
                match (package_entry(entry).map(|(_, state)| state), enabled) {
                    (Some(PackageState::Filtered), _) => Err(AppError::Conflict(format!(
                        "{source} has resource filters; change it with `pi config`"
                    ))),
                    (_, true) => {
                        *entry = Value::String(source.clone());
                        Ok(())
                    }
                    (_, false) => {
                        *entry = disabled_object(source);
                        Ok(())
                    }
                }
            }
            ToggleTarget::Builtin(id) => {
                if !BUILTIN_EXTENSIONS.contains(&id.as_str()) {
                    return Err(AppError::InvalidRequest(format!(
                        "unknown built-in extension {id}"
                    )));
                }
                let extensions = settings
                    .entry("extensions")
                    .or_insert_with(|| Value::Array(Vec::new()));
                let Value::Array(extensions) = extensions else {
                    return Err(AppError::Conflict(
                        "settings.json `extensions` is not an array".to_owned(),
                    ));
                };
                let (on, off) = (format!("+builtin:{id}"), format!("-builtin:{id}"));
                extensions
                    .retain(|entry| entry.as_str() != Some(&on) && entry.as_str() != Some(&off));
                if !enabled {
                    extensions.push(Value::String(off));
                }
                Ok(())
            }
        },
    )
}

/// Checks a package source given to `pi install` / `pi remove`: one of
/// Pi's source forms, never something the CLI could read as an option.
pub(crate) fn validate_source(source: &str) -> Result<(), AppError> {
    let invalid = |reason: &str| {
        Err(AppError::InvalidRequest(format!(
            "invalid package source: {reason}"
        )))
    };
    if source.is_empty() || source.chars().count() > MAX_SOURCE_CHARS {
        return invalid("empty or too long");
    }
    if source
        .chars()
        .any(|ch| ch.is_control() || ch.is_whitespace())
    {
        return invalid("contains whitespace or control characters");
    }
    let known = ["npm:", "git:", "https://", "http://", "/", "~/"];
    if !known.iter().any(|prefix| source.starts_with(prefix)) {
        return invalid("use npm:<name>, git:<repo>, a URL or an absolute path");
    }
    if source.len() <= 4 || source.ends_with(':') {
        return invalid("missing package name");
    }
    Ok(())
}

pub(crate) fn plugin_spec(id: &str) -> Result<&'static PluginConfigSpec, AppError> {
    PLUGIN_CONFIGS
        .iter()
        .find(|spec| spec.id == id)
        .ok_or_else(|| AppError::NotFound(format!("no configuration for Pi plugin {id}")))
}

fn plugin_file(dirs: &AgentDirs, spec: &PluginConfigSpec) -> SettingsFile {
    let mut path = dirs.pi_dir().to_path_buf();
    path.extend(spec.path);
    SettingsFile::json(path)
}

pub(crate) fn read_plugin_config(
    dirs: &AgentDirs,
    spec: &PluginConfigSpec,
) -> Result<PluginConfigDocument, AppError> {
    Ok(serve(spec, cli_settings::read(&plugin_file(dirs, spec))?))
}

/// Merges `patch` into the file. Each path in `replace` is then set to the
/// patch's value as sent instead of merged, so an object whose key order
/// matters (permission rules, where the last match wins) can be rewritten
/// in a new order.
pub(crate) fn patch_plugin_config(
    dirs: &AgentDirs,
    spec: &PluginConfigSpec,
    expected_revision: &str,
    patch: &Map<String, Value>,
    replace: &[Vec<String>],
) -> Result<PluginConfigDocument, AppError> {
    if let Some(visible) = spec.visible {
        if let Some(key) = patch.keys().find(|key| !visible.contains(&key.as_str())) {
            return Err(AppError::InvalidRequest(format!(
                "{key} cannot be edited here"
            )));
        }
    }
    let replacements = replace
        .iter()
        .map(|path| {
            let value = path
                .iter()
                .try_fold(&Value::Object(patch.clone()), |value, key| value.get(key))
                .filter(|value| value.is_object())
                .cloned()
                .ok_or_else(|| {
                    AppError::InvalidRequest(format!(
                        "replace path {} names no object in the patch",
                        path.join(".")
                    ))
                })?;
            Ok((path, value))
        })
        .collect::<Result<Vec<_>, AppError>>()?;
    let mut patch = patch.clone();
    drop_placeholders(&mut patch);
    let document = cli_settings::modify(&plugin_file(dirs, spec), expected_revision, |settings| {
        let mut document = Value::Object(std::mem::take(settings));
        cli_settings::merge_patch(&mut document, &Value::Object(patch));
        for (path, value) in replacements {
            if let Some(slot) = path
                .iter()
                .try_fold(&mut document, |value, key| value.get_mut(key))
            {
                *slot = value;
            }
        }
        let Value::Object(document) = document else {
            unreachable!("a merge patch keeps an object an object")
        };
        *settings = document;
        Ok(())
    })?;
    Ok(serve(spec, document))
}

fn serve(spec: &PluginConfigSpec, document: SettingsDocument) -> PluginConfigDocument {
    let settings = document.settings.map(|mut settings| {
        if let Some(visible) = spec.visible {
            settings.retain(|key, _| visible.contains(&key.as_str()));
        }
        mask_secrets(spec.secrets, &mut settings);
        settings
    });
    PluginConfigDocument {
        plugin: spec.id.to_owned(),
        path: document.path,
        exists: document.exists,
        revision: document.revision,
        settings,
        parse_error: document.parse_error,
    }
}

fn is_secret_key(key: &str) -> bool {
    ["ApiKey", "ApiToken", "Token", "Secret", "Password"]
        .iter()
        .any(|suffix| key.ends_with(suffix))
}

fn is_secret_tree(key: &str) -> bool {
    key.ends_with("Headers") || key == "authFetch"
}

/// Literal credentials are masked; `$ENV` and `!command` references are not
/// secrets and stay readable.
fn mask_value(value: &mut Value) {
    match value {
        Value::String(text)
            if !text.is_empty() && !text.starts_with('$') && !text.starts_with('!') =>
        {
            *text = SECRET_PLACEHOLDER.to_owned();
        }
        Value::Array(items) => items.iter_mut().for_each(mask_value),
        Value::Object(object) => object.values_mut().for_each(mask_value),
        _ => {}
    }
}

fn mask_secrets(rule: SecretRule, settings: &mut Map<String, Value>) {
    if matches!(rule, SecretRule::None) {
        return;
    }
    for (key, value) in settings.iter_mut() {
        if is_secret_key(key) || is_secret_tree(key) {
            mask_value(value);
        }
    }
}

/// Removes every patch entry that is the placeholder, so masked values the
/// form sends back unchanged keep what is stored.
fn drop_placeholders(patch: &mut Map<String, Value>) {
    patch.retain(|_, value| value.as_str() != Some(SECRET_PLACEHOLDER));
    for value in patch.values_mut() {
        if let Value::Object(object) = value {
            drop_placeholders(object);
        }
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn temp_dirs(name: &str) -> AgentDirs {
        let root = std::env::temp_dir().join(format!("{name}-{}", uuid::Uuid::new_v4().simple()));
        fs::create_dir_all(&root).unwrap();
        AgentDirs::for_tests_with_pi_dir(root)
    }

    fn write_settings(dirs: &AgentDirs, settings: Value) {
        fs::write(
            dirs.pi_dir().join("settings.json"),
            serde_json::to_vec_pretty(&settings).unwrap(),
        )
        .unwrap();
    }

    fn read_settings(dirs: &AgentDirs) -> Value {
        serde_json::from_slice(&fs::read(dirs.pi_dir().join("settings.json")).unwrap()).unwrap()
    }

    #[test]
    fn parses_user_packages_from_pi_list() {
        let text = "User packages:\n  npm:pi-exa\n    /a/node_modules/pi-exa\n  npm:@x/y@1.0.0 (filtered)\n    /a/node_modules/@x/y\n  git:github.com/x/z\n\nProject packages:\n  npm:local-only\n    /p/node_modules/local-only\n";
        let paths = parse_pi_list(text);
        assert_eq!(paths.get("npm:pi-exa").unwrap(), "/a/node_modules/pi-exa");
        assert_eq!(paths.get("npm:@x/y@1.0.0").unwrap(), "/a/node_modules/@x/y");
        assert!(!paths.contains_key("git:github.com/x/z"));
        assert!(!paths.contains_key("npm:local-only"));
        assert_eq!(
            parse_pi_list("\u{1b}[1mUser packages:\u{1b}[22m\n").len(),
            0
        );
        assert_eq!(strip_ansi("\u{1b}[2m    /a\u{1b}[22m"), "    /a");
    }

    #[test]
    fn lists_states_versions_and_builtins() {
        let dirs = temp_dirs("todex-pi-plugins-list");
        let package = dirs.pi_dir().join("npm/node_modules/@x/y");
        fs::create_dir_all(&package).unwrap();
        fs::write(
            package.join("package.json"),
            r#"{"name":"@x/y","version":"1.2.3","description":"Y"}"#,
        )
        .unwrap();
        write_settings(
            &dirs,
            json!({
                "packages": [
                    "npm:@x/y@1.2.3",
                    {"source": "npm:off", "extensions": [], "skills": [], "prompts": [], "themes": []},
                    {"source": "npm:custom", "extensions": ["!a.ts"]},
                ],
                "extensions": ["-builtin:mcp", "+builtin:codemode"],
            }),
        );
        let list = list(&dirs, Err("pi missing".to_owned())).unwrap();
        let states: Vec<_> = list
            .packages
            .iter()
            .map(|p| (p.source.as_str(), p.state))
            .collect();
        assert_eq!(
            states,
            [
                ("npm:@x/y@1.2.3", PackageState::Enabled),
                ("npm:off", PackageState::Disabled),
                ("npm:custom", PackageState::Filtered),
            ]
        );
        assert_eq!(list.packages[0].version.as_deref(), Some("1.2.3"));
        assert_eq!(list.packages[0].description.as_deref(), Some("Y"));
        assert!(list.packages[1].installed_path.is_none());
        assert_eq!(list.list_error.as_deref(), Some("pi missing"));
        let builtins: Vec<_> = list
            .builtins
            .iter()
            .map(|b| (b.id.as_str(), b.enabled))
            .collect();
        assert_eq!(
            builtins,
            [
                ("mcp", false),
                ("codemode", true),
                ("tool-search", true),
                ("llama.cpp", true)
            ]
        );
    }

    #[test]
    fn toggles_packages_and_builtins_in_place() {
        let dirs = temp_dirs("todex-pi-plugins-toggle");
        write_settings(
            &dirs,
            json!({
                "theme": "dark",
                "packages": ["npm:a", {"source": "npm:f", "skills": []}],
                "extensions": ["-builtin:mcp", "./local.ts"],
            }),
        );
        let revision =
            |dirs: &AgentDirs| cli_settings::read(&settings_file(dirs)).unwrap().revision;

        set_enabled(
            &dirs,
            &revision(&dirs),
            &ToggleTarget::Package("npm:a".into()),
            false,
        )
        .unwrap();
        assert_eq!(
            read_settings(&dirs)["packages"][0],
            disabled_object("npm:a")
        );
        set_enabled(
            &dirs,
            &revision(&dirs),
            &ToggleTarget::Package("npm:a".into()),
            true,
        )
        .unwrap();
        assert_eq!(read_settings(&dirs)["packages"][0], json!("npm:a"));

        let filtered = set_enabled(
            &dirs,
            &revision(&dirs),
            &ToggleTarget::Package("npm:f".into()),
            false,
        );
        assert!(matches!(filtered, Err(AppError::Conflict(_))));
        let missing = set_enabled(
            &dirs,
            &revision(&dirs),
            &ToggleTarget::Package("npm:zzz".into()),
            false,
        );
        assert!(matches!(missing, Err(AppError::NotFound(_))));

        set_enabled(
            &dirs,
            &revision(&dirs),
            &ToggleTarget::Builtin("mcp".into()),
            true,
        )
        .unwrap();
        set_enabled(
            &dirs,
            &revision(&dirs),
            &ToggleTarget::Builtin("codemode".into()),
            false,
        )
        .unwrap();
        let settings = read_settings(&dirs);
        assert_eq!(
            settings["extensions"],
            json!(["./local.ts", "-builtin:codemode"])
        );
        assert_eq!(settings["theme"], json!("dark"));

        let stale = set_enabled(&dirs, "stale", &ToggleTarget::Builtin("mcp".into()), false);
        assert!(matches!(stale, Err(AppError::Conflict(_))));
        let unknown = set_enabled(
            &dirs,
            &revision(&dirs),
            &ToggleTarget::Builtin("rm".into()),
            false,
        );
        assert!(matches!(unknown, Err(AppError::InvalidRequest(_))));
    }

    #[test]
    fn plugin_configs_create_keep_unknown_keys_and_mask_secrets() {
        let dirs = temp_dirs("todex-pi-plugins-config");
        let prune = plugin_spec("context-prune").unwrap();
        let created = read_plugin_config(&dirs, prune).unwrap();
        assert!(!created.exists);
        let saved = patch_plugin_config(
            &dirs,
            prune,
            &created.revision,
            json!({"enabled": true}).as_object().unwrap(),
            &[],
        )
        .unwrap();
        assert!(dirs.pi_dir().join("context-prune/settings.json").exists());
        assert_eq!(saved.settings.unwrap()["enabled"], json!(true));

        let web = plugin_spec("web-access").unwrap();
        fs::write(dirs.pi_dir().join("web-search.json"), r#"{"braveApiKey":"sk-1","tavilyApiKey":"$TAVILY","searxngHeaders":{"Authorization":"Bearer x"},"future":1}"#).unwrap();
        let read = read_plugin_config(&dirs, web).unwrap();
        let settings = read.settings.unwrap();
        assert_eq!(settings["braveApiKey"], json!(SECRET_PLACEHOLDER));
        assert_eq!(settings["tavilyApiKey"], json!("$TAVILY"));
        assert_eq!(
            settings["searxngHeaders"]["Authorization"],
            json!(SECRET_PLACEHOLDER)
        );
        let patch = json!({"braveApiKey": SECRET_PLACEHOLDER, "searxngHeaders": {"Authorization": SECRET_PLACEHOLDER}, "workflow": "none"});
        patch_plugin_config(&dirs, web, &read.revision, patch.as_object().unwrap(), &[]).unwrap();
        let stored: Value =
            serde_json::from_slice(&fs::read(dirs.pi_dir().join("web-search.json")).unwrap())
                .unwrap();
        assert_eq!(
            stored,
            json!({"braveApiKey":"sk-1","tavilyApiKey":"$TAVILY","searxngHeaders":{"Authorization":"Bearer x"},"future":1,"workflow":"none"})
        );

        let mcp = plugin_spec("mcp-adapter").unwrap();
        fs::write(
            dirs.pi_dir().join("mcp-adapter.json"),
            r#"{"mcpServers":{"a":{"bearerToken":"t"}},"settings":{"idleTimeout":5}}"#,
        )
        .unwrap();
        let read = read_plugin_config(&dirs, mcp).unwrap();
        assert_eq!(
            Value::Object(read.settings.unwrap()),
            json!({"settings":{"idleTimeout":5}})
        );
        let hidden = patch_plugin_config(
            &dirs,
            mcp,
            &read.revision,
            json!({"mcpServers": null}).as_object().unwrap(),
            &[],
        );
        assert!(matches!(hidden, Err(AppError::InvalidRequest(_))));
        assert!(plugin_spec("../../etc").is_err());
    }

    #[test]
    fn replaced_rule_maps_keep_the_order_sent() {
        let dirs = temp_dirs("todex-pi-plugins-rules");
        let spec = plugin_spec("permission-system").unwrap();
        let path = dirs
            .pi_dir()
            .join("extensions/pi-permission-system/config.json");
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, r#"{"yoloMode":false,"permission":{"*":"ask","bash":{"*":"allow","rm *":"ask","sudo *":"deny"}}}"#).unwrap();
        let read = read_plugin_config(&dirs, spec).unwrap();
        // "rm *" renamed in place and "git push*" put first.
        let patch = json!({"permission": {"bash": {"git push*": "ask", "*": "allow", "rm -rf *": "deny", "sudo *": "deny"}}});
        let replace = [vec!["permission".to_owned(), "bash".to_owned()]];
        patch_plugin_config(
            &dirs,
            spec,
            &read.revision,
            patch.as_object().unwrap(),
            &replace,
        )
        .unwrap();
        let text = fs::read_to_string(&path).unwrap();
        let order: Vec<_> = ["git push*", "\"*\": \"allow", "rm -rf *", "sudo *"]
            .iter()
            .map(|needle| text.find(needle).unwrap())
            .collect();
        assert!(order.windows(2).all(|pair| pair[0] < pair[1]), "{text}");
        assert!(!text.contains("\"rm *\""));
        assert!(text.contains("\"*\": \"ask\""), "untouched keys stay");

        let bad = patch_plugin_config(
            &dirs,
            spec,
            "x",
            patch.as_object().unwrap(),
            &[vec!["nope".to_owned()]],
        );
        assert!(matches!(bad, Err(AppError::InvalidRequest(_))));
    }

    #[test]
    fn validates_package_sources() {
        for ok in [
            "npm:pi-exa",
            "npm:@x/y@1.0.0",
            "git:github.com/x/y@v1",
            "https://github.com/x/y",
            "/opt/pkg",
            "~/pkg",
        ] {
            assert!(validate_source(ok).is_ok(), "{ok}");
        }
        for bad in [
            "", "-e", "--help", "pi-exa", "npm:", "npm:a b", "npm:a\nb", "./rel",
        ] {
            assert!(validate_source(bad).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn falls_back_to_the_npm_install_path() {
        let dirs = temp_dirs("todex-pi-plugins-fallback");
        let root = dirs.pi_dir().join("npm/node_modules");
        assert_eq!(
            fallback_install_path(&dirs, "npm:@x/y@1.0.0"),
            Some(root.join("@x/y"))
        );
        assert_eq!(
            fallback_install_path(&dirs, "npm:pi-exa"),
            Some(root.join("pi-exa"))
        );
        assert_eq!(fallback_install_path(&dirs, "npm:../evil"), None);
        assert_eq!(fallback_install_path(&dirs, "git:github.com/x/y"), None);
    }
}
