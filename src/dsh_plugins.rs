//! DeepSeek Harness plugins: the rows of the composed `acp` profile, the
//! bundles (plugin packages) the profile installs, exact-version
//! compatibility exemptions, and the settings of a few known rows.
//!
//! Rows are read from `dsh --dump-config` (see
//! [`crate::provider::deepseek_harness::composed_profile`]), so what TodeX
//! shows is what dsh composes. Turning a row on or off writes its `disabled`
//! into the profile patch layer; selecting a bundle edits the profile
//! manifest's `dsh.profile.bundles`. Installing and removing packages runs
//! `dsh plugin --profile acp add|remove`, which also keeps the bundle list
//! in step (see [`crate::server`]'s `dsh_plugins` routes).

use std::fs;
use std::path::Path;

use serde::Serialize;
use serde_json::{json, Map, Value};

use crate::agent_providers::AgentDirs;
use crate::dsh_config::{
    edit_profile_patch, revision, with_lock, ComposedRow, DshPaths, HomeLayer, PatchFile, RowState,
};
use crate::error::AppError;
use crate::provider::deepseek_harness::LOCKED_ROWS;

/// Bundles the dsh installation supplies to the `acp` template; they cannot
/// be deselected or removed from TodeX.
const INSTALLATION_BUNDLES: [&str; 2] = ["@deepseek-ai/dsh-base", "@deepseek-ai/dsh-acp-app"];
const MAX_SOURCE_CHARS: usize = 500;

