//! Global instruction files of the CLIs, edited from the CLI manager:
//! Claude Code's `<claude_dir>/CLAUDE.md`, Codex's `<codex_home>/AGENTS.md`,
//! Pi's `<pi_dir>/AGENTS.md` and DeepSeek Harness's `$DSH_HOME/AGENTS.md`,
//! plus a library of instruction presets kept
//! in `<data_dir>/agent-instructions.json`.
//!
//! A CLI either keeps its own text or is bound to a preset. A bound file is
//! rewritten whenever its preset changes. The store remembers the revision
//! it last wrote, so a file edited meanwhile (by the CLI or by hand) reports
//! `drifted` and is left alone until the user overwrites it or keeps it as
//! its own text. Writes to a file name the revision they were based on, like
//! [`crate::cli_settings`].

use std::{
    collections::BTreeMap,
    fs, io,
    path::{Path, PathBuf},
    sync::Mutex,
};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::{agent_providers::AgentDirs, error::AppError};

/// Largest instruction file or preset text.
pub(crate) const MAX_INSTRUCTIONS_BYTES: usize = 1024 * 1024;
/// Most presets the library holds.
pub(crate) const MAX_PRESETS: usize = 50;
/// Longest preset name, in characters.
const MAX_NAME_CHARS: usize = 80;
const STORE_FILE: &str = "agent-instructions.json";
const STORE_VERSION: u32 = 1;
/// The revision of a file that does not exist yet.
const MISSING_REVISION: &str = "missing";

/// Serializes read-check-write cycles over the store and the files.
static LOCK: Mutex<()> = Mutex::new(());

/// A CLI whose global instruction file the CLI manager edits.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum InstructionsTarget {
    ClaudeCode,
    Codex,
    Pi,
    DeepseekHarness,
}

impl InstructionsTarget {
    const ALL: [Self; 4] = [
        Self::ClaudeCode,
        Self::Codex,
        Self::Pi,
        Self::DeepseekHarness,
    ];

    pub(crate) fn from_provider(provider: &str) -> Option<Self> {
        Self::ALL
            .into_iter()
            .find(|target| target.provider() == provider)
    }

    pub(crate) fn provider(self) -> &'static str {
        match self {
            Self::ClaudeCode => "claude-code",
            Self::Codex => "codex",
            Self::Pi => "pi",
            Self::DeepseekHarness => "deepseek-harness",
        }
    }

    pub(crate) fn file(self) -> InstructionsFile {
        let dirs = AgentDirs::detect();
        match self {
            Self::ClaudeCode => InstructionsFile {
                target: self,
                path: dirs.claude_dir().join("CLAUDE.md"),
                override_path: None,
            },
            // Codex reads `AGENTS.override.md` instead when it exists.
            Self::Codex => InstructionsFile {
                target: self,
                path: dirs.codex_home().join("AGENTS.md"),
                override_path: Some(dirs.codex_home().join("AGENTS.override.md")),
            },
            Self::Pi => InstructionsFile {
                target: self,
                path: dirs.pi_dir().join("AGENTS.md"),
                override_path: None,
            },
            // Read before `$DSH_AGENTS_HOME/AGENTS.md` and the project's files.
            Self::DeepseekHarness => InstructionsFile {
                target: self,
                path: crate::dsh_config::DshPaths::of(&dirs).instructions(),
                override_path: None,
            },
        }
    }
}

/// An instruction file and the file that would shadow it, if any.
#[derive(Clone, Debug)]
pub(crate) struct InstructionsFile {
    pub target: InstructionsTarget,
    pub path: PathBuf,
    pub override_path: Option<PathBuf>,
}

/// The preset library, in `<data_dir>/agent-instructions.json`.
#[derive(Clone, Debug)]
pub(crate) struct InstructionsStore {
    path: PathBuf,
}

impl InstructionsStore {
    pub(crate) fn new(data_dir: &Path) -> Self {
        Self {
            path: data_dir.join(STORE_FILE),
        }
    }
}

