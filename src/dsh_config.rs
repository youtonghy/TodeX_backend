//! DeepSeek Harness (dsh) configuration files that TodeX reads and edits.
//!
//! dsh composes its `acp` profile from the bundle patch layers, then
//! `$DSH_HOME/profiles/acp/cordis.patch.yml` (the profile's user layer, the
//! one dsh's own Settings write), then `$DSH_HOME/cordis.patch.yml`, then
//! every `--patch` overlay. TodeX writes only the profile layer, and keeps
//! every top-level entry it does not change byte for byte: YAML libraries
//! drop the `!!js` tags dsh entries may carry, so entries are spliced as text
//! and only the entries TodeX itself renders are serialized. An entry's
//! `config` replaces the composed one wholesale, so TodeX only writes whole
//! configs it owns or read in full from `dsh --dump-config`.
//!
//! API keys live in `refs` of `$DSH_HOME/.credentials.yaml` (mode 0600),
//! which dsh reloads live and prefers over the environment. Every writer
//! takes dsh's cross-process `<file>.lock` (the profile's `package.json` for
//! profile files), so edits never interleave with `dsh web` or `dsh plugin`.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use serde::Serialize;
use serde_json::{Map, Value};
use sha2::{Digest, Sha256};

use crate::agent_providers::{files, AgentDirs};
use crate::error::AppError;

/// The profile TodeX launches (`dsh --profile acp`).
pub(crate) const PROFILE: &str = "acp";
const MISSING_REVISION: &str = "missing";
const LOCK_WAIT: Duration = Duration::from_secs(10);

/// The dsh files under one `$DSH_HOME`.
#[derive(Clone, Debug)]
pub(crate) struct DshPaths {
    home: PathBuf,
}

impl DshPaths {
    pub(crate) fn of(dirs: &AgentDirs) -> Self {
        Self {
            home: dirs.dsh_home().to_owned(),
        }
    }

    #[cfg(test)]
    pub(crate) fn home(&self) -> &Path {
        &self.home
    }

    pub(crate) fn profile_dir(&self) -> PathBuf {
        self.home.join("profiles").join(PROFILE)
    }

    /// The `acp` profile's user patch layer.
    pub(crate) fn profile_patch(&self) -> PathBuf {
        self.profile_dir().join("cordis.patch.yml")
    }

    /// The profile manifest; its lock serializes every profile write.
    pub(crate) fn profile_manifest(&self) -> PathBuf {
        self.profile_dir().join("package.json")
    }

    /// Exact-version plugin exemptions of the profile.
    pub(crate) fn compatibility(&self) -> PathBuf {
        self.profile_dir().join("compatibility.json")
    }

    /// The home-level patch layer every profile shares; it wins over the
    /// profile layer.
    pub(crate) fn home_patch(&self) -> PathBuf {
        self.home.join("cordis.patch.yml")
    }

    pub(crate) fn credentials(&self) -> PathBuf {
        self.home.join(".credentials.yaml")
    }

    /// The global instructions dsh reads before the shared `~/.agents` file.
    pub(crate) fn instructions(&self) -> PathBuf {
        self.home.join("AGENTS.md")
    }
}

pub(crate) fn revision(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

fn read_bytes(path: &Path, label: &str) -> Result<Option<Vec<u8>>, AppError> {
    match files::read_limited(path, label) {
        Ok(bytes) => Ok(Some(bytes)),
        Err(AppError::Io(error)) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error),
    }
}

// ---------- dsh's writer lock ----------

/// Runs `operation` while holding dsh's writer lock for `target`: an
/// exclusively created `<target>.lock` holding this process's pid. A lock
/// whose holder has exited is taken over, as dsh does; a live holder is
/// waited for, then reported as a conflict.
pub(crate) fn with_lock<T>(
    target: &Path,
    operation: impl FnOnce() -> Result<T, AppError>,
) -> Result<T, AppError> {
    let mut lock = target.as_os_str().to_owned();
    lock.push(".lock");
    let lock = PathBuf::from(lock);
    if let Some(parent) = lock.parent() {
        fs::create_dir_all(parent)?;
    }
    let deadline = Instant::now() + LOCK_WAIT;
    let mut delay = Duration::from_millis(20);
    loop {
        let mut options = fs::OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        match options.open(&lock) {
            Ok(mut file) => {
                let _ = writeln!(file, "{}", std::process::id());
                break;
            }
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                if holder_exited(&lock) {
                    let _ = fs::remove_file(&lock);
                    continue;
                }
            }
            Err(error) => return Err(AppError::Io(error)),
        }
        if Instant::now() >= deadline {
            return Err(AppError::Conflict(format!(
                "another dsh writer holds {}; try again",
                lock.display()
            )));
        }
        std::thread::sleep(delay);
        delay = (delay * 2).min(Duration::from_millis(500));
    }
    let result = operation();
    let _ = fs::remove_file(&lock);
    result
}