/// Where a composed row's layer sits.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) enum RowLayer {
    /// A bundle package's patch.
    Bundle,
    /// The `acp` profile patch (the layer TodeX edits).
    Profile,
    /// `$DSH_HOME/cordis.patch.yml`.
    Home,
    /// TodeX's launch overlay.
    Todex,
    Other,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct DshPluginRow {
    pub id: Option<String>,
    pub name: String,
    pub state: RowState,
    /// The layer that declared the row.
    pub source: String,
    pub source_layer: RowLayer,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub patched_by: Vec<String>,
    /// TodeX's overlay keeps the row off.
    pub locked: bool,
    /// The home layer decides its state.
    pub overridden: bool,
    /// Whether TodeX can turn it on or off.
    pub toggleable: bool,
    /// Whether [`plugin_config_page`] has a settings page for it.
    pub configurable: bool,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct DshPackage {
    pub name: String,
    /// The dependency spec in the profile manifest; `None` for bundles the
    /// installation supplies.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub spec: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub version: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// Whether the package declares a dsh bundle patch.
    pub bundle: bool,
    /// Whether the profile composes it (`dsh.profile.bundles`).
    pub selected: bool,
    /// Supplied by the dsh installation: neither removable nor deselectable.
    pub builtin: bool,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct VersionExemption {
    /// `name@version`.
    pub package: String,
    pub runtime_versions: Vec<String>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct DshPluginList {
    pub profile_path: String,
    /// Revision of the profile patch (row toggles, settings pages).
    pub revision: String,
    /// Revision of the profile manifest (bundle selection).
    pub manifest_revision: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub runtime_version: Option<String>,
    /// Package operations need pnpm on the daemon's PATH.
    pub pnpm_available: bool,
    pub rows: Vec<DshPluginRow>,
    pub packages: Vec<DshPackage>,
    pub exemptions: Vec<VersionExemption>,
    /// Why the composed profile could not be read; rows are empty then.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub compose_error: Option<String>,
    /// Why the profile patch cannot be edited entry by entry.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub parse_error: Option<String>,
}

/// The known rows with settings pages, and the fields each page edits.
struct ConfigPage {
    id: &'static str,
    fields: &'static [&'static str],
}

const CONFIG_PAGES: [ConfigPage; 2] = [
    ConfigPage {
        id: "web",
        fields: &["searchProvider", "fetchProvider"],
    },
    ConfigPage {
        id: "web-deepseek-api",
        fields: &["sendOriginalQuestion", "baseURL"],
    },
];

fn config_page(id: &str) -> Option<&'static ConfigPage> {
    CONFIG_PAGES.iter().find(|page| page.id == id)
}

fn same_path(layer: &str, path: &Path) -> bool {
    Path::new(layer) == path
        || fs::canonicalize(layer)
            .ok()
            .zip(fs::canonicalize(path).ok())
            .is_some_and(|(left, right)| left == right)
}

fn classify(layer: &str, paths: &DshPaths, overlay: &Path) -> RowLayer {
    if (!layer.contains('/') && !layer.contains('\\')) || layer.starts_with('@') {
        RowLayer::Bundle
    } else if same_path(layer, &paths.profile_patch()) {
        RowLayer::Profile
    } else if same_path(layer, &paths.home_patch()) {
        RowLayer::Home
    } else if same_path(layer, overlay) {
        RowLayer::Todex
    } else {
        RowLayer::Other
    }
}

fn read_manifest(paths: &DshPaths) -> Result<(Map<String, Value>, String), AppError> {
    let path = paths.profile_manifest();
    match fs::read(&path) {
        Ok(bytes) => {
            let manifest: Value = serde_json::from_slice(&bytes).map_err(|error| {
                AppError::Conflict(format!("{} is not valid JSON: {error}", path.display()))
            })?;
            let Value::Object(manifest) = manifest else {
                return Err(AppError::Conflict(format!(
                    "{} must hold a JSON object",
                    path.display()
                )));
            };
            Ok((manifest, revision(&bytes)))
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            Ok((Map::new(), "missing".to_owned()))
        }
        Err(error) => Err(error.into()),
    }
}

fn manifest_bundles(manifest: &Map<String, Value>) -> Vec<String> {
    manifest
        .get("dsh")
        .and_then(|dsh| dsh.pointer("/profile/bundles"))
        .and_then(Value::as_array)
        .map(|bundles| {
            bundles
                .iter()
                .filter_map(Value::as_str)
                .map(str::to_owned)
                .collect()
        })
        .unwrap_or_else(|| INSTALLATION_BUNDLES.map(str::to_owned).to_vec())
}

fn packages(paths: &DshPaths, manifest: &Map<String, Value>) -> Vec<DshPackage> {
    let bundles = manifest_bundles(manifest);
    let mut packages: Vec<DshPackage> = bundles
        .iter()
        .filter(|name| INSTALLATION_BUNDLES.contains(&name.as_str()))
        .map(|name| DshPackage {
            name: name.clone(),
            spec: None,
            version: None,
            description: None,
            bundle: true,
            selected: true,
            builtin: true,
        })
        .collect();
    let dependencies = manifest
        .get("dependencies")
        .and_then(Value::as_object)
        .cloned()
        .unwrap_or_default();
    for (name, spec) in dependencies {
        let installed = fs::read(
            paths
                .profile_dir()
                .join("node_modules")
                .join(&name)
                .join("package.json"),
        )
        .ok()
        .and_then(|bytes| serde_json::from_slice::<Value>(&bytes).ok());
        let field = |key: &str| {
            installed
                .as_ref()
                .and_then(|package| package.get(key))
                .and_then(Value::as_str)
                .map(str::to_owned)
        };
        packages.push(DshPackage {
            selected: bundles.contains(&name),
            bundle: installed
                .as_ref()
                .and_then(|package| package.pointer("/dsh/bundle"))
                .is_some(),
            version: field("version"),
            description: field("description"),
            spec: spec.as_str().map(str::to_owned),
            builtin: false,
            name,
        });
    }
    packages
}

fn exemptions(paths: &DshPaths) -> Vec<VersionExemption> {
    let Some(Value::Object(map)) = fs::read(paths.compatibility())
        .ok()
        .and_then(|bytes| serde_json::from_slice::<Value>(&bytes).ok())
    else {
        return Vec::new();
    };
    map.into_iter()
        .map(|(package, versions)| VersionExemption {
            package,
            runtime_versions: versions
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(Value::as_str)
                .map(str::to_owned)
                .collect(),
        })
        .collect()
}

/// The plugin page: composed rows (or why they are missing), packages and
/// exemptions.
pub(crate) fn list(
    dirs: &AgentDirs,
    composed: Result<&[ComposedRow], String>,
    overlay: &Path,
    runtime_version: Option<String>,
    pnpm_available: bool,
) -> Result<DshPluginList, AppError> {
    let paths = DshPaths::of(dirs);
    let patch = PatchFile::read(&paths.profile_patch())?;
    let (manifest, manifest_revision) = read_manifest(&paths)?;
    let home = HomeLayer::read(&paths);
    let (rows, compose_error) = match composed {
        Ok(rows) => (
            rows.iter()
                .map(|row| {
                    let source_layer = classify(&row.layer, &paths, overlay);
                    let locked = row
                        .id
                        .as_deref()
                        .is_some_and(|id| LOCKED_ROWS.contains(&id));
                    let overridden = row
                        .id
                        .as_deref()
                        .is_some_and(|id| home.overrides(id, "disabled"));
                    let toggleable = row.id.is_some()
                        && !locked
                        && !overridden
                        && !matches!(row.state, RowState::Conditional(_))
                        && matches!(source_layer, RowLayer::Bundle | RowLayer::Profile)
                        && patch.layout().is_some();
                    DshPluginRow {
                        configurable: row.id.as_deref().and_then(config_page).is_some(),
                        id: row.id.clone(),
                        name: row.name.clone(),
                        state: row.state.clone(),
                        source: row.layer.clone(),
                        source_layer,
                        patched_by: row.patched_by.clone(),
                        locked,
                        overridden,
                        toggleable,
                    }
                })
                .collect(),
            None,
        ),
        Err(error) => (Vec::new(), Some(error)),
    };
    Ok(DshPluginList {
        profile_path: patch.path.clone(),
        revision: patch.revision.clone(),
        manifest_revision,
        runtime_version,
        pnpm_available,
        rows,
        packages: packages(&paths, &manifest),
        exemptions: exemptions(&paths),
        compose_error,
        parse_error: patch.parse_error.clone(),
    })
}

/// Turns composed row `id` on or off in the profile patch. A row the
/// profile layer inserted is edited in place; any other gets a `disabled`
/// override.
pub(crate) fn set_row_enabled(
    dirs: &AgentDirs,
    composed: &[ComposedRow],
    overlay: &Path,
    expected_revision: &str,
    id: &str,
    enabled: bool,
) -> Result<(), AppError> {
    let paths = DshPaths::of(dirs);
    if LOCKED_ROWS.contains(&id) {
        return Err(AppError::Conflict(format!(
            "TodeX keeps `{id}` off; it uploads data to the DeepSeek API"
        )));
    }
    let row = composed
        .iter()
        .find(|row| row.id.as_deref() == Some(id))
        .ok_or_else(|| AppError::NotFound(format!("no `{id}` plugin in the acp profile")))?;
    if matches!(row.state, RowState::Conditional(_)) {
        return Err(AppError::Conflict(format!(
            "`{id}` is switched by an expression; edit it in the YAML file"
        )));
    }
    HomeLayer::read(&paths).ensure_not_overridden(&paths, id, "disabled")?;
    let inserted_here = classify(&row.layer, &paths, overlay) == RowLayer::Profile;
    edit_profile_patch(&paths, Some(expected_revision), |layout| {
        if inserted_here {
            let mut inserted = layout
                .inserted_rows()
                .into_iter()
                .find(|(row, _)| row.get("id").and_then(Value::as_str) == Some(id))
                .map(|(row, _)| row.clone());
            if let Some(row) = inserted.as_mut() {
                if enabled {
                    row.remove("disabled");
                } else {
                    row.insert("disabled".to_owned(), Value::Bool(true));
                }
                return layout.set_inserted_row(id, inserted);
            }
        }
        layout.set_patch_key(id, "disabled", Some(Value::Bool(!enabled)))
    })
    .map(|_| ())
}

/// Selects or deselects an installed bundle in the profile manifest.
/// Selecting appends it last, as dsh's plugin manager does.
pub(crate) fn set_bundle_selected(
    dirs: &AgentDirs,
    expected_revision: &str,
    name: &str,
    selected: bool,
) -> Result<(), AppError> {
    if INSTALLATION_BUNDLES.contains(&name) {
        return Err(AppError::Conflict(format!(
            "{name} is part of the dsh installation"
        )));
    }
    let paths = DshPaths::of(dirs);
    let manifest_path = paths.profile_manifest();
    with_lock(&manifest_path, || {
        let (mut manifest, current) = read_manifest(&paths)?;
        if current != expected_revision {
            return Err(AppError::Conflict(
                "the dsh profile manifest changed since it was read; reload and retry".to_owned(),
            ));
        }
        let installed = manifest
            .get("dependencies")
            .and_then(Value::as_object)
            .is_some_and(|dependencies| dependencies.contains_key(name));
        if !installed {
            return Err(AppError::NotFound(format!(
                "{name} is not installed in the acp profile"
            )));
        }
        let mut bundles = manifest_bundles(&manifest);
        let present = bundles.iter().any(|bundle| bundle == name);
        match (selected, present) {
            (true, false) => bundles.push(name.to_owned()),
            (false, true) => bundles.retain(|bundle| bundle != name),
            _ => return Ok(()),
        }
        let dsh = manifest.entry("dsh").or_insert_with(|| json!({}));
        if !dsh.is_object() {
            *dsh = json!({});
        }
        let profile = dsh
            .as_object_mut()
            .expect("dsh normalized to an object")
            .entry("profile")
            .or_insert_with(|| json!({}));
        if !profile.is_object() {
            *profile = json!({});
        }
        profile
            .as_object_mut()
            .expect("profile normalized to an object")
            .insert("bundles".to_owned(), json!(bundles));
        let mut bytes = serde_json::to_vec_pretty(&Value::Object(manifest))?;
        bytes.push(b'\n');
        crate::secure_fs::replace_keeping_mode(&manifest_path, &bytes)?;
        crate::agent_providers::bump_config_revision();
        Ok(())
    })
}

/// A known row's settings page: its effective config, as composed.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct PluginConfigDocument {
    pub id: String,
    /// Revision of the profile patch the page writes.
    pub revision: String,
    pub config: Map<String, Value>,
    /// The home layer sets this row's config; the page is read-only.
    pub overridden: bool,
}

