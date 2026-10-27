//! Custom agent types of the `@tintinweb/pi-subagents` Pi extension, for the
//! subagent template list on its plugin settings page.
//!
//! The extension reads `<pi_dir>/agents/*.md` (global) besides two project
//! directories this module leaves alone. A file is optional YAML front matter
//! plus a Markdown body, the system prompt; its type is the front matter
//! `name:`, else the file name. A file whose type equals one of the
//! extension's built-in agents replaces it, and one holding nothing but
//! `enabled: false` is how a built-in is switched off, so those stubs are
//! reported as the built-in's state rather than as templates.
//!
//! Templates are addressed by file stem. Writes either replace the text
//! (kept byte for byte) or apply a form patch to the front matter, which
//! re-serializes it: key order and unknown keys stay, YAML comments do not.
//! Every update names the revision it was based on. The extension rescans
//! the directory on each `Agent` call, so changes apply to running Pi too.

use std::{
    fs, io,
    path::{Path, PathBuf},
    sync::Mutex,
};

use serde::Serialize;
use serde_json::{Map, Value};
use serde_yaml_ng::{Mapping, Value as Yaml};

use crate::{agent_providers::AgentDirs, cli_settings, error::AppError};

/// The extension's built-in agent types (`src/default-agents.ts`); keep in
/// step with it.
pub(crate) const BUILT_IN_AGENTS: [&str; 3] = ["general-purpose", "Explore", "Plan"];

/// Largest template read or written.
const MAX_TEMPLATE_BYTES: usize = 256 * 1024;
const MAX_NAME_CHARS: usize = 100;

/// Front matter keys in the order a new file lists them; keys not named
/// here follow in alphabetical order.
const KEY_ORDER: [&str; 13] = [
    "name",
    "display_name",
    "description",
    "color",
    "tools",
    "model",
    "thinking",
    "max_turns",
    "prompt_mode",
    "run_in_background",
    "isolation",
    "enabled",
    "extensions",
];