/// Whether the pid recorded in `lock` names a process that no longer runs.
/// An unreadable or incomplete record counts as live, like dsh treats it.
fn holder_exited(lock: &Path) -> bool {
    let Some(pid) = fs::read_to_string(lock)
        .ok()
        .and_then(|text| text.trim().parse::<i32>().ok())
        .filter(|pid| *pid > 0)
    else {
        return false;
    };
    #[cfg(unix)]
    {
        // SAFETY: signal 0 only checks that the process exists.
        let alive = unsafe { libc::kill(pid, 0) } == 0;
        !alive && std::io::Error::last_os_error().raw_os_error() == Some(libc::ESRCH)
    }
    #[cfg(not(unix))]
    {
        let _ = pid;
        false
    }
}

// ---------- patch layers ----------

/// One top-level entry of a patch file, kept as its source lines.
#[derive(Clone, Debug)]
struct Entry {
    /// Comment and blank lines between the previous entry and this one.
    lead: Vec<String>,
    /// The `- ` line and its continuation lines.
    body: Vec<String>,
    value: Map<String, Value>,
    /// Whether the entry carries YAML tags such as `!!js`, which TodeX cannot
    /// reproduce; such entries are never rewritten.
    expressions: bool,
}

impl Entry {
    fn rendered(value: Map<String, Value>) -> Result<Self, AppError> {
        let text = serde_yaml_ng::to_string(&vec![Value::Object(value.clone())])
            .map_err(|error| AppError::Anyhow(error.into()))?;
        Ok(Self {
            lead: Vec::new(),
            body: text.lines().map(str::to_owned).collect(),
            value,
            expressions: false,
        })
    }

    fn rerender(&mut self) -> Result<(), AppError> {
        let lead = std::mem::take(&mut self.lead);
        *self = Self::rendered(std::mem::take(&mut self.value))?;
        self.lead = lead;
        Ok(())
    }

    fn patch_id(&self) -> Option<&str> {
        if self.value.contains_key("insert") {
            return None;
        }
        self.value.get("id").and_then(Value::as_str)
    }

    /// Rows a top-level `insert` (without a group `id`) adds.
    fn inserted(&self) -> Option<&Vec<Value>> {
        if self.value.contains_key("id") {
            return None;
        }
        self.value.get("insert").and_then(Value::as_array)
    }

    fn editable(&self, what: &str) -> Result<(), AppError> {
        if self.expressions {
            return Err(AppError::Conflict(format!(
                "{what} uses YAML expressions (such as !!js); edit it in the YAML file"
            )));
        }
        Ok(())
    }
}

/// The entries of a patch file, or why they cannot be edited one by one.
#[derive(Clone, Debug)]
pub(crate) struct PatchLayout {
    header: Vec<String>,
    entries: Vec<Entry>,
    trailer: Vec<String>,
}

fn has_tag(line: &str) -> bool {
    let content = line.split(" #").next().unwrap_or(line);
    content
        .split_whitespace()
        .any(|token| token.starts_with('!') && token.len() > 1)
}