fn effective_config(composed: &[ComposedRow], id: &str) -> Result<Map<String, Value>, AppError> {
    let row = composed
        .iter()
        .find(|row| row.id.as_deref() == Some(id))
        .ok_or_else(|| AppError::NotFound(format!("no `{id}` plugin in the acp profile")))?;
    Ok(row
        .config
        .as_ref()
        .and_then(Value::as_object)
        .cloned()
        .unwrap_or_default())
}

pub(crate) fn plugin_config_page(
    dirs: &AgentDirs,
    composed: &[ComposedRow],
    id: &str,
) -> Result<PluginConfigDocument, AppError> {
    let page = config_page(id)
        .ok_or_else(|| AppError::NotFound(format!("`{id}` has no settings page")))?;
    let paths = DshPaths::of(dirs);
    let config = effective_config(composed, id)?
        .into_iter()
        .filter(|(key, _)| page.fields.contains(&key.as_str()))
        .collect();
    Ok(PluginConfigDocument {
        id: id.to_owned(),
        revision: PatchFile::read(&paths.profile_patch())?.revision,
        config,
        overridden: HomeLayer::read(&paths).overrides(id, "config"),
    })
}

fn validate_field(id: &str, key: &str, value: &Value) -> Result<(), AppError> {
    let one_of = |allowed: &[&str]| value.as_str().is_some_and(|value| allowed.contains(&value));
    let valid = match (id, key) {
        ("web", "searchProvider") => one_of(&["deepseek-official", "deepseek-search-api"]),
        ("web", "fetchProvider") => one_of(&["http", "deepseek-fetch-api"]),
        ("web-deepseek-api", "sendOriginalQuestion") => value.is_boolean(),
        ("web-deepseek-api", "baseURL") => value
            .as_str()
            .is_some_and(|url| url.starts_with("https://") || url.starts_with("http://")),
        _ => false,
    };
    if valid {
        Ok(())
    } else {
        Err(AppError::InvalidRequest(format!(
            "invalid value for `{key}` of `{id}`"
        )))
    }
}

