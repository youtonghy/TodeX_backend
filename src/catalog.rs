use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine as _;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};

use crate::config::Config;
use crate::conversation::ProviderKind;
use crate::error::AppError;

const MAX_SKILL_BYTES: u64 = 256 * 1024;
const MAX_CONFIG_BYTES: u64 = 2 * 1024 * 1024;
const MAX_SCAN_ENTRIES: usize = 2000;
const MAX_SCAN_VISITED_ENTRIES: usize = 20_000;
const MAX_SCAN_DEPTH: usize = 4;

#[derive(Clone)]
pub struct CatalogService {
    config: Arc<Config>,
    home: Option<PathBuf>,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SkillCatalog {
    pub provider: ProviderKind,
    pub skills: Vec<SkillDescriptor>,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SkillDescriptor {
    pub resource_id: String,
    pub name: String,
    pub description: String,
    pub scope: CatalogScope,
    pub source: String,
    pub active: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub shadowed_by: Option<String>,
    pub valid: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    #[serde(skip)]
    path: PathBuf,
    #[serde(skip)]
    priority: u8,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SkillResource {
    pub descriptor: SkillDescriptor,
    pub content: String,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct McpCatalog {
    pub provider: ProviderKind,
    pub servers: Vec<McpServerDescriptor>,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct McpToolDescriptor {
    pub name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct McpServerDescriptor {
    pub resource_id: String,
    pub name: String,
    pub provider: ProviderKind,
    pub scope: CatalogScope,
    pub source: String,
    pub transport: McpTransport,
    pub enabled: bool,
    pub active: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub shadowed_by: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tools: Vec<McpToolDescriptor>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub auth_status: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    #[serde(skip)]
    priority: u8,
    #[serde(skip)]
    command: Vec<String>,
    #[serde(skip)]
    env: BTreeMap<String, String>,
    #[serde(skip)]
    url: Option<String>,
    #[serde(skip)]
    headers: BTreeMap<String, String>,
}

#[derive(Clone, Debug)]
pub struct McpRuntimeTarget {
    pub descriptor: McpServerDescriptor,
    pub command: Vec<String>,
    pub env: BTreeMap<String, String>,
    pub url: Option<String>,
    pub headers: BTreeMap<String, String>,
    pub workspace: PathBuf,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum CatalogScope {
    User,
    Project,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum McpTransport {
    Stdio,
    Http,
    Unknown,
}

#[derive(Debug, Default, Deserialize)]
struct SkillFrontMatter {
    name: Option<String>,
    description: Option<String>,
}

#[derive(Clone)]
struct SourceRoot {
    path: PathBuf,
    scope: CatalogScope,
    source: &'static str,
    priority: u8,
}

impl CatalogService {
    pub fn new(config: Arc<Config>) -> Self {
        Self {
            config,
            home: std::env::var_os("HOME").map(PathBuf::from),
        }
    }

    #[cfg(test)]
    pub fn with_home(config: Arc<Config>, home: Option<PathBuf>) -> Self {
        Self { config, home }
    }

    pub async fn skills(
        &self,
        provider: ProviderKind,
        workspace: PathBuf,
    ) -> Result<SkillCatalog, AppError> {
        if provider == ProviderKind::GrokBuild {
            let inspect = crate::provider::inspect_grok(&self.config.agent, &workspace).await?;
            return Ok(SkillCatalog {
                provider,
                skills: parse_grok_skills(&inspect, &workspace),
            });
        }
        let roots = skill_roots(self.home.as_deref(), &workspace, provider);
        let skills = tokio::task::spawn_blocking(move || scan_skills(roots))
            .await
            .map_err(|error| AppError::Anyhow(error.into()))??;
        Ok(SkillCatalog { provider, skills })
    }

    pub async fn skill_resource(
        &self,
        provider: ProviderKind,
        workspace: PathBuf,
        resource_id: &str,
    ) -> Result<SkillResource, AppError> {
        let catalog = self.skills(provider, workspace).await?;
        let descriptor = catalog
            .skills
            .into_iter()
            .find(|skill| skill.resource_id == resource_id)
            .ok_or_else(|| AppError::NotFound(format!("skill resource {resource_id}")))?;
        if !descriptor.valid {
            return Err(AppError::InvalidRequest(
                descriptor
                    .error
                    .clone()
                    .unwrap_or_else(|| "skill resource is not readable".to_owned()),
            ));
        }
        let path = descriptor.path.clone();
        let content =
            tokio::task::spawn_blocking(move || read_limited_text(&path, MAX_SKILL_BYTES))
                .await
                .map_err(|error| AppError::Anyhow(error.into()))??;
        Ok(SkillResource {
            descriptor,
            content,
        })
    }

    pub async fn mcp(
        &self,
        provider: ProviderKind,
        workspace: PathBuf,
    ) -> Result<McpCatalog, AppError> {
        if provider == ProviderKind::GrokBuild {
            let inspect = crate::provider::inspect_grok(&self.config.agent, &workspace).await?;
            return Ok(McpCatalog {
                provider,
                servers: parse_grok_mcp(&inspect, &workspace),
            });
        }
        let home = self.home.clone();
        let workspace_for_scan = workspace.clone();
        let servers = tokio::task::spawn_blocking(move || {
            scan_mcp(home.as_deref(), &workspace_for_scan, provider)
        })
        .await
        .map_err(|error| AppError::Anyhow(error.into()))??;
        Ok(McpCatalog { provider, servers })
    }

    pub async fn mcp_target(
        &self,
        provider: ProviderKind,
        workspace: PathBuf,
        resource_id: &str,
    ) -> Result<McpRuntimeTarget, AppError> {
        if provider == ProviderKind::GrokBuild {
            return Err(AppError::Unsupported(
                "Grok Build MCP servers are invoked natively during provider sessions".to_owned(),
            ));
        }
        let catalog = self.mcp(provider, workspace.clone()).await?;
        let descriptor = catalog
            .servers
            .into_iter()
            .find(|server| server.resource_id == resource_id)
            .ok_or_else(|| AppError::NotFound(format!("mcp resource {resource_id}")))?;
        if !descriptor.enabled || !descriptor.active {
            return Err(AppError::InvalidRequest(format!(
                "mcp server {} is not active",
                descriptor.name
            )));
        }
        Ok(McpRuntimeTarget {
            command: descriptor.command.clone(),
            env: descriptor.env.clone(),
            url: descriptor.url.clone(),
            headers: descriptor.headers.clone(),
            workspace,
            descriptor,
        })
    }

    pub fn config(&self) -> &Config {
        &self.config
    }
}

#[cfg(test)]
impl McpRuntimeTarget {
    pub(crate) fn stdio_fixture(name: &str, command: Vec<String>, workspace: PathBuf) -> Self {
        Self {
            descriptor: McpServerDescriptor {
                resource_id: format!("mcp_{name}"),
                name: name.to_owned(),
                provider: ProviderKind::Codex,
                scope: CatalogScope::User,
                source: "test".to_owned(),
                transport: McpTransport::Stdio,
                enabled: true,
                active: true,
                shadowed_by: None,
                tools: Vec::new(),
                auth_status: None,
                error: None,
                priority: 1,
                command: command.clone(),
                env: BTreeMap::new(),
                url: None,
                headers: BTreeMap::new(),
            },
            command,
            env: BTreeMap::new(),
            url: None,
            headers: BTreeMap::new(),
            workspace,
        }
    }

    pub(crate) fn http_fixture(name: &str, url: String, workspace: PathBuf) -> Self {
        Self {
            descriptor: McpServerDescriptor {
                resource_id: format!("mcp_{name}"),
                name: name.to_owned(),
                provider: ProviderKind::Codex,
                scope: CatalogScope::User,
                source: "test".to_owned(),
                transport: McpTransport::Http,
                enabled: true,
                active: true,
                shadowed_by: None,
                tools: Vec::new(),
                auth_status: None,
                error: None,
                priority: 1,
                command: Vec::new(),
                env: BTreeMap::new(),
                url: Some(url.clone()),
                headers: BTreeMap::new(),
            },
            command: Vec::new(),
            env: BTreeMap::new(),
            url: Some(url),
            headers: BTreeMap::new(),
            workspace,
        }
    }
}

fn skill_roots(home: Option<&Path>, workspace: &Path, provider: ProviderKind) -> Vec<SourceRoot> {
    let mut roots = Vec::new();
    if let Some(home) = home {
        roots.push(SourceRoot {
            path: home.join(".agents/skills"),
            scope: CatalogScope::User,
            source: "shared-user",
            priority: 10,
        });
        roots.push(SourceRoot {
            path: provider_user_skill_root(home, provider),
            scope: CatalogScope::User,
            source: "provider-user",
            priority: 20,
        });
    }
    roots.push(SourceRoot {
        path: workspace.join(".agents/skills"),
        scope: CatalogScope::Project,
        source: "shared-project",
        priority: 30,
    });
    roots.push(SourceRoot {
        path: provider_project_skill_root(workspace, provider),
        scope: CatalogScope::Project,
        source: "provider-project",
        priority: 40,
    });
    deduplicate_roots(roots)
}

fn provider_user_skill_root(home: &Path, provider: ProviderKind) -> PathBuf {
    match provider {
        ProviderKind::Acp => home.join(".agents/skills"),
        ProviderKind::Codex => std::env::var_os("CODEX_HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|| home.join(".codex"))
            .join("skills"),
        ProviderKind::Pi => std::env::var_os("PI_CODING_AGENT_DIR")
            .map(PathBuf::from)
            .unwrap_or_else(|| home.join(".pi/agent"))
            .join("skills"),
        ProviderKind::ClaudeCode => home.join(".claude/skills"),
        ProviderKind::GrokBuild => std::env::var_os("GROK_HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|| home.join(".grok"))
            .join("skills"),
    }
}

fn provider_project_skill_root(workspace: &Path, provider: ProviderKind) -> PathBuf {
    match provider {
        ProviderKind::Acp => workspace.join(".agents/skills"),
        ProviderKind::Codex => workspace.join(".codex/skills"),
        ProviderKind::Pi => workspace.join(".pi/skills"),
        ProviderKind::ClaudeCode => workspace.join(".claude/skills"),
        ProviderKind::GrokBuild => workspace.join(".grok/skills"),
    }
}

fn deduplicate_roots(roots: Vec<SourceRoot>) -> Vec<SourceRoot> {
    let mut selected = BTreeMap::<PathBuf, SourceRoot>::new();
    for root in roots {
        match selected.get(&root.path) {
            Some(existing) if existing.priority >= root.priority => {}
            _ => {
                selected.insert(root.path.clone(), root);
            }
        }
    }
    selected.into_values().collect()
}

fn scan_skills(mut roots: Vec<SourceRoot>) -> Result<Vec<SkillDescriptor>, AppError> {
    roots.sort_by_key(|root| std::cmp::Reverse(root.priority));
    let mut skills = Vec::new();
    let mut seen_paths = HashSet::new();
    let mut visited_entries = 0;
    for root in roots {
        let mut files = Vec::new();
        collect_named_files(&root.path, "SKILL.md", 0, &mut files, &mut visited_entries)?;
        files.sort();
        for path in files {
            if !seen_paths.insert(path.clone()) {
                continue;
            }
            let fallback_name = path
                .parent()
                .and_then(Path::file_name)
                .and_then(|name| name.to_str())
                .unwrap_or("unnamed-skill")
                .to_owned();
            let resource_id = resource_id("skill", &path);
            match read_limited_text(&path, MAX_SKILL_BYTES)
                .and_then(|content| parse_skill_front_matter(&content))
            {
                Ok(front_matter) => skills.push(SkillDescriptor {
                    resource_id,
                    name: clean_name(front_matter.name.unwrap_or(fallback_name)),
                    description: front_matter
                        .description
                        .unwrap_or_default()
                        .trim()
                        .chars()
                        .take(1000)
                        .collect(),
                    scope: root.scope,
                    source: root.source.to_owned(),
                    active: true,
                    shadowed_by: None,
                    valid: true,
                    error: None,
                    path,
                    priority: root.priority,
                }),
                Err(error) => skills.push(SkillDescriptor {
                    resource_id,
                    name: clean_name(fallback_name),
                    description: String::new(),
                    scope: root.scope,
                    source: root.source.to_owned(),
                    active: false,
                    shadowed_by: None,
                    valid: false,
                    error: Some(error.to_string().chars().take(500).collect()),
                    path,
                    priority: root.priority,
                }),
            }
        }
    }
    apply_skill_precedence(&mut skills);
    skills.sort_by(|left, right| {
        left.name
            .to_ascii_lowercase()
            .cmp(&right.name.to_ascii_lowercase())
            .then_with(|| right.priority.cmp(&left.priority))
            .then_with(|| left.resource_id.cmp(&right.resource_id))
    });
    Ok(skills)
}

fn collect_named_files(
    directory: &Path,
    name: &str,
    depth: usize,
    output: &mut Vec<PathBuf>,
    visited_entries: &mut usize,
) -> Result<(), AppError> {
    if depth > MAX_SCAN_DEPTH
        || output.len() >= MAX_SCAN_ENTRIES
        || *visited_entries >= MAX_SCAN_VISITED_ENTRIES
    {
        return Ok(());
    }
    let metadata = match std::fs::symlink_metadata(directory) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::PermissionDenied => return Ok(()),
        Err(error) => return Err(error.into()),
    };
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        return Ok(());
    }
    for entry in std::fs::read_dir(directory)? {
        if output.len() >= MAX_SCAN_ENTRIES || *visited_entries >= MAX_SCAN_VISITED_ENTRIES {
            break;
        }
        let entry = entry?;
        *visited_entries = visited_entries.saturating_add(1);
        let file_type = entry.file_type()?;
        if file_type.is_symlink() {
            continue;
        }
        if file_type.is_dir() {
            collect_named_files(&entry.path(), name, depth + 1, output, visited_entries)?;
        } else if file_type.is_file() && entry.file_name() == name {
            output.push(entry.path());
        }
    }
    Ok(())
}

fn parse_skill_front_matter(content: &str) -> Result<SkillFrontMatter, AppError> {
    let mut lines = content.lines();
    if lines.next().map(str::trim) != Some("---") {
        return Err(AppError::InvalidRequest(
            "SKILL.md is missing YAML front matter".to_owned(),
        ));
    }
    let mut yaml = String::new();
    let mut closed = false;
    for line in lines {
        if line.trim() == "---" {
            closed = true;
            break;
        }
        yaml.push_str(line);
        yaml.push('\n');
    }
    if !closed {
        return Err(AppError::InvalidRequest(
            "SKILL.md front matter is not terminated".to_owned(),
        ));
    }
    serde_yaml_ng::from_str(&yaml).map_err(|error| {
        AppError::InvalidRequest(format!("invalid SKILL.md front matter: {error}"))
    })
}

fn apply_skill_precedence(skills: &mut [SkillDescriptor]) {
    let mut winners = HashMap::<String, (u8, String)>::new();
    for skill in skills.iter().filter(|skill| skill.valid) {
        let key = skill.name.to_ascii_lowercase();
        match winners.get(&key) {
            Some((priority, _)) if *priority >= skill.priority => {}
            _ => {
                winners.insert(key, (skill.priority, skill.resource_id.clone()));
            }
        }
    }
    for skill in skills {
        let winner = winners.get(&skill.name.to_ascii_lowercase());
        skill.active =
            skill.valid && winner.is_some_and(|(_, resource_id)| resource_id == &skill.resource_id);
        skill.shadowed_by = winner
            .filter(|(_, resource_id)| resource_id != &skill.resource_id)
            .map(|(_, resource_id)| resource_id.clone());
    }
}

fn scan_mcp(
    home: Option<&Path>,
    workspace: &Path,
    provider: ProviderKind,
) -> Result<Vec<McpServerDescriptor>, AppError> {
    if matches!(provider, ProviderKind::Acp | ProviderKind::GrokBuild) {
        return Ok(Vec::new());
    }
    let mut descriptors = Vec::new();
    for source in mcp_sources(home, workspace, provider) {
        let metadata = match std::fs::symlink_metadata(&source.path) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
            Err(error) if error.kind() == std::io::ErrorKind::PermissionDenied => continue,
            Err(error) => return Err(error.into()),
        };
        if metadata.file_type().is_symlink()
            || !metadata.is_file()
            || metadata.len() > MAX_CONFIG_BYTES
        {
            continue;
        }
        let raw = match std::fs::read_to_string(&source.path) {
            Ok(raw) => raw,
            Err(error) => {
                tracing::warn!(path = %source.path.display(), error = %error, "skipping unreadable MCP config");
                continue;
            }
        };
        let parsed = if source.path.extension().and_then(|value| value.to_str()) == Some("toml") {
            parse_toml_mcp(&raw)
        } else {
            parse_json_mcp(&raw)
        };
        let entries = match parsed {
            Ok(entries) => entries,
            Err(error) => {
                tracing::warn!(path = %source.path.display(), error = %error, "skipping invalid MCP config");
                continue;
            }
        };
        for entry in entries {
            descriptors.push(McpServerDescriptor {
                resource_id: resource_id(&format!("mcp:{}", entry.name), &source.path),
                name: entry.name,
                provider,
                scope: source.scope,
                source: source.source.to_owned(),
                transport: entry.transport,
                enabled: entry.enabled,
                active: true,
                shadowed_by: None,
                tools: Vec::new(),
                auth_status: None,
                error: None,
                priority: source.priority,
                command: entry.command,
                env: entry.env,
                url: entry.url,
                headers: entry.headers,
            });
        }
    }
    apply_mcp_precedence(&mut descriptors);
    descriptors.sort_by(|left, right| {
        left.name
            .to_ascii_lowercase()
            .cmp(&right.name.to_ascii_lowercase())
            .then_with(|| right.priority.cmp(&left.priority))
            .then_with(|| left.resource_id.cmp(&right.resource_id))
    });
    Ok(descriptors)
}

fn mcp_sources(home: Option<&Path>, workspace: &Path, provider: ProviderKind) -> Vec<SourceRoot> {
    let mut sources = Vec::new();
    match provider {
        ProviderKind::Codex => {
            if let Some(home) = home {
                let codex_home = std::env::var_os("CODEX_HOME")
                    .map(PathBuf::from)
                    .unwrap_or_else(|| home.join(".codex"));
                sources.push(SourceRoot {
                    path: codex_home.join("config.toml"),
                    scope: CatalogScope::User,
                    source: "codex-user",
                    priority: 20,
                });
            }
            sources.push(SourceRoot {
                path: workspace.join(".codex/config.toml"),
                scope: CatalogScope::Project,
                source: "codex-project",
                priority: 40,
            });
        }
        ProviderKind::ClaudeCode => {
            if let Some(home) = home {
                for path in [
                    home.join(".claude.json"),
                    home.join(".claude/settings.json"),
                ] {
                    sources.push(SourceRoot {
                        path,
                        scope: CatalogScope::User,
                        source: "claude-user",
                        priority: 20,
                    });
                }
            }
            for path in [
                workspace.join(".mcp.json"),
                workspace.join(".claude/settings.json"),
            ] {
                sources.push(SourceRoot {
                    path,
                    scope: CatalogScope::Project,
                    source: "claude-project",
                    priority: 40,
                });
            }
        }
        ProviderKind::Pi => {
            if let Some(home) = home {
                let pi_home = std::env::var_os("PI_CODING_AGENT_DIR")
                    .map(PathBuf::from)
                    .unwrap_or_else(|| home.join(".pi/agent"));
                for path in [pi_home.join("mcp.json"), pi_home.join("settings.json")] {
                    sources.push(SourceRoot {
                        path,
                        scope: CatalogScope::User,
                        source: "pi-user",
                        priority: 20,
                    });
                }
            }
            for path in [workspace.join(".pi/mcp.json"), workspace.join(".mcp.json")] {
                sources.push(SourceRoot {
                    path,
                    scope: CatalogScope::Project,
                    source: "pi-project",
                    priority: 40,
                });
            }
        }
        ProviderKind::Acp => {}
        ProviderKind::GrokBuild => {}
    }
    sources
}

fn parse_grok_skills(inspect: &Value, workspace: &Path) -> Vec<SkillDescriptor> {
    let mut skills = inspect
        .get("skills")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|skill| {
            let name = skill.get("name").and_then(Value::as_str)?.trim();
            if name.is_empty() {
                return None;
            }
            let path = skill
                .pointer("/source/path")
                .and_then(Value::as_str)
                .map(PathBuf::from)
                .unwrap_or_default();
            let source_type = skill
                .pointer("/source/type")
                .and_then(Value::as_str)
                .unwrap_or("grok");
            let valid = safe_grok_skill_path(&path);
            Some(SkillDescriptor {
                resource_id: resource_id("skill", &path),
                name: clean_name(name.to_owned()),
                description: skill
                    .get("description")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .trim()
                    .chars()
                    .take(1000)
                    .collect(),
                scope: grok_scope(source_type, &path, workspace),
                source: format!("grok-{source_type}"),
                active: skill
                    .get("userInvocable")
                    .and_then(Value::as_bool)
                    .unwrap_or(true)
                    && valid,
                shadowed_by: None,
                valid,
                error: (!valid).then(|| {
                    "Grok reported a skill path that is not a readable regular file".to_owned()
                }),
                path,
                priority: 100,
            })
        })
        .collect::<Vec<_>>();
    skills.sort_by(|left, right| {
        left.name
            .to_ascii_lowercase()
            .cmp(&right.name.to_ascii_lowercase())
            .then_with(|| left.resource_id.cmp(&right.resource_id))
    });
    skills
}

fn parse_grok_mcp(inspect: &Value, workspace: &Path) -> Vec<McpServerDescriptor> {
    let mut servers = inspect
        .get("mcpServers")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|server| {
            let name = server.get("name").and_then(Value::as_str)?.trim();
            if name.is_empty() {
                return None;
            }
            let source_path = server
                .pointer("/source/path")
                .and_then(Value::as_str)
                .map(PathBuf::from)
                .unwrap_or_default();
            let source_type = server
                .pointer("/source/type")
                .and_then(Value::as_str)
                .unwrap_or("grok");
            let target = server
                .get("target")
                .and_then(Value::as_str)
                .unwrap_or_default();
            let transport = match server.get("transport").and_then(Value::as_str) {
                Some("stdio") => McpTransport::Stdio,
                Some("http" | "sse") => McpTransport::Http,
                _ => McpTransport::Unknown,
            };
            let enabled = server
                .get("compatibilityStatus")
                .and_then(Value::as_str)
                != Some("disabled");
            Some(McpServerDescriptor {
                resource_id: resource_id(&format!("mcp:{name}"), &source_path),
                name: clean_name(name.to_owned()),
                provider: ProviderKind::GrokBuild,
                scope: grok_scope(source_type, &source_path, workspace),
                source: format!("grok-{source_type}"),
                transport,
                enabled,
                active: enabled,
                shadowed_by: None,
                tools: Vec::new(),
                auth_status: None,
                error: None,
                priority: 100,
                command: Vec::new(),
                env: BTreeMap::new(),
                url: matches!(transport, McpTransport::Http)
                    .then(|| target.to_owned())
                    .filter(|value| !value.is_empty()),
                headers: BTreeMap::new(),
            })
        })
        .collect::<Vec<_>>();
    servers.sort_by(|left, right| {
        left.name
            .to_ascii_lowercase()
            .cmp(&right.name.to_ascii_lowercase())
            .then_with(|| left.resource_id.cmp(&right.resource_id))
    });
    servers
}

fn safe_grok_skill_path(path: &Path) -> bool {
    if !path.is_absolute() {
        return false;
    }
    let Ok(metadata) = std::fs::symlink_metadata(path) else {
        return false;
    };
    metadata.file_type().is_file() && !metadata.file_type().is_symlink()
}

fn grok_scope(source_type: &str, path: &Path, workspace: &Path) -> CatalogScope {
    if source_type.eq_ignore_ascii_case("project") || path.starts_with(workspace) {
        CatalogScope::Project
    } else {
        CatalogScope::User
    }
}

struct ParsedMcpServer {
    name: String,
    transport: McpTransport,
    enabled: bool,
    command: Vec<String>,
    env: BTreeMap<String, String>,
    url: Option<String>,
    headers: BTreeMap<String, String>,
}

fn parse_toml_mcp(raw: &str) -> Result<Vec<ParsedMcpServer>, AppError> {
    let value: toml::Value = toml::from_str(raw)
        .map_err(|error| AppError::InvalidRequest(format!("invalid MCP TOML config: {error}")))?;
    let Some(servers) = value.get("mcp_servers").and_then(toml::Value::as_table) else {
        return Ok(Vec::new());
    };
    Ok(servers
        .iter()
        .filter_map(|(name, value)| {
            let table = value.as_table()?;
            Some(parsed_mcp_from_toml(clean_name(name.clone()), table))
        })
        .collect())
}

fn parsed_mcp_from_toml(name: String, table: &toml::Table) -> ParsedMcpServer {
    let command = table
        .get("command")
        .and_then(toml::Value::as_str)
        .map(|command| {
            let mut parts = vec![command.to_owned()];
            if let Some(args) = table.get("args").and_then(toml::Value::as_array) {
                parts.extend(
                    args.iter()
                        .filter_map(toml::Value::as_str)
                        .map(ToOwned::to_owned),
                );
            }
            parts
        })
        .unwrap_or_default();
    let url = table
        .get("url")
        .and_then(toml::Value::as_str)
        .map(ToOwned::to_owned);
    let transport = if !command.is_empty() {
        McpTransport::Stdio
    } else if url.is_some() {
        McpTransport::Http
    } else {
        McpTransport::Unknown
    };
    let env = table
        .get("env")
        .and_then(toml::Value::as_table)
        .map(|env| {
            env.iter()
                .filter_map(|(key, value)| {
                    Some((key.clone(), value.as_str()?.to_owned()))
                })
                .collect()
        })
        .unwrap_or_default();
    let headers = table
        .get("headers")
        .and_then(toml::Value::as_table)
        .map(|headers| {
            headers
                .iter()
                .filter_map(|(key, value)| Some((key.clone(), value.as_str()?.to_owned())))
                .collect()
        })
        .unwrap_or_default();
    ParsedMcpServer {
        name,
        transport,
        enabled: table
            .get("enabled")
            .and_then(toml::Value::as_bool)
            .unwrap_or(true),
        command,
        env,
        url,
        headers,
    }
}

fn parse_json_mcp(raw: &str) -> Result<Vec<ParsedMcpServer>, AppError> {
    let value: Value = serde_json::from_str(raw)
        .map_err(|error| AppError::InvalidRequest(format!("invalid MCP JSON config: {error}")))?;
    let mut maps = Vec::new();
    find_mcp_server_maps(&value, 0, &mut maps);
    let mut entries = BTreeMap::new();
    for map in maps {
        for (name, value) in map {
            let Some(server) = value.as_object() else {
                continue;
            };
            entries.insert(clean_name(name.clone()), parsed_mcp_from_json(server));
        }
    }
    Ok(entries
        .into_iter()
        .map(|(name, mut entry)| {
            entry.name = name;
            entry
        })
        .collect())
}

fn parsed_mcp_from_json(server: &serde_json::Map<String, Value>) -> ParsedMcpServer {
    let mut command = Vec::new();
    if let Some(bin) = server.get("command").and_then(Value::as_str) {
        command.push(bin.to_owned());
        if let Some(args) = server.get("args").and_then(Value::as_array) {
            command.extend(
                args.iter()
                    .filter_map(Value::as_str)
                    .map(ToOwned::to_owned),
            );
        }
    }
    let url = server
        .get("url")
        .and_then(Value::as_str)
        .map(ToOwned::to_owned);
    let transport = if !command.is_empty()
        || server.get("type").and_then(Value::as_str) == Some("stdio")
    {
        McpTransport::Stdio
    } else if url.is_some()
        || matches!(
            server.get("type").and_then(Value::as_str),
            Some("http" | "sse")
        )
    {
        McpTransport::Http
    } else {
        McpTransport::Unknown
    };
    let env = server
        .get("env")
        .and_then(Value::as_object)
        .map(|env| {
            env.iter()
                .filter_map(|(key, value)| Some((key.clone(), value.as_str()?.to_owned())))
                .collect()
        })
        .unwrap_or_default();
    let headers = server
        .get("headers")
        .and_then(Value::as_object)
        .map(|headers| {
            headers
                .iter()
                .filter_map(|(key, value)| Some((key.clone(), value.as_str()?.to_owned())))
                .collect()
        })
        .unwrap_or_default();
    ParsedMcpServer {
        name: String::new(),
        transport,
        enabled: server
            .get("enabled")
            .and_then(Value::as_bool)
            .unwrap_or(true),
        command,
        env,
        url,
        headers,
    }
}

fn find_mcp_server_maps<'a>(
    value: &'a Value,
    depth: usize,
    output: &mut Vec<&'a serde_json::Map<String, Value>>,
) {
    if depth > 4 {
        return;
    }
    match value {
        Value::Object(map) => {
            if let Some(servers) = map.get("mcpServers").and_then(Value::as_object) {
                output.push(servers);
            }
            for value in map.values() {
                find_mcp_server_maps(value, depth + 1, output);
            }
        }
        Value::Array(values) => {
            for value in values {
                find_mcp_server_maps(value, depth + 1, output);
            }
        }
        _ => {}
    }
}

fn apply_mcp_precedence(servers: &mut [McpServerDescriptor]) {
    let mut winners = HashMap::<String, (u8, String)>::new();
    for server in servers.iter() {
        let key = server.name.to_ascii_lowercase();
        match winners.get(&key) {
            Some((priority, _)) if *priority >= server.priority => {}
            _ => {
                winners.insert(key, (server.priority, server.resource_id.clone()));
            }
        }
    }
    for server in servers {
        let winner = winners.get(&server.name.to_ascii_lowercase());
        server.active = winner.is_some_and(|(_, resource_id)| resource_id == &server.resource_id);
        server.shadowed_by = winner
            .filter(|(_, resource_id)| resource_id != &server.resource_id)
            .map(|(_, resource_id)| resource_id.clone());
    }
}

fn resource_id(kind: &str, path: &Path) -> String {
    let mut hash = Sha256::new();
    hash.update(kind.as_bytes());
    hash.update(b"\0");
    hash.update(path.as_os_str().as_encoded_bytes());
    format!("res_{}", URL_SAFE_NO_PAD.encode(hash.finalize()))
}

fn clean_name(value: String) -> String {
    let value = value.trim().chars().take(200).collect::<String>();
    if value.is_empty() {
        "unnamed".to_owned()
    } else {
        value
    }
}

fn read_limited_text(path: &Path, max_bytes: u64) -> Result<String, AppError> {
    let metadata = std::fs::symlink_metadata(path)?;
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        return Err(AppError::InvalidRequest(
            "catalog resource must be a regular file".to_owned(),
        ));
    }
    if metadata.len() > max_bytes {
        return Err(AppError::InvalidRequest(format!(
            "catalog resource exceeds {max_bytes} bytes"
        )));
    }
    std::fs::read_to_string(path).map_err(Into::into)
}

#[cfg(test)]
mod tests {
    use std::fs;

    use super::*;
    use crate::config::{AgentConfig, PairingEncryption, SecurityConfig};

    #[tokio::test]
    async fn catalogs_are_read_only_precedence_aware_and_secret_free() {
        let root = temp_dir("todex-catalog");
        let home = root.join("home");
        let workspace_root = root.join("workspaces");
        let workspace = workspace_root.join("project");
        let user_skill = home.join(".agents/skills/example/SKILL.md");
        let project_skill = workspace.join(".claude/skills/example/SKILL.md");
        fs::create_dir_all(user_skill.parent().unwrap()).unwrap();
        fs::create_dir_all(project_skill.parent().unwrap()).unwrap();
        let user_content = "---\nname: example\ndescription: user copy\n---\nUser instructions.\n";
        let project_content =
            "---\nname: example\ndescription: project copy\n---\nProject instructions.\n";
        fs::write(&user_skill, user_content).unwrap();
        fs::write(&project_skill, project_content).unwrap();
        fs::create_dir_all(&workspace).unwrap();

        #[cfg(unix)]
        {
            let outside = root.join("outside/SKILL.md");
            let escaped = workspace.join(".claude/skills/escaped");
            fs::create_dir_all(outside.parent().unwrap()).unwrap();
            fs::create_dir_all(&escaped).unwrap();
            fs::write(&outside, "---\nname: escaped\n---\nsecret\n").unwrap();
            std::os::unix::fs::symlink(&outside, escaped.join("SKILL.md")).unwrap();
        }

        fs::write(
            workspace.join(".mcp.json"),
            r#"{
                "mcpServers": {
                    "local-tools": {
                        "command": "npx",
                        "args": ["private-package"],
                        "env": {"API_TOKEN": "catalog-secret"},
                        "url": "https://user:pass@example.test/mcp?token=catalog-secret"
                    }
                }
            }"#,
        )
        .unwrap();
        fs::create_dir_all(home.join(".claude")).unwrap();
        fs::write(home.join(".claude.json"), "{ malformed").unwrap();

        let config = Arc::new(test_config(root.join("data"), workspace_root));
        let service = CatalogService::with_home(config, Some(home));
        let before_user = fs::read(&user_skill).unwrap();
        let before_project = fs::read(&project_skill).unwrap();

        let skills = service
            .skills(ProviderKind::ClaudeCode, workspace.clone())
            .await
            .unwrap();
        let matching = skills
            .skills
            .iter()
            .filter(|skill| skill.name == "example")
            .collect::<Vec<_>>();
        assert_eq!(matching.len(), 2);
        let active = matching.iter().find(|skill| skill.active).unwrap();
        assert_eq!(active.scope, CatalogScope::Project);
        assert_eq!(active.description, "project copy");
        assert!(matching
            .iter()
            .any(|skill| !skill.active && skill.shadowed_by.is_some()));
        assert!(!skills.skills.iter().any(|skill| skill.name == "escaped"));
        assert!(!serde_json::to_string(&skills).unwrap().contains("SKILL.md"));

        let resource = service
            .skill_resource(
                ProviderKind::ClaudeCode,
                workspace.clone(),
                &active.resource_id,
            )
            .await
            .unwrap();
        assert_eq!(resource.content, project_content);

        let mcp = service
            .mcp(ProviderKind::ClaudeCode, workspace)
            .await
            .unwrap();
        assert_eq!(mcp.servers.len(), 1);
        assert_eq!(mcp.servers[0].name, "local-tools");
        assert_eq!(mcp.servers[0].transport, McpTransport::Stdio);
        let serialized = serde_json::to_string(&mcp).unwrap();
        for secret in [
            "catalog-secret",
            "private-package",
            "user:pass",
            "example.test",
            "API_TOKEN",
        ] {
            assert!(!serialized.contains(secret), "catalog leaked {secret}");
        }

        assert_eq!(fs::read(user_skill).unwrap(), before_user);
        assert_eq!(fs::read(project_skill).unwrap(), before_project);
        let _ = fs::remove_dir_all(root);
    }

    fn test_config(data_dir: PathBuf, workspace_root: PathBuf) -> Config {
        Config {
            host: "127.0.0.1".to_owned(),
            port: 0,
            pairing_encryption: PairingEncryption::None,
            data_dir,
            workspace_root,
            history_retention_days: None,
            agent: AgentConfig {
                default_agent: "codex".to_owned(),
                codex_bin: "codex".to_owned(),
                claude_bin: "claude".to_owned(),
                pi_bin: "pi".to_owned(),
                grok_bin: "grok".to_owned(),
                grok_auth_method: None,
                grok_env_allowlist: Vec::new(),
                acp_profiles: BTreeMap::new(),
            },
            security: SecurityConfig {
                enable_auth: true,
                enable_tls: false,
                auth_token: Some("test-token".to_owned()),
            },
        }
    }

    fn temp_dir(prefix: &str) -> PathBuf {
        std::env::temp_dir().join(format!("{prefix}-{}", uuid::Uuid::new_v4().simple()))
    }
}