/// Splits `text` into top-level entries. Only a block list at column 0 (or
/// dsh's empty `[]` template) can be edited entry by entry.
fn layout(text: &str) -> Result<PatchLayout, String> {
    let parsed: serde_yaml_ng::Value =
        serde_yaml_ng::from_str(text).map_err(|error| format!("invalid YAML: {error}"))?;
    let expected = match &parsed {
        serde_yaml_ng::Value::Null => 0,
        serde_yaml_ng::Value::Sequence(entries) => entries.len(),
        _ => return Err("the file must be a YAML list of patch entries".to_owned()),
    };
    let mut header = Vec::new();
    let mut entries: Vec<Entry> = Vec::new();
    let mut pending: Vec<String> = Vec::new();
    for line in text.lines() {
        if line == "-" || line.starts_with("- ") {
            let lead = std::mem::take(&mut pending);
            if entries.is_empty() {
                header.extend(lead);
                entries.push(Entry {
                    lead: Vec::new(),
                    body: vec![line.to_owned()],
                    value: Map::new(),
                    expressions: false,
                });
            } else {
                entries.push(Entry {
                    lead,
                    body: vec![line.to_owned()],
                    value: Map::new(),
                    expressions: false,
                });
            }
        } else if line.trim().is_empty() || line.starts_with('#') {
            pending.push(line.to_owned());
        } else if line.starts_with(' ') || line.starts_with('\t') {
            let Some(entry) = entries.last_mut() else {
                return Err("entries must be a block list at the top level".to_owned());
            };
            entry.body.append(&mut pending);
            entry.body.push(line.to_owned());
        } else if entries.is_empty() && line.trim() == "[]" {
            header.append(&mut pending);
            header.push(line.to_owned());
        } else {
            return Err("entries must be a block list at the top level".to_owned());
        }
    }
    let trailer = if entries.is_empty() {
        header.append(&mut pending);
        Vec::new()
    } else {
        pending
    };
    for (index, entry) in entries.iter_mut().enumerate() {
        let text = entry.body.join("\n");
        let value: serde_yaml_ng::Value = serde_yaml_ng::from_str(&text)
            .map_err(|_| format!("entry {} could not be read on its own", index + 1))?;
        let json = serde_json::to_value(&value)
            .map_err(|error| format!("entry {} is not plain data: {error}", index + 1))?;
        let Value::Array(mut items) = json else {
            return Err(format!("entry {} is not a list item", index + 1));
        };
        if items.len() != 1 {
            return Err(format!("entry {} could not be read on its own", index + 1));
        }
        let Value::Object(map) = items.remove(0) else {
            return Err(format!("entry {} is not a mapping", index + 1));
        };
        entry.value = map;
        entry.expressions = entry.body.iter().any(|line| has_tag(line));
    }
    if entries.len() != expected {
        return Err("the file's entries could not be told apart".to_owned());
    }
    Ok(PatchLayout {
        header,
        entries,
        trailer,
    })
}

impl PatchLayout {
    fn render(&self) -> String {
        let mut lines: Vec<&str> = Vec::new();
        let empty = self.entries.is_empty();
        for line in &self.header {
            if !empty && line.trim() == "[]" {
                continue;
            }
            lines.push(line);
        }
        if empty && !self.header.iter().any(|line| line.trim() == "[]") {
            lines.push("[]");
        }
        for entry in &self.entries {
            lines.extend(entry.lead.iter().map(String::as_str));
            lines.extend(entry.body.iter().map(String::as_str));
        }
        lines.extend(self.trailer.iter().map(String::as_str));
        let mut text = lines.join("\n");
        text.push('\n');
        text
    }

    /// Every entry's data, in file order.
    pub(crate) fn values(&self) -> impl Iterator<Item = &Map<String, Value>> {
        self.entries.iter().map(|entry| &entry.value)
    }

    /// The value this layer gives `key` of row `id`: the last patch entry
    /// for `id` that sets it wins.
    pub(crate) fn patch_value(&self, id: &str, key: &str) -> Option<&Value> {
        self.entries
            .iter()
            .rev()
            .filter(|entry| entry.patch_id() == Some(id))
            .find_map(|entry| entry.value.get(key))
    }

    /// Sets (`Some`) or removes (`None`) `key` of row `id` in this layer.
    /// Setting edits the last patch entry for `id`, or appends one; removing
    /// clears the key from every patch entry for `id` and drops entries left
    /// with nothing but their id.
    pub(crate) fn set_patch_key(
        &mut self,
        id: &str,
        key: &str,
        value: Option<Value>,
    ) -> Result<(), AppError> {
        let what = format!("the `{id}` entry");
        match value {
            Some(value) => {
                if let Some(entry) = self
                    .entries
                    .iter_mut()
                    .rev()
                    .find(|entry| entry.patch_id() == Some(id))
                {
                    entry.editable(&what)?;
                    if entry.value.get(key) != Some(&value) {
                        entry.value.insert(key.to_owned(), value);
                        entry.rerender()?;
                    }
                    return Ok(());
                }
                let mut map = Map::new();
                map.insert("id".to_owned(), Value::String(id.to_owned()));
                map.insert(key.to_owned(), value);
                self.entries.push(Entry::rendered(map)?);
            }
            None => {
                for entry in &self.entries {
                    if entry.patch_id() == Some(id) && entry.value.contains_key(key) {
                        entry.editable(&what)?;
                    }
                }
                let mut index = 0;
                while index < self.entries.len() {
                    let entry = &mut self.entries[index];
                    if entry.patch_id() == Some(id) && entry.value.remove(key).is_some() {
                        let only_target =
                            entry.value.keys().all(|key| key == "id" || key == "name");
                        if only_target {
                            let lead = std::mem::take(&mut entry.lead);
                            self.entries.remove(index);
                            if let Some(next) = self.entries.get_mut(index) {
                                let mut merged = lead;
                                merged.append(&mut next.lead);
                                next.lead = merged;
                            }
                            continue;
                        }
                        entry.rerender()?;
                    }
                    index += 1;
                }
            }
        }
        Ok(())
    }