#[derive(Debug, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct StoreData {
    version: u32,
    presets: Vec<StoredPreset>,
    /// Provider id → the preset its file follows.
    bindings: BTreeMap<String, Binding>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct StoredPreset {
    id: String,
    name: String,
    text: String,
    /// Bumped on every edit; updates name the revision they were based on.
    revision: u64,
    updated_at: u64,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Binding {
    preset_id: String,
    /// The file's revision right after the preset was last written to it.
    written_revision: String,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct InstructionsDocument {
    pub path: String,
    pub exists: bool,
    pub revision: String,
    /// The file as stored; empty when it does not exist.
    pub text: String,
    /// The preset the file follows; none when it keeps its own text.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub preset_id: Option<String>,
    /// The file was edited after the preset was last written to it.
    pub drifted: bool,
    /// An existing file the CLI reads instead of this one.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub override_path: Option<String>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct InstructionPreset {
    pub id: String,
    pub name: String,
    pub text: String,
    pub revision: u64,
    pub updated_at: u64,
    /// Providers whose file follows this preset.
    pub used_by: Vec<String>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct InstructionPresetList {
    pub presets: Vec<InstructionPreset>,
}

/// What a preset edit did to a file that follows it.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct PresetSyncResult {
    pub provider: String,
    /// `updated`, `drifted` (edited meanwhile, left alone) or `failed`.
    pub status: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct PresetUpdate {
    pub preset: InstructionPreset,
    pub sync: Vec<PresetSyncResult>,
}

pub(crate) fn read(
    store: &InstructionsStore,
    file: &InstructionsFile,
) -> Result<InstructionsDocument, AppError> {
    let _guard = lock();
    let data = load(store)?;
    document(&data, file)
}

/// Replaces the file with `text`; the file stops following its preset.
pub(crate) fn write_text(
    store: &InstructionsStore,
    file: &InstructionsFile,
    expected_revision: &str,
    text: &str,
) -> Result<InstructionsDocument, AppError> {
    check_text_size(text)?;
    let _guard = lock();
    let mut data = load(store)?;
    checked(file, expected_revision)?;
    write_file(&file.path, text)?;
    if data.bindings.remove(file.target.provider()).is_some() {
        save(store, &data)?;
    }
    document(&data, file)
}

/// Makes the file follow `preset_id`, writing the preset's text to it (again,
/// when it already follows it), or keep its current text when `None`.
pub(crate) fn set_source(
    store: &InstructionsStore,
    file: &InstructionsFile,
    expected_revision: &str,
    preset_id: Option<&str>,
) -> Result<InstructionsDocument, AppError> {
    let _guard = lock();
    let mut data = load(store)?;
    checked(file, expected_revision)?;
    let provider = file.target.provider().to_owned();
    match preset_id {
        Some(id) => {
            let preset = data
                .presets
                .iter()
                .find(|preset| preset.id == id)
                .ok_or_else(|| AppError::NotFound(format!("instruction preset {id}")))?;
            write_file(&file.path, &preset.text)?;
            let binding = Binding {
                preset_id: preset.id.clone(),
                written_revision: file_revision(&file.path)?,
            };
            data.bindings.insert(provider, binding);
        }
        None => {
            data.bindings.remove(&provider);
        }
    }
    save(store, &data)?;
    document(&data, file)
}

pub(crate) fn list_presets(store: &InstructionsStore) -> Result<InstructionPresetList, AppError> {
    let _guard = lock();
    let data = load(store)?;
    Ok(InstructionPresetList {
        presets: data
            .presets
            .iter()
            .map(|preset| public_preset(&data, preset))
            .collect(),
    })
}

pub(crate) fn create_preset(
    store: &InstructionsStore,
    name: &str,
    text: &str,
) -> Result<InstructionPreset, AppError> {
    let name = checked_name(name)?;
    check_text_size(text)?;
    let _guard = lock();
    let mut data = load(store)?;
    if data.presets.len() >= MAX_PRESETS {
        return Err(AppError::InvalidRequest(format!(
            "at most {MAX_PRESETS} instruction presets are allowed"
        )));
    }
    let preset = StoredPreset {
        id: uuid::Uuid::new_v4().to_string(),
        name,
        text: text.to_owned(),
        revision: 1,
        updated_at: crate::agent_providers::now_millis(),
    };
    data.presets.push(preset.clone());
    save(store, &data)?;
    Ok(public_preset(&data, &preset))
}

/// Edits a preset and rewrites the files that follow it, except those
/// edited since the preset was last written to them.
pub(crate) fn update_preset(
    store: &InstructionsStore,
    id: &str,
    expected_revision: u64,
    name: &str,
    text: &str,
) -> Result<PresetUpdate, AppError> {
    update_preset_files(store, id, expected_revision, name, text, |target| {
        target.file()
    })
}

fn update_preset_files(
    store: &InstructionsStore,
    id: &str,
    expected_revision: u64,
    name: &str,
    text: &str,
    file_of: impl Fn(InstructionsTarget) -> InstructionsFile,
) -> Result<PresetUpdate, AppError> {
    let name = checked_name(name)?;
    check_text_size(text)?;
    let _guard = lock();
    let mut data = load(store)?;
    let preset = data
        .presets
        .iter_mut()
        .find(|preset| preset.id == id)
        .ok_or_else(|| AppError::NotFound(format!("instruction preset {id}")))?;
    if preset.revision != expected_revision {
        return Err(AppError::Conflict(
            "the preset changed meanwhile; reload and try again".to_owned(),
        ));
    }
    let text_changed = preset.text != text;
    preset.name = name;
    preset.text = text.to_owned();
    preset.revision += 1;
    preset.updated_at = crate::agent_providers::now_millis();
    let preset = preset.clone();

    let mut sync = Vec::new();
    if text_changed {
        for target in InstructionsTarget::ALL {
            let provider = target.provider();
            let Some(binding) = data.bindings.get_mut(provider) else {
                continue;
            };
            if binding.preset_id != preset.id {
                continue;
            }
            let file = file_of(target);
            let result = file_revision(&file.path).and_then(|current| {
                if current != binding.written_revision {
                    return Ok("drifted");
                }
                write_file(&file.path, &preset.text)?;
                binding.written_revision = file_revision(&file.path)?;
                Ok("updated")
            });
            sync.push(match result {
                Ok(status) => PresetSyncResult {
                    provider: provider.to_owned(),
                    status,
                    message: None,
                },
                Err(error) => PresetSyncResult {
                    provider: provider.to_owned(),
                    status: "failed",
                    message: Some(error.to_string()),
                },
            });
        }
    }
    save(store, &data)?;
    Ok(PresetUpdate {
        preset: public_preset(&data, &preset),
        sync,
    })
}

/// Deletes a preset; files that followed it keep their text as their own.
pub(crate) fn delete_preset(store: &InstructionsStore, id: &str) -> Result<(), AppError> {
    let _guard = lock();
    let mut data = load(store)?;
    let before = data.presets.len();
    data.presets.retain(|preset| preset.id != id);
    if data.presets.len() == before {
        return Err(AppError::NotFound(format!("instruction preset {id}")));
    }
    data.bindings.retain(|_, binding| binding.preset_id != id);
    save(store, &data)
}

fn lock() -> std::sync::MutexGuard<'static, ()> {
    LOCK.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

fn document(data: &StoreData, file: &InstructionsFile) -> Result<InstructionsDocument, AppError> {
    let display = file.path.display().to_string();
    let (exists, revision, text) = match read_file(&file.path)? {
        Some(text) => (true, revision(text.as_bytes()), text),
        None => (false, MISSING_REVISION.to_owned(), String::new()),
    };
    let binding = data.bindings.get(file.target.provider());
    let override_path = file
        .override_path
        .as_ref()
        .filter(|path| path.is_file())
        .map(|path| path.display().to_string());
    Ok(InstructionsDocument {
        path: display,
        exists,
        drifted: binding.is_some_and(|binding| binding.written_revision != revision),
        revision,
        text,
        preset_id: binding.map(|binding| binding.preset_id.clone()),
        override_path,
    })
}

fn public_preset(data: &StoreData, preset: &StoredPreset) -> InstructionPreset {
    InstructionPreset {
        id: preset.id.clone(),
        name: preset.name.clone(),
        text: preset.text.clone(),
        revision: preset.revision,
        updated_at: preset.updated_at,
        used_by: data
            .bindings
            .iter()
            .filter(|(_, binding)| binding.preset_id == preset.id)
            .map(|(provider, _)| provider.clone())
            .collect(),
    }
}

fn checked(file: &InstructionsFile, expected_revision: &str) -> Result<(), AppError> {
    if file_revision(&file.path)? != expected_revision {
        return Err(AppError::Conflict(format!(
            "{} changed on disk; reload and try again",
            file.path.display()
        )));
    }
    Ok(())
}

fn checked_name(name: &str) -> Result<String, AppError> {
    let name = name.trim();
    if name.is_empty() || name.chars().count() > MAX_NAME_CHARS {
        return Err(AppError::InvalidRequest(format!(
            "a preset name must be 1–{MAX_NAME_CHARS} characters"
        )));
    }
    Ok(name.to_owned())
}

fn check_text_size(text: &str) -> Result<(), AppError> {
    if text.len() > MAX_INSTRUCTIONS_BYTES {
        return Err(AppError::InvalidRequest(
            "instructions exceed the 1 MiB limit".to_owned(),
        ));
    }
    Ok(())
}

fn read_file(path: &Path) -> Result<Option<String>, AppError> {
    let bytes = match fs::read(path) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error.into()),
    };
    if bytes.len() > MAX_INSTRUCTIONS_BYTES {
        return Err(AppError::InvalidRequest(format!(
            "{} exceeds the 1 MiB limit",
            path.display()
        )));
    }
    String::from_utf8(bytes)
        .map(Some)
        .map_err(|_| AppError::InvalidRequest(format!("{} is not UTF-8", path.display())))
}

fn file_revision(path: &Path) -> Result<String, AppError> {
    Ok(match read_file(path)? {
        Some(text) => revision(text.as_bytes()),
        None => MISSING_REVISION.to_owned(),
    })
}

fn write_file(path: &Path, text: &str) -> Result<(), AppError> {
    crate::secure_fs::replace_keeping_mode(path, text.as_bytes())?;
    Ok(())
}

fn revision(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

fn load(store: &InstructionsStore) -> Result<StoreData, AppError> {
    let bytes = match fs::read(&store.path) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            return Ok(StoreData {
                version: STORE_VERSION,
                ..StoreData::default()
            });
        }
        Err(error) => return Err(error.into()),
    };
    let data: StoreData = serde_json::from_slice(&bytes).map_err(|error| {
        AppError::Conflict(format!("{} is not valid: {error}", store.path.display()))
    })?;
    if data.version != STORE_VERSION {
        return Err(AppError::Conflict(format!(
            "{} has unsupported version {}",
            store.path.display(),
            data.version
        )));
    }
    Ok(data)
}