/// Applies `patch` (top-level fields; `null` restores the composed value)
/// to a known row and pins the whole resulting config in the profile patch:
/// dsh replaces a row's config wholesale, so a partial one would drop the
/// bundle's other fields.
pub(crate) fn patch_plugin_config(
    dirs: &AgentDirs,
    composed: &[ComposedRow],
    id: &str,
    expected_revision: &str,
    patch: &Map<String, Value>,
) -> Result<(), AppError> {
    let page = config_page(id)
        .ok_or_else(|| AppError::NotFound(format!("`{id}` has no settings page")))?;
    let paths = DshPaths::of(dirs);
    HomeLayer::read(&paths).ensure_not_overridden(&paths, id, "config")?;
    let mut config = effective_config(composed, id)?;
    for (key, value) in patch {
        if !page.fields.contains(&key.as_str()) {
            return Err(AppError::InvalidRequest(format!(
                "`{key}` is not a setting of `{id}`"
            )));
        }
        if value.is_null() {
            continue;
        }
        validate_field(id, key, value)?;
        config.insert(key.clone(), value.clone());
    }
    edit_profile_patch(&paths, Some(expected_revision), |layout| {
        layout.set_patch_key(id, "config", Some(Value::Object(config)))
    })
    .map(|_| ())
}