    /// Rows added by top-level `insert` entries, with whether TodeX can
    /// rewrite them.
    pub(crate) fn inserted_rows(&self) -> Vec<(&Map<String, Value>, bool)> {
        self.entries
            .iter()
            .filter_map(|entry| entry.inserted().map(|rows| (rows, !entry.expressions)))
            .flat_map(|(rows, editable)| {
                rows.iter()
                    .filter_map(Value::as_object)
                    .map(move |row| (row, editable))
            })
            .collect()
    }

    /// Replaces (`Some`) or removes (`None`) the inserted row `row_id`; a
    /// new row is appended as its own `insert` entry.
    pub(crate) fn set_inserted_row(
        &mut self,
        row_id: &str,
        row: Option<Map<String, Value>>,
    ) -> Result<(), AppError> {
        let position = self.entries.iter().enumerate().find_map(|(index, entry)| {
            entry.inserted().and_then(|rows| {
                rows.iter()
                    .position(|row| row.get("id").and_then(Value::as_str) == Some(row_id))
                    .map(|element| (index, element))
            })
        });
        let Some((index, element)) = position else {
            if let Some(row) = row {
                let mut map = Map::new();
                map.insert("insert".to_owned(), Value::Array(vec![Value::Object(row)]));
                self.entries.push(Entry::rendered(map)?);
            }
            return Ok(());
        };
        let entry = &mut self.entries[index];
        entry.editable(&format!("the `{row_id}` entry"))?;
        let rows = entry
            .value
            .get_mut("insert")
            .and_then(Value::as_array_mut)
            .expect("located inside an insert list");
        match row {
            Some(row) => rows[element] = Value::Object(row),
            None => {
                rows.remove(element);
            }
        }
        if rows.is_empty() {
            let lead = std::mem::take(&mut entry.lead);
            self.entries.remove(index);
            if let Some(next) = self.entries.get_mut(index) {
                let mut merged = lead;
                merged.append(&mut next.lead);
                next.lead = merged;
            }
        } else {
            entry.rerender()?;
        }
        Ok(())
    }
}

/// A patch file as read, with its revision.
#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct PatchFile {
    pub path: String,
    pub exists: bool,
    pub revision: String,
    pub text: String,
    /// Why entries cannot be edited one by one (the raw text still can).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub parse_error: Option<String>,
    #[serde(skip)]
    layout: Option<PatchLayout>,
}

impl PatchFile {
    pub(crate) fn read(path: &Path) -> Result<Self, AppError> {
        let label = "dsh patch";
        let (exists, text) = match read_bytes(path, label)? {
            Some(bytes) => (
                true,
                String::from_utf8(bytes).map_err(|_| {
                    AppError::InvalidRequest(format!(
                        "dsh patch file must be UTF-8: {}",
                        path.display()
                    ))
                })?,
            ),
            None => (false, String::new()),
        };
        let revision = if exists {
            revision(text.as_bytes())
        } else {
            MISSING_REVISION.to_owned()
        };
        let (layout, parse_error) = match layout(&text) {
            Ok(layout) => (Some(layout), None),
            Err(error) => (None, Some(error)),
        };
        Ok(Self {
            path: path.to_string_lossy().into_owned(),
            exists,
            revision,
            text,
            parse_error,
            layout,
        })
    }

    /// A patch held in memory, for readers that already have its text.
    pub(crate) fn from_text(text: &str) -> Self {
        let (layout, parse_error) = match layout(text) {
            Ok(layout) => (Some(layout), None),
            Err(error) => (None, Some(error)),
        };
        Self {
            path: String::new(),
            exists: true,
            revision: revision(text.as_bytes()),
            text: text.to_owned(),
            parse_error,
            layout,
        }
    }

    pub(crate) fn layout(&self) -> Option<&PatchLayout> {
        self.layout.as_ref()
    }
}

/// Edits the profile patch entry by entry under dsh's profile lock. A stale
/// `expected_revision` is a conflict; a file that is not a plain block list
/// must be edited as text. Bumps the managed-config revision when the file
/// changed.
pub(crate) fn edit_profile_patch(
    paths: &DshPaths,
    expected_revision: Option<&str>,
    edit: impl FnOnce(&mut PatchLayout) -> Result<(), AppError>,
) -> Result<PatchFile, AppError> {
    edit_patch_at(&paths.profile_patch(), expected_revision, edit)
}