/// Serializes this module's read-check-write cycles within the daemon.
static WRITE_LOCK: Mutex<()> = Mutex::new(());

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct TemplateList {
    /// The directory templates live in.
    pub dir: String,
    /// Sorted by file name, case-insensitively.
    pub templates: Vec<TemplateSummary>,
    pub built_ins: Vec<BuiltInAgent>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct TemplateSummary {
    /// File stem: `<file>.md`.
    pub file: String,
    /// The agent type: front matter `name:`, else the file stem.
    pub name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub display_name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub color: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    pub enabled: bool,
    /// Its type replaces a built-in agent.
    pub overrides_built_in: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub parse_error: Option<String>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub(crate) enum BuiltInState {
    Enabled,
    /// Switched off by an `enabled: false` stub.
    Disabled,
    /// Replaced by a template of the same type.
    Overridden,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct BuiltInAgent {
    pub name: &'static str,
    pub state: BuiltInState,
    /// The stub or overriding template.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub file: Option<String>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct TemplateDocument {
    pub file: String,
    pub path: String,
    pub revision: String,
    /// The file as stored.
    pub content: String,
    /// Parsed front matter; empty when the file has none, `None` when it
    /// does not parse.
    pub front_matter: Option<Map<String, Value>>,
    /// The system prompt, trimmed as the extension reads it.
    pub body: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub parse_error: Option<String>,
}

/// New template text: the raw file, or front matter and body from the form.
#[derive(Debug)]
pub(crate) enum TemplateInput {
    Content {
        content: String,
    },
    Form {
        front_matter: Map<String, Value>,
        body: String,
    },
}

pub(crate) fn templates_dir(dirs: &AgentDirs) -> PathBuf {
    dirs.pi_dir().join("agents")
}

pub(crate) fn list(dirs: &AgentDirs) -> Result<TemplateList, AppError> {
    let dir = templates_dir(dirs);
    let mut templates = Vec::new();
    let mut stubs = Vec::new();
    let entries = match fs::read_dir(&dir) {
        Ok(entries) => Some(entries),
        Err(error) if error.kind() == io::ErrorKind::NotFound => None,
        Err(error) => return Err(error.into()),
    };
    for entry in entries.into_iter().flatten() {
        let path = entry?.path();
        let Some(file) = template_stem(&path) else {
            continue;
        };
        if !fs::metadata(&path).is_ok_and(|metadata| metadata.is_file()) {
            continue;
        }
        let parsed = fs::read(&path)
            .map_err(|error| error.to_string())
            .and_then(|bytes| {
                if bytes.len() > MAX_TEMPLATE_BYTES {
                    return Err("the file exceeds the 256 KiB limit".to_owned());
                }
                String::from_utf8(bytes).map_err(|_| "the file is not UTF-8".to_owned())
            })
            .and_then(|text| parse(&text));
        let summary = match parsed {
            Ok(parsed) if is_disable_stub(&file, &parsed) => {
                stubs.push(file);
                continue;
            }
            Ok(parsed) => summarize(file, &parsed),
            Err(parse_error) => TemplateSummary {
                name: file.clone(),
                overrides_built_in: BUILT_IN_AGENTS.contains(&file.as_str()),
                file,
                display_name: None,
                description: None,
                color: None,
                model: None,
                enabled: true,
                parse_error: Some(parse_error),
            },
        };
        templates.push(summary);
    }
    templates.sort_by_cached_key(|template| (template.file.to_lowercase(), template.file.clone()));
    let built_ins = BUILT_IN_AGENTS
        .iter()
        .map(|&name| {
            let overriding = templates
                .iter()
                .find(|template| template.parse_error.is_none() && template.name == name);
            match (overriding, stubs.iter().find(|stub| stub.as_str() == name)) {
                (Some(template), _) => BuiltInAgent {
                    name,
                    state: BuiltInState::Overridden,
                    file: Some(template.file.clone()),
                },
                (None, Some(stub)) => BuiltInAgent {
                    name,
                    state: BuiltInState::Disabled,
                    file: Some(stub.clone()),
                },
                (None, None) => BuiltInAgent {
                    name,
                    state: BuiltInState::Enabled,
                    file: None,
                },
            }
        })
        .collect();
    Ok(TemplateList {
        dir: dir.display().to_string(),
        templates,
        built_ins,
    })
}

pub(crate) fn read(dirs: &AgentDirs, file: &str) -> Result<TemplateDocument, AppError> {
    validate_file(file)?;
    read_existing(&template_path(dirs, file), file)
}

pub(crate) fn create(
    dirs: &AgentDirs,
    file: &str,
    input: TemplateInput,
) -> Result<TemplateDocument, AppError> {
    validate_file(file)?;
    let text = match input {
        TemplateInput::Content { content } => content,
        TemplateInput::Form { front_matter, body } => {
            let mut mapping = Mapping::new();
            apply_form(&mut mapping, &front_matter)?;
            render(&mapping, &body)?
        }
    };
    validate_text(&text)?;
    let _guard = lock();
    let path = template_path(dirs, file);
    if fs::symlink_metadata(&path).is_ok() {
        return Err(AppError::Conflict(format!("{file}.md already exists")));
    }
    write(dirs, &path, file, &text)
}

pub(crate) fn update(
    dirs: &AgentDirs,
    file: &str,
    expected_revision: &str,
    input: TemplateInput,
) -> Result<TemplateDocument, AppError> {
    validate_file(file)?;
    let _guard = lock();
    let path = template_path(dirs, file);
    let current = read_existing(&path, file)?;
    if current.revision != expected_revision {
        return Err(AppError::Conflict(format!(
            "{file}.md changed on disk; reload and try again"
        )));
    }
    let text = match input {
        TemplateInput::Content { content } => content,
        TemplateInput::Form { front_matter, body } => {
            let parsed = parse(&current.content).map_err(|error| {
                AppError::Conflict(format!(
                    "{file}.md does not parse ({error}); fix it in the source editor"
                ))
            })?;
            let mut mapping = parsed.front_matter;
            apply_form(&mut mapping, &front_matter)?;
            render(&mapping, &body)?
        }
    };
    validate_text(&text)?;
    write(dirs, &path, file, &text)
}

pub(crate) fn delete(dirs: &AgentDirs, file: &str) -> Result<(), AppError> {
    validate_file(file)?;
    let _guard = lock();
    match fs::remove_file(template_path(dirs, file)) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            Err(AppError::NotFound(format!("{file}.md does not exist")))
        }
        Err(error) => Err(error.into()),
    }
}

/// Switches a built-in agent off by writing an `enabled: false` stub named
/// after it, or back on by removing that stub. A built-in replaced by a
/// template is switched through the template instead.
pub(crate) fn set_built_in(
    dirs: &AgentDirs,
    name: &str,
    enabled: bool,
) -> Result<TemplateList, AppError> {
    let Some(&name) = BUILT_IN_AGENTS.iter().find(|&&built_in| built_in == name) else {
        return Err(AppError::NotFound(format!(
            "{name} is not a built-in agent"
        )));
    };
    {
        let _guard = lock();
        let state = list(dirs)?
            .built_ins
            .into_iter()
            .find(|built_in| built_in.name == name)
            .expect("every built-in is listed");
        match (state.state, enabled) {
            (BuiltInState::Overridden, _) => {
                return Err(AppError::Conflict(format!(
                    "{name} is replaced by {}.md; edit that template instead",
                    state.file.unwrap_or_default()
                )));
            }
            (BuiltInState::Enabled, true) | (BuiltInState::Disabled, false) => {}
            (BuiltInState::Enabled, false) => {
                let path = template_path(dirs, name);
                if fs::symlink_metadata(&path).is_ok() {
                    // An unparseable file of that name: not a stub to rewrite.
                    return Err(AppError::Conflict(format!(
                        "{name}.md exists but does not parse; fix or delete it first"
                    )));
                }
                write(dirs, &path, name, "---\nenabled: false\n---\n")?;
            }
            (BuiltInState::Disabled, true) => {
                fs::remove_file(template_path(dirs, &state.file.unwrap_or_default()))?;
            }
        }
    }
    list(dirs)
}

fn lock() -> std::sync::MutexGuard<'static, ()> {
    WRITE_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

fn template_path(dirs: &AgentDirs, file: &str) -> PathBuf {
    templates_dir(dirs).join(format!("{file}.md"))
}

fn template_stem(path: &Path) -> Option<String> {
    let name = path.file_name()?.to_str()?;
    let stem = name.strip_suffix(".md")?;
    (!stem.is_empty()).then(|| stem.to_owned())
}

fn validate_file(file: &str) -> Result<(), AppError> {
    let valid = !file.is_empty()
        && file.chars().count() <= MAX_NAME_CHARS
        && !file.starts_with('.')
        && file
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'));
    if valid {
        Ok(())
    } else {
        Err(AppError::InvalidRequest(format!(
            "template name {file:?} must be 1-{MAX_NAME_CHARS} letters, digits, '.', '_' or '-' and not start with '.'"
        )))
    }
}

fn validate_text(text: &str) -> Result<(), AppError> {
    if text.len() > MAX_TEMPLATE_BYTES {
        return Err(AppError::PayloadTooLarge(
            "the template exceeds the 256 KiB limit".to_owned(),
        ));
    }
    let parsed = parse(text).map_err(AppError::InvalidRequest)?;
    if let Some(Yaml::String(name)) = parsed.front_matter.get("name") {
        if name.contains(':') {
            return Err(AppError::InvalidRequest(
                "name must not contain ':'; use display_name for a label".to_owned(),
            ));
        }
    }
    Ok(())
}

/// Writes `text` to `path`, which must stay inside the templates directory
/// once symlinks are resolved.
fn write(
    dirs: &AgentDirs,
    path: &Path,
    file: &str,
    text: &str,
) -> Result<TemplateDocument, AppError> {
    let dir = templates_dir(dirs);
    fs::create_dir_all(&dir)?;
    let root = fs::canonicalize(&dir)?;
    if fs::symlink_metadata(path).is_ok_and(|metadata| metadata.file_type().is_symlink()) {
        let target = fs::canonicalize(path)?;
        if !target.starts_with(&root) {
            return Err(AppError::InvalidRequest(format!(
                "{file}.md resolves outside {}",
                dir.display()
            )));
        }
    }
    crate::secure_fs::replace_keeping_mode(path, text.as_bytes())?;
    read_existing(path, file)
}

fn read_existing(path: &Path, file: &str) -> Result<TemplateDocument, AppError> {
    let bytes = match fs::read(path) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            return Err(AppError::NotFound(format!("{file}.md does not exist")));
        }
        Err(error) => return Err(error.into()),
    };
    if bytes.len() > MAX_TEMPLATE_BYTES {
        return Err(AppError::PayloadTooLarge(format!(
            "{file}.md exceeds the 256 KiB limit"
        )));
    }
    let revision = cli_settings::revision(&bytes);
    let content = String::from_utf8(bytes)
        .map_err(|_| AppError::InvalidRequest(format!("{file}.md is not UTF-8")))?;
    let (front_matter, body, parse_error) = match parse(&content).and_then(|parsed| {
        let front_matter = match serde_json::to_value(Yaml::Mapping(parsed.front_matter)) {
            Ok(Value::Object(object)) => object,
            _ => return Err("front matter keys must be strings".to_owned()),
        };
        Ok((front_matter, parsed.body))
    }) {
        Ok((front_matter, body)) => (Some(front_matter), body, None),
        Err(error) => (None, String::new(), Some(error)),
    };
    Ok(TemplateDocument {
        file: file.to_owned(),
        path: path.display().to_string(),
        revision,
        content,
        front_matter,
        body,
        parse_error,
    })
}