/// A package spec `dsh plugin add` accepts: a registry name (optionally
/// with a version), a git or tarball URL, or a local path. Never an option.
pub(crate) fn validate_source(source: &str) -> Result<(), AppError> {
    if source.is_empty()
        || source.chars().count() > MAX_SOURCE_CHARS
        || source.starts_with('-')
        || source
            .chars()
            .any(|char| char.is_whitespace() || char.is_control())
    {
        return Err(AppError::InvalidRequest(
            "a plugin source is a package name, git or tarball URL, or path without spaces"
                .to_owned(),
        ));
    }
    Ok(())
}

/// `name@version` of an exemption target, and a plain dsh version.
pub(crate) fn validate_exemption(package: &str, runtime_version: &str) -> Result<(), AppError> {
    let valid_version = |version: &str| {
        !version.is_empty()
            && version.len() <= 64
            && version
                .chars()
                .all(|char| char.is_ascii_alphanumeric() || matches!(char, '.' | '-' | '+'))
    };
    let target = package
        .rsplit_once('@')
        .filter(|(name, version)| !name.is_empty() && name != &"@" && valid_version(version));
    if target.is_none() || validate_source(package).is_err() || !valid_version(runtime_version) {
        return Err(AppError::InvalidRequest(
            "an exemption names an exact package@version and dsh version".to_owned(),
        ));
    }
    Ok(())
}

/// A package `dsh plugin` refused for its version, with the exemption that
/// would admit it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct IncompatiblePackage {
    pub package: String,
    pub runtime_version: String,
}

/// The `allow-version` hints `dsh plugin` prints for refused packages.
pub(crate) fn incompatible_packages(stderr: &str) -> Vec<IncompatiblePackage> {
    stderr
        .lines()
        .filter_map(|line| line.split_once(" allow-version ").map(|(_, rest)| rest))
        .filter_map(|rest| {
            let mut words = rest.split_whitespace();
            let package = words.next()?;
            let mut runtime = None;
            while let Some(word) = words.next() {
                if word == "--dsh-version" {
                    runtime = words.next();
                }
            }
            Some(IncompatiblePackage {
                package: package.to_owned(),
                runtime_version: runtime?.to_owned(),
            })
        })
        .collect()
}