/// [`edit_profile_patch`] for a profile patch at `path`; the profile's
/// manifest beside it carries the lock.
pub(crate) fn edit_patch_at(
    path: &Path,
    expected_revision: Option<&str>,
    edit: impl FnOnce(&mut PatchLayout) -> Result<(), AppError>,
) -> Result<PatchFile, AppError> {
    with_lock(&path.with_file_name("package.json"), || {
        let current = PatchFile::read(path)?;
        if expected_revision.is_some_and(|expected| expected != current.revision) {
            return Err(AppError::Conflict(
                "the dsh profile patch changed since it was read; reload and retry".to_owned(),
            ));
        }
        let mut layout = current.layout.clone().ok_or_else(|| {
            AppError::Conflict(format!(
                "{} cannot be edited entry by entry ({}); edit the YAML directly",
                path.display(),
                current.parse_error.as_deref().unwrap_or("unreadable")
            ))
        })?;
        edit(&mut layout)?;
        let text = layout.render();
        if current.exists && text == current.text {
            return Ok(current);
        }
        write_file(path, text.as_bytes())?;
        crate::agent_providers::bump_config_revision();
        PatchFile::read(path)
    })
}

/// Owner-only for a new file; an existing file keeps its mode.
fn write_file(path: &Path, bytes: &[u8]) -> Result<(), AppError> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    crate::secure_fs::replace_keeping_mode(path, bytes).map_err(AppError::Io)
}

/// What the home-level layer overrides: per row id, the keys it sets.
#[derive(Debug, Default)]
pub(crate) struct HomeLayer {
    patches: BTreeMap<String, BTreeSet<String>>,
}

impl HomeLayer {
    /// Reads `$DSH_HOME/cordis.patch.yml`; an unreadable file is treated as
    /// overriding nothing (dsh itself fails to start on it).
    pub(crate) fn read(paths: &DshPaths) -> Self {
        let mut layer = Self::default();
        let Ok(Some(text)) = files::read_text(&paths.home_patch(), "dsh home patch") else {
            return layer;
        };
        let Some(Value::Array(entries)) = serde_yaml_ng::from_str::<serde_yaml_ng::Value>(&text)
            .ok()
            .and_then(|value| serde_json::to_value(value).ok())
        else {
            return layer;
        };
        for entry in entries.iter().filter_map(Value::as_object) {
            if entry.contains_key("insert") {
                continue;
            }
            if let Some(id) = entry.get("id").and_then(Value::as_str) {
                layer
                    .patches
                    .entry(id.to_owned())
                    .or_default()
                    .extend(entry.keys().filter(|key| *key != "id").cloned());
            }
        }
        layer
    }

    pub(crate) fn overrides(&self, id: &str, key: &str) -> bool {
        self.patches.get(id).is_some_and(|keys| keys.contains(key))
    }

    /// A conflict when the home layer would override `key` of row `id`.
    pub(crate) fn ensure_not_overridden(
        &self,
        paths: &DshPaths,
        id: &str,
        key: &str,
    ) -> Result<(), AppError> {
        if self.overrides(id, key) {
            return Err(AppError::Conflict(format!(
                "`{id}` is set in {}, which overrides the acp profile; edit it there",
                paths.home_patch().display()
            )));
        }
        Ok(())
    }
}

// ---------- credentials ----------

pub(crate) fn valid_ref_name(name: &str) -> bool {
    let mut chars = name.chars();
    chars
        .next()
        .is_some_and(|first| first.is_ascii_alphabetic() || first == '_')
        && chars.all(|char| char.is_ascii_alphanumeric() || char == '_')
}

fn read_credentials(path: &Path) -> Result<serde_yaml_ng::Mapping, AppError> {
    let Some(text) = files::read_text(path, "dsh credentials")? else {
        return Ok(serde_yaml_ng::Mapping::new());
    };
    match serde_yaml_ng::from_str::<serde_yaml_ng::Value>(&text) {
        Ok(serde_yaml_ng::Value::Mapping(mapping)) => Ok(mapping),
        Ok(serde_yaml_ng::Value::Null) => Ok(serde_yaml_ng::Mapping::new()),
        Ok(_) | Err(_) => Err(AppError::Conflict(format!(
            "{} is not a valid dsh credentials file; fix or remove it first",
            path.display()
        ))),
    }
}

/// The saved value of reference `name`.
pub(crate) fn credential_ref(paths: &DshPaths, name: &str) -> Result<Option<String>, AppError> {
    let document = read_credentials(&paths.credentials())?;
    Ok(document
        .get("refs")
        .and_then(serde_yaml_ng::Value::as_mapping)
        .and_then(|refs| refs.get(name))
        .and_then(serde_yaml_ng::Value::as_str)
        .map(str::to_owned))
}