#[derive(Debug)]
struct Parsed {
    front_matter: Mapping,
    body: String,
}

/// Splits a template the way Pi's `parseFrontmatter` does: front matter
/// only when the text opens with `---` and a later line starts with `---`;
/// the body is the rest, trimmed.
fn parse(text: &str) -> Result<Parsed, String> {
    let text = text.strip_prefix('\u{feff}').unwrap_or(text);
    let normalized = text.replace("\r\n", "\n").replace('\r', "\n");
    let split = normalized
        .starts_with("---")
        .then(|| normalized[3..].find("\n---").map(|at| at + 3))
        .flatten();
    let Some(end) = split else {
        return Ok(Parsed {
            front_matter: Mapping::new(),
            body: normalized,
        });
    };
    let yaml = normalized.get(4..end).unwrap_or("");
    let body = normalized[end + 4..].trim().to_owned();
    let front_matter = match serde_yaml_ng::from_str::<Yaml>(yaml) {
        Ok(Yaml::Mapping(mapping)) => mapping,
        Ok(Yaml::Null) => Mapping::new(),
        Ok(_) => return Err("front matter must be a YAML mapping".to_owned()),
        Err(error) => return Err(format!("invalid front matter: {error}")),
    };
    Ok(Parsed { front_matter, body })
}