/// The line of `dsh plugin` output that says what went wrong.
pub(crate) fn failure_reason(stderr: &str, stdout: &str) -> String {
    let noise = |line: &str| {
        line.is_empty()
            || line.starts_with("dsh: plugin command failed")
            || line.starts_with("dsh: to accept the risk")
            || line.starts_with("dsh: nothing was installed")
    };
    let pick = |text: &str| {
        let lines: Vec<&str> = text.lines().map(str::trim).collect();
        lines
            .iter()
            .find(|line| line.starts_with("dsh: installation rejected"))
            .or_else(|| lines.iter().rev().find(|line| !noise(line)))
            .map(|line| line.chars().take(500).collect::<String>())
    };
    pick(stderr)
        .or_else(|| pick(stdout))
        .unwrap_or_else(|| "dsh plugin exited with an error".to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn dirs() -> (PathBuf, AgentDirs) {
        let root = std::env::temp_dir().join(format!("todex-dsh-plugins-{}", uuid::Uuid::new_v4()));
        fs::create_dir_all(&root).unwrap();
        (
            root.clone(),
            AgentDirs::for_tests_with_dsh_home(root.join("dsh")),
        )
    }

    fn row(id: &str, layer: &str, state: RowState, config: Option<Value>) -> ComposedRow {
        ComposedRow {
            id: Some(id.to_owned()),
            name: format!("@deepseek-ai/dsh-{id}"),
            state,
            layer: layer.to_owned(),
            patched_by: Vec::new(),
            config,
        }
    }

    fn composed(paths: &DshPaths) -> Vec<ComposedRow> {
        let profile = paths.profile_patch().to_string_lossy().into_owned();
        vec![
            row(
                "tool-todo",
                "@deepseek-ai/dsh-base",
                RowState::Enabled,
                None,
            ),
            row(
                "plugin-manager",
                "@deepseek-ai/dsh-base",
                RowState::Conditional("!ctx".to_owned()),
                None,
            ),
            row(
                "session-log-deepseek",
                "@deepseek-ai/dsh-base",
                RowState::Disabled,
                None,
            ),
            row(
                "web",
                "@deepseek-ai/dsh-base",
                RowState::Enabled,
                Some(json!({"searchProvider": "deepseek-official", "fetchProvider": "http"})),
            ),
            row("mcp-mine", &profile, RowState::Enabled, None),
        ]
    }

    #[test]
    fn rows_report_where_they_come_from_and_what_can_change() {
        let (root, dirs) = dirs();
        let paths = DshPaths::of(&dirs);
        let rows = composed(&paths);
        let list = list(
            &dirs,
            Ok(&rows),
            Path::new("/x/todex.patch.yml"),
            None,
            true,
        )
        .unwrap();
        let by_id = |id: &str| {
            list.rows
                .iter()
                .find(|row| row.id.as_deref() == Some(id))
                .unwrap()
        };
        assert!(by_id("tool-todo").toggleable);
        assert_eq!(by_id("tool-todo").source_layer, RowLayer::Bundle);
        assert!(!by_id("plugin-manager").toggleable);
        assert!(by_id("session-log-deepseek").locked);
        assert!(!by_id("session-log-deepseek").toggleable);
        assert!(by_id("web").configurable);
        assert_eq!(by_id("mcp-mine").source_layer, RowLayer::Profile);
        // The default acp manifest composes the two installation bundles.
        assert_eq!(list.packages.len(), 2);
        assert!(list.packages.iter().all(|package| package.builtin));
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn toggles_write_disabled_overrides_and_edit_own_inserts() {
        let (root, dirs) = dirs();
        let paths = DshPaths::of(&dirs);
        fs::create_dir_all(paths.profile_dir()).unwrap();
        fs::write(
            paths.profile_patch(),
            "- insert:\n    - id: mcp-mine\n      name: '@deepseek-ai/dsh-mcp-client'\n",
        )
        .unwrap();
        let rows = composed(&paths);
        let overlay = Path::new("/x/todex.patch.yml");
        let revision = PatchFile::read(&paths.profile_patch()).unwrap().revision;
        set_row_enabled(&dirs, &rows, overlay, &revision, "tool-todo", false).unwrap();
        let file = PatchFile::read(&paths.profile_patch()).unwrap();
        assert_eq!(
            file.layout().unwrap().patch_value("tool-todo", "disabled"),
            Some(&json!(true))
        );
        set_row_enabled(&dirs, &rows, overlay, &file.revision, "mcp-mine", false).unwrap();
        let file = PatchFile::read(&paths.profile_patch()).unwrap();
        assert_eq!(
            file.layout().unwrap().inserted_rows()[0].0["disabled"],
            json!(true)
        );
        for refused in ["session-log-deepseek", "plugin-manager"] {
            assert!(matches!(
                set_row_enabled(&dirs, &rows, overlay, &file.revision, refused, true),
                Err(AppError::Conflict(_))
            ));
        }
        assert!(matches!(
            set_row_enabled(&dirs, &rows, overlay, "stale", "tool-todo", true),
            Err(AppError::Conflict(_))
        ));
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn settings_pages_pin_the_whole_config() {
        let (root, dirs) = dirs();
        let paths = DshPaths::of(&dirs);
        let rows = composed(&paths);
        let page = plugin_config_page(&dirs, &rows, "web").unwrap();
        assert_eq!(page.config["fetchProvider"], "http");
        let patch = json!({"searchProvider": "deepseek-search-api"});
        patch_plugin_config(
            &dirs,
            &rows,
            "web",
            &page.revision,
            patch.as_object().unwrap(),
        )
        .unwrap();
        let file = PatchFile::read(&paths.profile_patch()).unwrap();
        assert_eq!(
            file.layout().unwrap().patch_value("web", "config"),
            Some(&json!({"searchProvider": "deepseek-search-api", "fetchProvider": "http"}))
        );
        let bad = json!({"searchProvider": "bing"});
        assert!(patch_plugin_config(
            &dirs,
            &rows,
            "web",
            &file.revision,
            bad.as_object().unwrap()
        )
        .is_err());
        assert!(plugin_config_page(&dirs, &rows, "tool-todo").is_err());
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn bundles_are_selected_in_the_manifest() {
        let (root, dirs) = dirs();
        let paths = DshPaths::of(&dirs);
        fs::create_dir_all(paths.profile_dir()).unwrap();
        fs::write(
            paths.profile_manifest(),
            r#"{"name":"dsh-profile-acp","private":true,"dependencies":{"@x/bundle":"1.0.0"},"dsh":{"profile":{"bundles":["@deepseek-ai/dsh-base","@deepseek-ai/dsh-acp-app","@x/bundle"]}}}"#,
        )
        .unwrap();
        let (_, revision) = read_manifest(&paths).unwrap();
        set_bundle_selected(&dirs, &revision, "@x/bundle", false).unwrap();
        let (manifest, revision) = read_manifest(&paths).unwrap();
        assert_eq!(
            manifest_bundles(&manifest),
            ["@deepseek-ai/dsh-base", "@deepseek-ai/dsh-acp-app"]
        );
        assert_eq!(manifest["name"], "dsh-profile-acp");
        assert!(set_bundle_selected(&dirs, &revision, "@deepseek-ai/dsh-base", false).is_err());
        assert!(set_bundle_selected(&dirs, &revision, "@y/missing", true).is_err());
        let list = list(&dirs, Ok(&[]), Path::new("/x"), None, false).unwrap();
        let package = list
            .packages
            .iter()
            .find(|package| package.name == "@x/bundle")
            .unwrap();
        assert!(!package.selected && !package.builtin);
        assert_eq!(package.spec.as_deref(), Some("1.0.0"));
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn refusals_name_the_exemption_that_would_admit_them() {
        let stderr = "\
dsh: installation rejected: Plugin @deepseek-ai/dsh-subagent-codex@0.2.1-alpha.2 is incompatible with dsh 0.2.1-rc.2: requirements {}.
dsh: nothing was installed.
dsh: to accept the risk, run: dsh plugin --profile acp allow-version @deepseek-ai/dsh-subagent-codex@0.2.1-alpha.2 --dsh-version 0.2.1-rc.2 --accept-risk
dsh: plugin command failed; diagnostics: /tmp/log
";
        assert_eq!(
            incompatible_packages(stderr),
            [IncompatiblePackage {
                package: "@deepseek-ai/dsh-subagent-codex@0.2.1-alpha.2".to_owned(),
                runtime_version: "0.2.1-rc.2".to_owned(),
            }]
        );
        assert!(failure_reason(stderr, "").starts_with("dsh: installation rejected: Plugin"));
        assert_eq!(
            failure_reason(
                "ERR_PNPM_FETCH_404 not found\ndsh: plugin command failed; x\n",
                ""
            ),
            "ERR_PNPM_FETCH_404 not found"
        );
    }

    #[test]
    fn sources_and_exemptions_are_validated() {
        assert!(validate_source("@deepseek-ai/dsh-subagent-codex@0.2.1-rc.2").is_ok());
        assert!(validate_source("github:me/plugin#v1").is_ok());
        assert!(validate_source("--registry=evil").is_err());
        assert!(validate_source("a b").is_err());
        assert!(validate_exemption("@scope/pkg@1.2.3", "0.2.1-rc.2").is_ok());
        assert!(validate_exemption("@scope/pkg", "0.2.1").is_err());
        assert!(validate_exemption("pkg@1.0.0", "0.2.1 --x").is_err());
    }
}