/// Sets (`Some`) or removes (`None`) references, keeping every other part of
/// the file (records, unknown keys). The file stays 0600.
pub(crate) fn set_credential_refs(
    paths: &DshPaths,
    changes: &[(&str, Option<&str>)],
) -> Result<(), AppError> {
    for (name, value) in changes {
        if !valid_ref_name(name) {
            return Err(AppError::InvalidRequest(format!(
                "invalid dsh credential reference name {name}"
            )));
        }
        if value.is_some_and(|value| value.is_empty()) {
            return Err(AppError::InvalidRequest(format!(
                "dsh credential {name} needs a value"
            )));
        }
    }
    let path = paths.credentials();
    with_lock(&path, || {
        let mut document = read_credentials(&path)?;
        let before = document.clone();
        let version = serde_yaml_ng::Value::from("version");
        if !document.contains_key(&version) {
            let mut fresh = serde_yaml_ng::Mapping::new();
            fresh.insert(version, serde_yaml_ng::Value::from(1));
            fresh.extend(document);
            document = fresh;
        }
        let refs_key = serde_yaml_ng::Value::from("refs");
        if !document
            .get(&refs_key)
            .is_some_and(serde_yaml_ng::Value::is_mapping)
        {
            document.insert(
                refs_key.clone(),
                serde_yaml_ng::Value::Mapping(Default::default()),
            );
        }
        let refs = document
            .get_mut(&refs_key)
            .and_then(serde_yaml_ng::Value::as_mapping_mut)
            .expect("refs normalized to a mapping");
        for (name, value) in changes {
            match value {
                Some(value) => {
                    refs.insert((*name).into(), (*value).into());
                }
                None => {
                    refs.remove(*name);
                }
            }
        }
        if document == before {
            return Ok(());
        }
        let text =
            serde_yaml_ng::to_string(&document).map_err(|error| AppError::Anyhow(error.into()))?;
        files::atomic_write_private(&path, text.as_bytes())
    })
}

// ---------- composed profile (`dsh --dump-config`) ----------

/// A row's `disabled` state in the composed tree.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase", tag = "kind", content = "expression")]
pub(crate) enum RowState {
    Enabled,
    Disabled,
    /// A `!!js` expression decided at startup.
    Conditional(String),
}

/// One row of the composed `acp` profile.
#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct ComposedRow {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    /// The plugin module.
    pub name: String,
    pub state: RowState,
    /// The layer that declared the row: a bundle package or a patch file.
    pub layer: String,
    /// Later layers that changed it.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub patched_by: Vec<String>,
    #[serde(skip)]
    pub config: Option<Value>,
}

/// Parses `dsh --dump-config` output: YAML rows grouped under
/// `# == <layer>[, patched by <layer>, …]` comments.
pub(crate) fn parse_dump(text: &str) -> Result<Vec<ComposedRow>, AppError> {
    let mut rows = Vec::new();
    let mut label: Option<String> = None;
    let mut chunk = String::new();
    let mut flush = |label: &Option<String>, chunk: &mut String| -> Result<(), AppError> {
        if chunk.trim().is_empty() {
            chunk.clear();
            return Ok(());
        }
        let parsed: Value = serde_yaml_ng::from_str(chunk).map_err(|error| {
            AppError::ProviderUnavailable(format!("dsh printed an unreadable profile: {error}"))
        })?;
        chunk.clear();
        let (layer, patched_by) = match label {
            Some(label) => match label.split_once(", patched by ") {
                Some((layer, patches)) => (
                    layer.to_owned(),
                    patches.split(", ").map(str::to_owned).collect(),
                ),
                None => (label.clone(), Vec::new()),
            },
            None => (String::new(), Vec::new()),
        };
        collect_rows(&parsed, &layer, &patched_by, &mut rows);
        Ok(())
    };
    for line in text.lines() {
        if let Some(next) = line.strip_prefix("# == ") {
            flush(&label, &mut chunk)?;
            label = Some(next.trim().to_owned());
        } else {
            chunk.push_str(line);
            chunk.push('\n');
        }
    }
    flush(&label, &mut chunk)?;
    Ok(rows)
}