fn summarize(file: String, parsed: &Parsed) -> TemplateSummary {
    let text = |key: &str| match parsed.front_matter.get(key) {
        Some(Yaml::String(value)) if !value.trim().is_empty() => Some(value.clone()),
        _ => None,
    };
    let name = text("name")
        .map(|name| name.trim().to_owned())
        .unwrap_or_else(|| file.clone());
    TemplateSummary {
        overrides_built_in: BUILT_IN_AGENTS.contains(&name.as_str()),
        display_name: text("display_name"),
        description: text("description"),
        color: text("color"),
        model: text("model"),
        enabled: parsed.front_matter.get("enabled") != Some(&Yaml::Bool(false)),
        parse_error: None,
        name,
        file,
    }
}

fn is_disable_stub(file: &str, parsed: &Parsed) -> bool {
    BUILT_IN_AGENTS.contains(&file)
        && parsed.body.is_empty()
        && parsed.front_matter.len() == 1
        && parsed.front_matter.get("enabled") == Some(&Yaml::Bool(false))
}

/// Applies a form patch: `null` removes a key, other values replace it in
/// place; new keys are inserted in [`KEY_ORDER`].
fn apply_form(mapping: &mut Mapping, patch: &Map<String, Value>) -> Result<(), AppError> {
    let rank = |key: &str| {
        KEY_ORDER
            .iter()
            .position(|known| *known == key)
            .unwrap_or(KEY_ORDER.len())
    };
    let mut keys: Vec<&String> = patch.keys().collect();
    keys.sort_by_key(|key| (rank(key), key.as_str()));
    for key in keys {
        let value = &patch[key];
        if value.is_null() {
            mapping.shift_remove(key.as_str());
            continue;
        }
        let value = serde_yaml_ng::to_value(value).map_err(|error| {
            AppError::InvalidRequest(format!("invalid value for {key}: {error}"))
        })?;
        match mapping.get_mut(key.as_str()) {
            Some(slot) => *slot = value,
            None => {
                mapping.insert(Yaml::String(key.clone()), value);
            }
        }
    }
    Ok(())
}