fn save(store: &InstructionsStore, data: &StoreData) -> Result<(), AppError> {
    let mut bytes = serde_json::to_vec_pretty(data)?;
    bytes.push(b'\n');
    crate::secure_fs::write_owner_only_atomic(&store.path, &bytes)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir() -> PathBuf {
        let dir = std::env::temp_dir().join(format!("todex-instructions-{}", uuid::Uuid::new_v4()));
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn file_in(dir: &Path, target: InstructionsTarget) -> InstructionsFile {
        InstructionsFile {
            target,
            path: dir.join(target.provider()).join("AGENTS.md"),
            override_path: Some(dir.join(target.provider()).join("AGENTS.override.md")),
        }
    }

    #[test]
    fn writes_own_text_and_rejects_stale_revisions() {
        let dir = temp_dir();
        let store = InstructionsStore::new(&dir);
        let file = file_in(&dir, InstructionsTarget::Codex);
        let missing = read(&store, &file).unwrap();
        assert!(!missing.exists);
        assert_eq!(missing.revision, MISSING_REVISION);

        let saved = write_text(&store, &file, &missing.revision, "# Rules\n").unwrap();
        assert!(saved.exists);
        assert_eq!(fs::read_to_string(&file.path).unwrap(), "# Rules\n");

        let stale = write_text(&store, &file, &missing.revision, "other");
        assert!(matches!(stale, Err(AppError::Conflict(_))));
    }

    #[test]
    fn bound_files_follow_preset_edits_unless_edited_meanwhile() {
        let dir = temp_dir();
        let store = InstructionsStore::new(&dir);
        let codex = file_in(&dir, InstructionsTarget::Codex);
        let pi = file_in(&dir, InstructionsTarget::Pi);
        let preset = create_preset(&store, " Shared ", "v1").unwrap();
        assert_eq!(preset.name, "Shared");

        for file in [&codex, &pi] {
            let current = read(&store, file).unwrap();
            let bound = set_source(&store, file, &current.revision, Some(&preset.id)).unwrap();
            assert_eq!(bound.preset_id.as_deref(), Some(preset.id.as_str()));
            assert!(!bound.drifted);
        }
        fs::write(&pi.path, "edited by hand").unwrap();
        assert!(read(&store, &pi).unwrap().drifted);

        let files = |target| file_in(&dir, target);
        let update =
            update_preset_files(&store, &preset.id, preset.revision, "Shared", "v2", files)
                .unwrap();
        assert_eq!(update.preset.used_by, ["codex", "pi"]);
        let statuses: Vec<_> = update
            .sync
            .iter()
            .map(|result| (result.provider.as_str(), result.status))
            .collect();
        assert_eq!(statuses, [("codex", "updated"), ("pi", "drifted")]);
        assert_eq!(fs::read_to_string(&codex.path).unwrap(), "v2");
        assert_eq!(fs::read_to_string(&pi.path).unwrap(), "edited by hand");

        // Overwriting with the preset again clears the drift.
        let current = read(&store, &pi).unwrap();
        let rebound = set_source(&store, &pi, &current.revision, Some(&preset.id)).unwrap();
        assert!(!rebound.drifted);
        assert_eq!(rebound.text, "v2");

        let stale = update_preset_files(&store, &preset.id, preset.revision, "x", "v3", files);
        assert!(matches!(stale, Err(AppError::Conflict(_))));
    }

    #[test]
    fn deleting_a_preset_keeps_the_text_of_files_that_followed_it() {
        let dir = temp_dir();
        let store = InstructionsStore::new(&dir);
        let file = file_in(&dir, InstructionsTarget::ClaudeCode);
        let preset = create_preset(&store, "Claude", "keep me").unwrap();
        set_source(&store, &file, MISSING_REVISION, Some(&preset.id)).unwrap();

        delete_preset(&store, &preset.id).unwrap();
        let after = read(&store, &file).unwrap();
        assert_eq!(after.preset_id, None);
        assert_eq!(after.text, "keep me");
        assert!(list_presets(&store).unwrap().presets.is_empty());
        assert!(matches!(
            delete_preset(&store, &preset.id),
            Err(AppError::NotFound(_))
        ));
    }

    #[test]
    fn own_text_unbinds_and_override_is_reported() {
        let dir = temp_dir();
        let store = InstructionsStore::new(&dir);
        let file = file_in(&dir, InstructionsTarget::Codex);
        let preset = create_preset(&store, "P", "preset").unwrap();
        let bound = set_source(&store, &file, MISSING_REVISION, Some(&preset.id)).unwrap();
        assert_eq!(bound.override_path, None);

        let own = write_text(&store, &file, &bound.revision, "mine").unwrap();
        assert_eq!(own.preset_id, None);
        assert!(list_presets(&store).unwrap().presets[0].used_by.is_empty());

        fs::write(file.override_path.as_ref().unwrap(), "override").unwrap();
        assert!(read(&store, &file).unwrap().override_path.is_some());
    }

    #[test]
    fn validates_names_sizes_and_preset_count() {
        let dir = temp_dir();
        let store = InstructionsStore::new(&dir);
        assert!(matches!(
            create_preset(&store, "  ", "x"),
            Err(AppError::InvalidRequest(_))
        ));
        let big = "x".repeat(MAX_INSTRUCTIONS_BYTES + 1);
        assert!(matches!(
            create_preset(&store, "big", &big),
            Err(AppError::InvalidRequest(_))
        ));
        for index in 0..MAX_PRESETS {
            create_preset(&store, &format!("p{index}"), "").unwrap();
        }
        assert!(matches!(
            create_preset(&store, "one more", ""),
            Err(AppError::InvalidRequest(_))
        ));
    }
}