fn collect_rows(value: &Value, layer: &str, patched_by: &[String], rows: &mut Vec<ComposedRow>) {
    for row in value.as_array().into_iter().flatten() {
        let Some(map) = row.as_object() else {
            continue;
        };
        let Some(name) = map.get("name").and_then(Value::as_str) else {
            continue;
        };
        let state = match map.get("disabled") {
            None | Some(Value::Null) | Some(Value::Bool(false)) => RowState::Enabled,
            Some(Value::Bool(true)) => RowState::Disabled,
            Some(other) => RowState::Conditional(match other {
                Value::String(text) => text.clone(),
                other => other.to_string(),
            }),
        };
        let group = map.get("group").and_then(Value::as_bool).unwrap_or(false);
        rows.push(ComposedRow {
            id: map.get("id").and_then(Value::as_str).map(str::to_owned),
            name: name.to_owned(),
            state,
            layer: layer.to_owned(),
            patched_by: patched_by.to_vec(),
            config: map.get("config").filter(|_| !group).cloned(),
        });
        if group {
            if let Some(children) = map.get("config") {
                collect_rows(children, layer, patched_by, rows);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn temp_paths() -> (PathBuf, DshPaths) {
        let root = std::env::temp_dir().join(format!("todex-dsh-config-{}", uuid::Uuid::new_v4()));
        fs::create_dir_all(&root).unwrap();
        let paths = DshPaths { home: root.clone() };
        (root, paths)
    }

    const USER_PATCH: &str = "\
# my notes
- id: sandbox-policy
  config:
    mode: !!js process.env.X ?? 'workspace-write'

# web tweaks
- id: web
  config:
    searchProvider: deepseek-official
- insert:
    - id: mcp-mine
      name: '@deepseek-ai/dsh-mcp-client'
      config: { serverName: mine, transport: stdio, command: /bin/true }
# trailing note
";

    #[test]
    fn untouched_entries_keep_their_text_and_expressions() {
        let mut layout = layout(USER_PATCH).unwrap();
        layout
            .set_patch_key(
                "acp",
                "config",
                Some(json!({"provider": "gw", "model": "m"})),
            )
            .unwrap();
        let text = layout.render();
        assert!(text.starts_with("# my notes\n- id: sandbox-policy\n  config:\n    mode: !!js process.env.X ?? 'workspace-write'\n\n# web tweaks\n- id: web\n"));
        assert!(text.contains("- id: acp\n  config:\n    provider: gw\n    model: m\n"));
        assert!(text.contains("# trailing note"));
        let reread = super::layout(&text).unwrap();
        assert_eq!(
            reread.patch_value("acp", "config"),
            Some(&json!({"provider": "gw", "model": "m"}))
        );
        // An entry with a tag cannot be rewritten.
        assert!(matches!(
            layout.set_patch_key("sandbox-policy", "disabled", Some(json!(true))),
            Err(AppError::Conflict(_))
        ));
    }

    #[test]
    fn patch_keys_edit_in_place_and_entries_disappear_when_empty() {
        let mut layout = layout(USER_PATCH).unwrap();
        layout
            .set_patch_key("web", "disabled", Some(json!(true)))
            .unwrap();
        assert_eq!(layout.patch_value("web", "disabled"), Some(&json!(true)));
        assert_eq!(layout.entries.len(), 3);
        layout.set_patch_key("web", "config", None).unwrap();
        layout.set_patch_key("web", "disabled", None).unwrap();
        assert_eq!(layout.entries.len(), 2);
        // Its lead comment moves to the next entry instead of vanishing.
        assert!(layout.render().contains("# web tweaks\n- insert:"));
    }

    #[test]
    fn inserted_rows_are_added_replaced_and_removed() {
        let mut layout = layout("[]\n").unwrap();
        assert!(layout.inserted_rows().is_empty());
        let row = |command: &str| {
            json!({"id": "todex-mcp-a", "name": "@deepseek-ai/dsh-mcp-client",
                   "config": {"serverName": "a", "transport": "stdio", "command": command}})
            .as_object()
            .unwrap()
            .clone()
        };
        layout
            .set_inserted_row("todex-mcp-a", Some(row("/bin/a")))
            .unwrap();
        let text = layout.render();
        assert!(!text.contains("[]"));
        let mut layout = super::layout(&text).unwrap();
        assert_eq!(layout.inserted_rows().len(), 1);
        layout
            .set_inserted_row("todex-mcp-a", Some(row("/bin/b")))
            .unwrap();
        assert_eq!(
            layout.inserted_rows()[0].0["config"]["command"],
            json!("/bin/b")
        );
        layout.set_inserted_row("todex-mcp-a", None).unwrap();
        assert_eq!(layout.render(), "[]\n");
    }

    #[test]
    fn flow_lists_and_mappings_are_text_only() {
        assert!(layout("[{id: web, disabled: true}]\n").is_err());
        assert!(layout("id: web\n").is_err());
        assert!(layout("").unwrap().entries.is_empty());
    }

    #[test]
    fn edits_check_the_revision_and_take_the_profile_lock() {
        let (root, paths) = temp_paths();
        let first = edit_profile_patch(&paths, Some(MISSING_REVISION), |layout| {
            layout.set_patch_key("web", "disabled", Some(json!(true)))
        })
        .unwrap();
        assert!(first.exists);
        assert!(matches!(
            edit_profile_patch(&paths, Some(MISSING_REVISION), |_| Ok(())),
            Err(AppError::Conflict(_))
        ));
        // A lock left by an exited process is taken over (only Unix can tell
        // that its holder exited; see `holder_exited`).
        #[cfg(unix)]
        fs::write(root.join("profiles/acp/package.json.lock"), "999999999\n").unwrap();
        let second = edit_profile_patch(&paths, Some(&first.revision), |layout| {
            layout.set_patch_key("web", "disabled", None)
        })
        .unwrap();
        assert_eq!(second.text, "[]\n");
        assert!(!root.join("profiles/acp/package.json.lock").exists());
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn credential_refs_keep_records_and_stay_private() {
        let (root, paths) = temp_paths();
        fs::write(
            paths.credentials(),
            "version: 1\nrefs:\n  OTHER_KEY: keep\nrecords:\n  llm-pi-ai/x:\n    kind: api-key\nfuture: true\n",
        )
        .unwrap();
        set_credential_refs(&paths, &[("TODEX_DSH_GW_API_KEY", Some("sk-1"))]).unwrap();
        assert_eq!(
            credential_ref(&paths, "TODEX_DSH_GW_API_KEY")
                .unwrap()
                .as_deref(),
            Some("sk-1")
        );
        let text = fs::read_to_string(paths.credentials()).unwrap();
        assert!(text.contains("OTHER_KEY: keep"));
        assert!(text.contains("llm-pi-ai/x"));
        assert!(text.contains("future: true"));
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = fs::metadata(paths.credentials())
                .unwrap()
                .permissions()
                .mode();
            assert_eq!(mode & 0o777, 0o600);
        }
        set_credential_refs(&paths, &[("TODEX_DSH_GW_API_KEY", None)]).unwrap();
        assert_eq!(
            credential_ref(&paths, "TODEX_DSH_GW_API_KEY").unwrap(),
            None
        );
        assert!(set_credential_refs(&paths, &[("bad-name", Some("x"))]).is_err());
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn a_fresh_credentials_file_starts_with_its_version() {
        let (root, paths) = temp_paths();
        set_credential_refs(&paths, &[("DEEPSEEK_API_KEY", Some("sk-2"))]).unwrap();
        assert_eq!(
            fs::read_to_string(paths.credentials()).unwrap(),
            "version: 1\nrefs:\n  DEEPSEEK_API_KEY: sk-2\n"
        );
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn home_layer_overrides_are_reported() {
        let (root, paths) = temp_paths();
        fs::write(
            paths.home_patch(),
            "- id: llm-pi-ai\n  config: {}\n- insert:\n    - id: x\n      name: y\n",
        )
        .unwrap();
        let home = HomeLayer::read(&paths);
        assert!(home.overrides("llm-pi-ai", "config"));
        assert!(!home.overrides("llm-pi-ai", "disabled"));
        assert!(!home.overrides("x", "config"));
        assert!(home
            .ensure_not_overridden(&paths, "llm-pi-ai", "config")
            .is_err());
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn dump_rows_carry_their_layers_and_states() {
        let dump = "\
# == @deepseek-ai/dsh-base
- id: plugin-manager
  name: '@deepseek-ai/dsh-plugin-manager'
  disabled: !!js '!ctx.get(''profileContext'')'
- id: web
  name: '@deepseek-ai/dsh-web'
  config:
    searchProvider: deepseek-official
    fetchProvider: http
# == @deepseek-ai/dsh-base, patched by @deepseek-ai/dsh-acp-app, /tmp/todex.patch.yml
- id: hmr
  name: '@deepseek-ai/dsh-hmr'
  disabled: true
# == /home/u/.dsh/profiles/acp/cordis.patch.yml
- id: mcp-mine
  name: '@deepseek-ai/dsh-mcp-client'
";
        let rows = parse_dump(dump).unwrap();
        assert_eq!(rows.len(), 4);
        assert_eq!(
            rows[0].state,
            RowState::Conditional("!ctx.get('profileContext')".to_owned())
        );
        assert_eq!(rows[1].layer, "@deepseek-ai/dsh-base");
        assert_eq!(rows[1].config.as_ref().unwrap()["fetchProvider"], "http");
        assert_eq!(rows[2].state, RowState::Disabled);
        assert_eq!(
            rows[2].patched_by,
            ["@deepseek-ai/dsh-acp-app", "/tmp/todex.patch.yml"]
        );
        assert_eq!(rows[3].layer, "/home/u/.dsh/profiles/acp/cordis.patch.yml");
        assert_eq!(rows[3].state, RowState::Enabled);
    }
}