fn render(front_matter: &Mapping, body: &str) -> Result<String, AppError> {
    let body = body.trim();
    if front_matter.is_empty() {
        return Ok(if body.is_empty() {
            String::new()
        } else {
            format!("{body}\n")
        });
    }
    let yaml = serde_yaml_ng::to_string(front_matter)
        .map_err(|error| AppError::InvalidRequest(format!("cannot write front matter: {error}")))?;
    Ok(if body.is_empty() {
        format!("---\n{yaml}---\n")
    } else {
        format!("---\n{yaml}---\n\n{body}\n")
    })
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn temp_dirs() -> AgentDirs {
        let root = std::env::temp_dir().join(format!(
            "pi-agent-templates-{}",
            uuid::Uuid::new_v4().simple()
        ));
        fs::create_dir_all(&root).unwrap();
        AgentDirs::for_tests_with_pi_dir(root)
    }

    fn form(value: Value, body: &str) -> TemplateInput {
        TemplateInput::Form {
            front_matter: value.as_object().unwrap().clone(),
            body: body.to_owned(),
        }
    }

    fn content(text: &str) -> TemplateInput {
        TemplateInput::Content {
            content: text.to_owned(),
        }
    }

    #[test]
    fn parses_like_pi() {
        let parsed = parse("\u{feff}---\r\ndescription: x\r\n---\r\n\r\n  Prompt  \r\n").unwrap();
        assert_eq!(
            parsed.front_matter.get("description"),
            Some(&Yaml::String("x".into()))
        );
        assert_eq!(parsed.body, "Prompt");
        // No closing fence: the whole text is the body.
        assert_eq!(parse("---\nname: a\n").unwrap().body, "---\nname: a\n");
        assert!(parse("plain prompt").unwrap().front_matter.is_empty());
        assert!(parse("---\n- a\n---\nx").is_err());
        assert!(parse("---\n\n---\nx").unwrap().front_matter.is_empty());
    }

    #[test]
    fn creates_lists_and_reads_form_templates() {
        let dirs = temp_dirs();
        let created = create(
            &dirs,
            "auditor",
            form(
                json!({"thinking": "high", "description": "Security reviewer", "name": null, "color": "red"}),
                "\nYou audit code.\n",
            ),
        )
        .unwrap();
        assert_eq!(
            created.content,
            "---\ndescription: Security reviewer\ncolor: red\nthinking: high\n---\n\nYou audit code.\n"
        );
        assert_eq!(created.body, "You audit code.");
        assert_eq!(created.front_matter.unwrap()["color"], json!("red"));

        let listed = list(&dirs).unwrap();
        assert_eq!(listed.templates.len(), 1);
        let template = &listed.templates[0];
        assert_eq!(
            (template.file.as_str(), template.name.as_str()),
            ("auditor", "auditor")
        );
        assert_eq!(template.description.as_deref(), Some("Security reviewer"));
        assert!(template.enabled && !template.overrides_built_in);
        assert!(listed
            .built_ins
            .iter()
            .all(|built_in| built_in.state == BuiltInState::Enabled));

        assert!(matches!(
            create(&dirs, "auditor", content("x")),
            Err(AppError::Conflict(_))
        ));
    }

    #[test]
    fn form_updates_keep_order_and_unknown_keys() {
        let dirs = temp_dirs();
        let text = "---\n# comment\nmodel: a/b\nmemory: project\ndescription: old\n---\nBody\n";
        let created = create(&dirs, "keep", content(text)).unwrap();
        assert_eq!(created.content, text, "source writes are byte for byte");
        let updated = update(
            &dirs,
            "keep",
            &created.revision,
            form(
                json!({"description": "new", "model": null, "enabled": false}),
                "New body",
            ),
        )
        .unwrap();
        assert_eq!(
            updated.content,
            "---\nmemory: project\ndescription: new\nenabled: false\n---\n\nNew body\n"
        );
        assert!(!list(&dirs).unwrap().templates[0].enabled);

        assert!(matches!(
            update(&dirs, "keep", &created.revision, content("x")),
            Err(AppError::Conflict(_))
        ));
    }

    #[test]
    fn rejects_bad_names_and_text() {
        let dirs = temp_dirs();
        for file in ["", ".hidden", "a/b", "../x", "a b", &"x".repeat(101)] {
            assert!(
                matches!(
                    create(&dirs, file, content("x")),
                    Err(AppError::InvalidRequest(_))
                ),
                "{file}"
            );
        }
        assert!(matches!(
            create(&dirs, "bad", content("---\nname: plugin:agent\n---\n")),
            Err(AppError::InvalidRequest(_))
        ));
        assert!(matches!(
            create(&dirs, "bad", content("---\n[unclosed\n---\n")),
            Err(AppError::InvalidRequest(_))
        ));
        assert!(matches!(
            create(&dirs, "big", content(&"x".repeat(MAX_TEMPLATE_BYTES + 1))),
            Err(AppError::PayloadTooLarge(_))
        ));
    }

    #[test]
    fn unparseable_files_are_listed_and_can_be_fixed_or_deleted() {
        let dirs = temp_dirs();
        let dir = templates_dir(&dirs);
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("broken.md"), "---\n[oops\n---\n").unwrap();
        fs::write(dir.join("notes.txt"), "ignored").unwrap();
        let listed = list(&dirs).unwrap();
        assert_eq!(listed.templates.len(), 1);
        assert!(listed.templates[0].parse_error.is_some());

        let doc = read(&dirs, "broken").unwrap();
        assert!(doc.front_matter.is_none() && doc.parse_error.is_some());
        assert!(matches!(
            update(&dirs, "broken", &doc.revision, form(json!({}), "x")),
            Err(AppError::Conflict(_))
        ));
        let fixed = update(
            &dirs,
            "broken",
            &doc.revision,
            content("---\ndescription: ok\n---\n"),
        )
        .unwrap();
        assert!(fixed.parse_error.is_none());

        delete(&dirs, "broken").unwrap();
        assert!(matches!(
            delete(&dirs, "broken"),
            Err(AppError::NotFound(_))
        ));
        assert!(list(&dirs).unwrap().templates.is_empty());
    }

    #[test]
    fn built_ins_switch_through_stubs_and_report_overrides() {
        let dirs = temp_dirs();
        let state = |dirs: &AgentDirs, name: &str| {
            list(dirs)
                .unwrap()
                .built_ins
                .into_iter()
                .find(|built_in| built_in.name == name)
                .unwrap()
                .state
        };
        let listed = set_built_in(&dirs, "Explore", false).unwrap();
        assert!(listed.templates.is_empty(), "stubs are not templates");
        assert_eq!(state(&dirs, "Explore"), BuiltInState::Disabled);
        assert_eq!(
            fs::read_to_string(template_path(&dirs, "Explore")).unwrap(),
            "---\nenabled: false\n---\n"
        );
        set_built_in(&dirs, "Explore", true).unwrap();
        assert_eq!(state(&dirs, "Explore"), BuiltInState::Enabled);
        assert!(!template_path(&dirs, "Explore").exists());

        // A template naming a built-in's type replaces it, whatever its file.
        create(
            &dirs,
            "my-plan",
            content("---\nname: Plan\n---\nPlan better."),
        )
        .unwrap();
        let listed = list(&dirs).unwrap();
        assert!(listed.templates[0].overrides_built_in);
        assert_eq!(state(&dirs, "Plan"), BuiltInState::Overridden);
        assert!(matches!(
            set_built_in(&dirs, "Plan", false),
            Err(AppError::Conflict(_))
        ));
        assert!(matches!(
            set_built_in(&dirs, "plan", false),
            Err(AppError::NotFound(_))
        ));
    }

    #[cfg(unix)]
    #[test]
    fn writes_refuse_symlinks_out_of_the_directory() {
        let dirs = temp_dirs();
        let dir = templates_dir(&dirs);
        fs::create_dir_all(&dir).unwrap();
        let outside = dirs.pi_dir().join("outside.md");
        fs::write(&outside, "x").unwrap();
        std::os::unix::fs::symlink(&outside, dir.join("link.md")).unwrap();
        let doc = read(&dirs, "link").unwrap();
        assert!(matches!(
            update(&dirs, "link", &doc.revision, content("y")),
            Err(AppError::InvalidRequest(_))
        ));
        assert_eq!(fs::read_to_string(outside).unwrap(), "x");
    }
}
