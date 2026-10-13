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
use crate::provider::profile::{CatalogProfile, CatalogSource, UserConfigFile};

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
    /// Whether the listed skills can be edited and deleted here.
    pub editable: bool,
    /// Skill sources (`shared-user`, `provider-project`, …) a new skill can
    /// be created in.
    pub creatable_sources: Vec<&'static str>,
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
    pub(crate) path: PathBuf,
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
    /// Whether the listed entries can be edited and deleted here.
    pub editable: bool,
    /// Scopes a new entry can be added to (the first config file of each).
    pub creatable_scopes: Vec<CatalogScope>,
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
    #[serde(skip)]
    origin: McpOrigin,
}

/// Where a catalog MCP entry was declared, so it can be shown verbatim or
/// removed from that exact spot.
#[derive(Clone, Debug, Default)]
struct McpOrigin {
    path: PathBuf,
    format: McpConfigFormat,
    /// JSON pointer of the `mcpServers` map holding the entry; empty for TOML,
    /// whose entries always sit under `mcp_servers`.
    map_pointer: String,
    /// The entry's key exactly as written, before name cleanup.
    key: String,
    /// The entry as declared (TOML converted to JSON).
    raw: Value,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum McpConfigFormat {
    #[default]
    Json,
    Toml,
    /// Reported by the provider's own inspection (Grok Build), not a file
    /// TodeX parses.
    Native,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct McpServerConfig {
    pub descriptor: McpServerDescriptor,
    pub path: String,
    pub format: McpConfigFormat,
    pub config: Value,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct McpServerDeletion {
    pub resource_id: String,
    pub name: String,
    pub path: String,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SkillDeletion {
    pub resource_id: String,
    pub name: String,
    pub path: String,
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

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
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

impl CatalogScope {
    fn as_str(self) -> &'static str {
        match self {
            Self::User => "user",
            Self::Project => "project",
        }
    }
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

    /// The catalog a [`CatalogSource::NativeInspect`] CLI reports about
    /// itself (only Grok Build has one).
    async fn native_inspect(
        &self,
        provider: ProviderKind,
        workspace: &Path,
    ) -> Result<Value, AppError> {
        match provider {
            ProviderKind::GrokBuild => {
                crate::provider::inspect_grok(&self.config.agent, workspace).await
            }
            other => Err(AppError::Unsupported(format!(
                "{} has no native catalog inspection",
                other.as_str()
            ))),
        }
    }

    pub async fn skills(
        &self,
        provider: ProviderKind,
        workspace: PathBuf,
    ) -> Result<SkillCatalog, AppError> {
        if catalog_profile(provider).source == CatalogSource::NativeInspect {
            let inspect = self.native_inspect(provider, &workspace).await?;
            return Ok(SkillCatalog {
                provider,
                skills: parse_grok_skills(&inspect, &workspace),
                editable: false,
                creatable_sources: Vec::new(),
            });
        }
        let roots = skill_roots(self.home.as_deref(), &workspace, provider);
        let creatable_sources = roots.iter().map(|root| root.source).collect();
        let skills = tokio::task::spawn_blocking(move || scan_skills(roots))
            .await
            .map_err(|error| AppError::Anyhow(error.into()))??;
        Ok(SkillCatalog {
            provider,
            skills,
            editable: true,
            creatable_sources,
        })
    }

    pub async fn skill_resource(
        &self,
        provider: ProviderKind,
        workspace: PathBuf,
        resource_id: &str,
    ) -> Result<SkillResource, AppError> {
        let mut resources = self
            .skill_resources(provider, workspace, &[resource_id])
            .await?;
        Ok(resources.remove(0))
    }

    /// [`Self::skill_resource`] for each of `resource_ids`, in order, from one
    /// scan of the skill roots.
    pub async fn skill_resources(
        &self,
        provider: ProviderKind,
        workspace: PathBuf,
        resource_ids: &[&str],
    ) -> Result<Vec<SkillResource>, AppError> {
        if resource_ids.is_empty() {
            return Ok(Vec::new());
        }
        let catalog = self.skills(provider, workspace).await?;
        let mut resources = Vec::with_capacity(resource_ids.len());
        for resource_id in resource_ids {
            let descriptor = catalog
                .skills
                .iter()
                .find(|skill| skill.resource_id == *resource_id)
                .cloned()
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
            resources.push(SkillResource {
                descriptor,
                content,
            });
        }
        Ok(resources)
    }

    pub async fn mcp(
        &self,
        provider: ProviderKind,
        workspace: PathBuf,
    ) -> Result<McpCatalog, AppError> {
        if catalog_profile(provider).source == CatalogSource::NativeInspect {
            let inspect = self.native_inspect(provider, &workspace).await?;
            return Ok(McpCatalog {
                provider,
                servers: parse_grok_mcp(&inspect, &workspace),
                editable: false,
                creatable_scopes: Vec::new(),
            });
        }
        let sources = mcp_sources(self.home.as_deref(), &workspace, provider);
        let mut creatable_scopes = Vec::new();
        for scope in [CatalogScope::User, CatalogScope::Project] {
            if sources.iter().any(|source| source.scope == scope) {
                creatable_scopes.push(scope);
            }
        }
        let home = self.home.clone();
        let workspace_for_scan = workspace.clone();
        let servers = tokio::task::spawn_blocking(move || {
            scan_mcp(home.as_deref(), &workspace_for_scan, provider)
        })
        .await
        .map_err(|error| AppError::Anyhow(error.into()))??;
        Ok(McpCatalog {
            provider,
            servers,
            editable: !sources.is_empty(),
            creatable_scopes,
        })
    }

    pub async fn mcp_target(
        &self,
        provider: ProviderKind,
        workspace: PathBuf,
        resource_id: &str,
    ) -> Result<McpRuntimeTarget, AppError> {
        if catalog_profile(provider).source == CatalogSource::NativeInspect {
            return Err(AppError::Unsupported(format!(
                "{} MCP servers are invoked natively during provider sessions",
                crate::provider::profile::profile(provider).display_name
            )));
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

    /// One catalog MCP entry with its configuration exactly as declared,
    /// credentials included.
    pub async fn mcp_config(
        &self,
        provider: ProviderKind,
        workspace: PathBuf,
        resource_id: &str,
    ) -> Result<McpServerConfig, AppError> {
        let descriptor = self
            .mcp_descriptor(provider, workspace, resource_id)
            .await?;
        Ok(McpServerConfig {
            path: descriptor.origin.path.to_string_lossy().into_owned(),
            format: descriptor.origin.format,
            config: descriptor.origin.raw.clone(),
            descriptor,
        })
    }

    /// Removes one catalog MCP entry from the config file that declares it,
    /// leaving the rest of the file in place.
    pub async fn delete_mcp(
        &self,
        provider: ProviderKind,
        workspace: PathBuf,
        resource_id: &str,
    ) -> Result<McpServerDeletion, AppError> {
        if catalog_profile(provider).source == CatalogSource::NativeInspect {
            return Err(AppError::Unsupported(format!(
                "{} MCP servers are managed by the provider itself",
                crate::provider::profile::profile(provider).display_name
            )));
        }
        let descriptor = self
            .mcp_descriptor(provider, workspace, resource_id)
            .await?;
        let origin = descriptor.origin.clone();
        tokio::task::spawn_blocking(move || remove_mcp_entry(&origin))
            .await
            .map_err(|error| AppError::Anyhow(error.into()))??;
        Ok(McpServerDeletion {
            resource_id: descriptor.resource_id,
            name: descriptor.name,
            path: descriptor.origin.path.to_string_lossy().into_owned(),
        })
    }

    /// Adds a new MCP entry to the first config file of `scope`, creating
    /// the file when it does not exist yet.
    pub async fn create_mcp(
        &self,
        provider: ProviderKind,
        workspace: PathBuf,
        scope: CatalogScope,
        name: String,
        config: Value,
    ) -> Result<McpServerConfig, AppError> {
        let name = validate_mcp_entry(name, &config)?;
        let source = mcp_sources(self.home.as_deref(), &workspace, provider)
            .into_iter()
            .find(|source| source.scope == scope)
            .ok_or_else(|| {
                AppError::Unsupported(format!(
                    "{} has no {} MCP config file TodeX can write",
                    crate::provider::profile::profile(provider).display_name,
                    scope.as_str()
                ))
            })?;
        let path = source.path.clone();
        let entry_name = name.clone();
        tokio::task::spawn_blocking(move || insert_mcp_entry(&path, &entry_name, config))
            .await
            .map_err(|error| AppError::Anyhow(error.into()))??;
        let resource_id = resource_id_for_mcp(&name, &source.path);
        self.mcp_config(provider, workspace, &resource_id).await
    }

    /// Replaces one MCP entry in the file that declares it, optionally
    /// renaming it. A rename changes the entry's resource id.
    pub async fn update_mcp(
        &self,
        provider: ProviderKind,
        workspace: PathBuf,
        resource_id: &str,
        name: String,
        config: Value,
    ) -> Result<McpServerConfig, AppError> {
        if catalog_profile(provider).source == CatalogSource::NativeInspect {
            return Err(AppError::Unsupported(format!(
                "{} MCP servers are managed by the provider itself",
                crate::provider::profile::profile(provider).display_name
            )));
        }
        let name = validate_mcp_entry(name, &config)?;
        let descriptor = self
            .mcp_descriptor(provider, workspace.clone(), resource_id)
            .await?;
        let origin = descriptor.origin.clone();
        let entry_name = name.clone();
        tokio::task::spawn_blocking(move || replace_mcp_entry(&origin, &entry_name, config))
            .await
            .map_err(|error| AppError::Anyhow(error.into()))??;
        let resource_id = resource_id_for_mcp(&name, &descriptor.origin.path);
        self.mcp_config(provider, workspace, &resource_id).await
    }

    /// Writes a new `<root>/<name>/SKILL.md` under the skill root of
    /// `source`.
    pub async fn create_skill(
        &self,
        provider: ProviderKind,
        workspace: PathBuf,
        source: &str,
        name: &str,
        content: String,
    ) -> Result<SkillDescriptor, AppError> {
        if catalog_profile(provider).source == CatalogSource::NativeInspect {
            return Err(AppError::Unsupported(format!(
                "{} skills are managed by the provider itself",
                crate::provider::profile::profile(provider).display_name
            )));
        }
        validate_skill_directory_name(name)?;
        validate_skill_content(&content)?;
        let root = skill_roots(self.home.as_deref(), &workspace, provider)
            .into_iter()
            .find(|root| root.source == source)
            .ok_or_else(|| AppError::InvalidRequest(format!("unknown skill source {source}")))?;
        let path = root.path.join(name).join("SKILL.md");
        let target = path.clone();
        tokio::task::spawn_blocking(move || -> Result<(), AppError> {
            let directory = target.parent().expect("skill file has a parent");
            if std::fs::symlink_metadata(directory).is_ok() {
                return Err(AppError::Conflict(format!(
                    "{} already exists",
                    directory.display()
                )));
            }
            std::fs::create_dir_all(directory)?;
            std::fs::write(&target, content)?;
            Ok(())
        })
        .await
        .map_err(|error| AppError::Anyhow(error.into()))??;
        self.skill_descriptor(provider, workspace, &resource_id("skill", &path))
            .await
    }

    /// Overwrites one skill's `SKILL.md`; invalid skills can be fixed this
    /// way too.
    pub async fn update_skill(
        &self,
        provider: ProviderKind,
        workspace: PathBuf,
        resource_id: &str,
        content: String,
    ) -> Result<SkillDescriptor, AppError> {
        let descriptor = self
            .writable_skill(provider, workspace.clone(), resource_id)
            .await?;
        validate_skill_content(&content)?;
        let path = descriptor.path.clone();
        tokio::task::spawn_blocking(move || replace_file(&path, content.as_bytes()))
            .await
            .map_err(|error| AppError::Anyhow(error.into()))??;
        self.skill_descriptor(provider, workspace, resource_id)
            .await
    }

    /// Deletes a skill's directory (or just its `SKILL.md` when it sits
    /// directly in a skill root).
    pub async fn delete_skill(
        &self,
        provider: ProviderKind,
        workspace: PathBuf,
        resource_id: &str,
    ) -> Result<SkillDeletion, AppError> {
        let descriptor = self
            .writable_skill(provider, workspace.clone(), resource_id)
            .await?;
        let roots = skill_roots(self.home.as_deref(), &workspace, provider);
        let path = descriptor.path.clone();
        let removed = tokio::task::spawn_blocking(move || -> Result<PathBuf, AppError> {
            let directory = path
                .parent()
                .expect("skill file has a parent")
                .to_path_buf();
            if roots.iter().any(|root| root.path == directory) {
                std::fs::remove_file(&path)?;
                return Ok(path);
            }
            let metadata = std::fs::symlink_metadata(&directory)?;
            if metadata.file_type().is_symlink() || !metadata.is_dir() {
                return Err(AppError::InvalidRequest(format!(
                    "{} is not a regular directory",
                    directory.display()
                )));
            }
            std::fs::remove_dir_all(&directory)?;
            Ok(directory)
        })
        .await
        .map_err(|error| AppError::Anyhow(error.into()))??;
        Ok(SkillDeletion {
            resource_id: descriptor.resource_id,
            name: descriptor.name,
            path: removed.to_string_lossy().into_owned(),
        })
    }

    async fn skill_descriptor(
        &self,
        provider: ProviderKind,
        workspace: PathBuf,
        resource_id: &str,
    ) -> Result<SkillDescriptor, AppError> {
        self.skills(provider, workspace)
            .await?
            .skills
            .into_iter()
            .find(|skill| skill.resource_id == resource_id)
            .ok_or_else(|| AppError::NotFound(format!("skill resource {resource_id}")))
    }

    async fn writable_skill(
        &self,
        provider: ProviderKind,
        workspace: PathBuf,
        resource_id: &str,
    ) -> Result<SkillDescriptor, AppError> {
        if catalog_profile(provider).source == CatalogSource::NativeInspect {
            return Err(AppError::Unsupported(format!(
                "{} skills are managed by the provider itself",
                crate::provider::profile::profile(provider).display_name
            )));
        }
        self.skill_descriptor(provider, workspace, resource_id)
            .await
    }

    async fn mcp_descriptor(
        &self,
        provider: ProviderKind,
        workspace: PathBuf,
        resource_id: &str,
    ) -> Result<McpServerDescriptor, AppError> {
        self.mcp(provider, workspace)
            .await?
            .servers
            .into_iter()
            .find(|server| server.resource_id == resource_id)
            .ok_or_else(|| AppError::NotFound(format!("mcp resource {resource_id}")))
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
                origin: McpOrigin::default(),
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
                origin: McpOrigin::default(),
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

fn catalog_profile(provider: ProviderKind) -> &'static CatalogProfile {
    &crate::provider::profile::profile(provider).catalog
}

fn provider_user_skill_root(home: &Path, provider: ProviderKind) -> PathBuf {
    catalog_profile(provider)
        .config_home
        .resolve(home)
        .join("skills")
}

fn provider_project_skill_root(workspace: &Path, provider: ProviderKind) -> PathBuf {
    workspace.join(catalog_profile(provider).project_skills)
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
        let format = if source.path.extension().and_then(|value| value.to_str()) == Some("toml") {
            McpConfigFormat::Toml
        } else {
            McpConfigFormat::Json
        };
        let parsed = match format {
            McpConfigFormat::Toml => parse_toml_mcp(&raw),
            _ => parse_json_mcp(&raw),
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
                resource_id: resource_id_for_mcp(&entry.name, &source.path),
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
                origin: McpOrigin {
                    path: source.path.clone(),
                    format,
                    map_pointer: entry.map_pointer,
                    key: entry.key,
                    raw: entry.raw,
                },
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

/// The MCP config files TodeX lists for `provider`; none for providers that
/// keep their servers elsewhere (ACP agents, OpenCode, Grok's inspection).
fn mcp_sources(home: Option<&Path>, workspace: &Path, provider: ProviderKind) -> Vec<SourceRoot> {
    let catalog = catalog_profile(provider);
    if catalog.source != CatalogSource::Filesystem {
        return Vec::new();
    }
    let mut sources = Vec::new();
    if let Some(home) = home {
        let config_home = catalog.config_home.resolve(home);
        for file in catalog.mcp_user_files {
            let path = match file {
                UserConfigFile::Home(relative) => home.join(relative),
                UserConfigFile::ConfigHome(relative) => config_home.join(relative),
            };
            sources.push(SourceRoot {
                path,
                scope: CatalogScope::User,
                source: catalog.mcp_user_source,
                priority: 20,
            });
        }
    }
    for relative in catalog.mcp_project_files {
        sources.push(SourceRoot {
            path: workspace.join(relative),
            scope: CatalogScope::Project,
            source: catalog.mcp_project_source,
            priority: 40,
        });
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
            let enabled =
                server.get("compatibilityStatus").and_then(Value::as_str) != Some("disabled");
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
                origin: McpOrigin {
                    path: source_path,
                    format: McpConfigFormat::Native,
                    map_pointer: String::new(),
                    key: name.to_owned(),
                    raw: server.clone(),
                },
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
    key: String,
    map_pointer: String,
    raw: Value,
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
            let mut entry = parsed_mcp_from_toml(clean_name(name.clone()), table);
            entry.key = name.clone();
            entry.raw = serde_json::to_value(table).unwrap_or(Value::Null);
            Some(entry)
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
                .filter_map(|(key, value)| Some((key.clone(), value.as_str()?.to_owned())))
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
        key: String::new(),
        map_pointer: String::new(),
        raw: Value::Null,
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
    find_mcp_server_maps(&value, String::new(), 0, &mut maps);
    let mut entries = BTreeMap::new();
    for (map_pointer, map) in maps {
        for (name, value) in map {
            let Some(server) = value.as_object() else {
                continue;
            };
            let mut entry = parsed_mcp_from_json(server);
            entry.key = name.clone();
            entry.map_pointer = map_pointer.clone();
            entry.raw = value.clone();
            entries.insert(clean_name(name.clone()), entry);
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
            command.extend(args.iter().filter_map(Value::as_str).map(ToOwned::to_owned));
        }
    }
    let url = server
        .get("url")
        .and_then(Value::as_str)
        .map(ToOwned::to_owned);
    let transport =
        if !command.is_empty() || server.get("type").and_then(Value::as_str) == Some("stdio") {
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
        key: String::new(),
        map_pointer: String::new(),
        raw: Value::Null,
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

/// Collects every `mcpServers` map with its JSON pointer.
fn find_mcp_server_maps<'a>(
    value: &'a Value,
    pointer: String,
    depth: usize,
    output: &mut Vec<(String, &'a serde_json::Map<String, Value>)>,
) {
    if depth > 4 {
        return;
    }
    match value {
        Value::Object(map) => {
            if let Some(servers) = map.get("mcpServers").and_then(Value::as_object) {
                output.push((format!("{pointer}/mcpServers"), servers));
            }
            for (key, value) in map {
                let token = key.replace('~', "~0").replace('/', "~1");
                find_mcp_server_maps(value, format!("{pointer}/{token}"), depth + 1, output);
            }
        }
        Value::Array(values) => {
            for (index, value) in values.iter().enumerate() {
                find_mcp_server_maps(value, format!("{pointer}/{index}"), depth + 1, output);
            }
        }
        _ => {}
    }
}

/// Deletes the entry `origin` points at and rewrites the file in place of
/// the old one, keeping its permissions. Fails with `NotFound` when the file
/// changed and the entry is no longer there.
fn remove_mcp_entry(origin: &McpOrigin) -> Result<(), AppError> {
    let path = &origin.path;
    let raw = std::fs::read_to_string(path)?;
    let missing = || AppError::NotFound(format!("mcp server {} in {}", origin.key, path.display()));
    let updated = match origin.format {
        McpConfigFormat::Toml => {
            let mut document = raw.parse::<toml_edit::DocumentMut>().map_err(|error| {
                AppError::InvalidRequest(format!("invalid MCP TOML config: {error}"))
            })?;
            document
                .get_mut("mcp_servers")
                .and_then(toml_edit::Item::as_table_like_mut)
                .and_then(|servers| servers.remove(&origin.key))
                .ok_or_else(missing)?;
            document.to_string()
        }
        McpConfigFormat::Json => {
            let mut value: Value = serde_json::from_str(&raw).map_err(|error| {
                AppError::InvalidRequest(format!("invalid MCP JSON config: {error}"))
            })?;
            value
                .pointer_mut(&origin.map_pointer)
                .and_then(Value::as_object_mut)
                .and_then(|servers| servers.remove(&origin.key))
                .ok_or_else(missing)?;
            let mut text = serde_json::to_string_pretty(&value)
                .map_err(|error| AppError::Anyhow(error.into()))?;
            if raw.ends_with('\n') {
                text.push('\n');
            }
            text
        }
        McpConfigFormat::Native => {
            return Err(AppError::Unsupported(
                "natively managed MCP servers cannot be removed here".to_owned(),
            ))
        }
    };
    replace_file(path, updated.as_bytes())
}

/// Swaps the regular file at `path` for `contents` through a temporary
/// file, keeping its permissions.
fn replace_file(path: &Path, contents: &[u8]) -> Result<(), AppError> {
    let metadata = std::fs::symlink_metadata(path)?;
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        return Err(AppError::InvalidRequest(format!(
            "{} is not a regular file",
            path.display()
        )));
    }
    write_through_temp(path, contents, Some(metadata.permissions()))
}

fn write_through_temp(
    path: &Path,
    contents: &[u8],
    permissions: Option<std::fs::Permissions>,
) -> Result<(), AppError> {
    let file_name = path
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_default();
    let temp = path.with_file_name(format!(
        ".{file_name}.todex-{}",
        uuid::Uuid::new_v4().simple()
    ));
    std::fs::write(&temp, contents)?;
    let result = match permissions {
        Some(permissions) => std::fs::set_permissions(&temp, permissions),
        None => restrict_to_owner(&temp),
    }
    .and_then(|()| std::fs::rename(&temp, path));
    if result.is_err() {
        let _ = std::fs::remove_file(&temp);
    }
    result.map_err(Into::into)
}

/// New MCP config files can hold credentials, so only the owner reads them.
fn restrict_to_owner(path: &Path) -> std::io::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))
    }
    #[cfg(not(unix))]
    {
        let _ = path;
        Ok(())
    }
}

/// Adds `name` to the config file at `path` (top-level `mcpServers` for
/// JSON, `[mcp_servers]` for TOML), creating the file when missing.
fn insert_mcp_entry(path: &Path, name: &str, config: Value) -> Result<(), AppError> {
    let existing = match std::fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_symlink() || !metadata.is_file() => {
            return Err(AppError::InvalidRequest(format!(
                "{} is not a regular file",
                path.display()
            )))
        }
        Ok(metadata) if metadata.len() > MAX_CONFIG_BYTES => {
            return Err(AppError::InvalidRequest(format!(
                "{} exceeds {MAX_CONFIG_BYTES} bytes",
                path.display()
            )))
        }
        Ok(metadata) => Some((std::fs::read_to_string(path)?, metadata.permissions())),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
        Err(error) => return Err(error.into()),
    };
    let raw = existing.as_ref().map(|(raw, _)| raw.as_str()).unwrap_or("");
    let conflict = || {
        AppError::Conflict(format!(
            "mcp server {name} already exists in {}",
            path.display()
        ))
    };
    let updated = if path.extension().and_then(|value| value.to_str()) == Some("toml") {
        let mut document = raw.parse::<toml_edit::DocumentMut>().map_err(|error| {
            AppError::InvalidRequest(format!("invalid MCP TOML config: {error}"))
        })?;
        let servers = document
            .entry("mcp_servers")
            .or_insert_with(|| {
                let mut table = toml_edit::Table::new();
                table.set_implicit(true);
                toml_edit::Item::Table(table)
            })
            .as_table_like_mut()
            .ok_or_else(|| AppError::InvalidRequest("mcp_servers is not a table".to_owned()))?;
        if servers.contains_key(name) {
            return Err(conflict());
        }
        servers.insert(name, toml_edit::Item::Table(json_to_toml_table(&config)?));
        document.to_string()
    } else {
        let mut value: Value = if raw.trim().is_empty() {
            Value::Object(serde_json::Map::new())
        } else {
            serde_json::from_str(raw).map_err(|error| {
                AppError::InvalidRequest(format!("invalid MCP JSON config: {error}"))
            })?
        };
        let root = value.as_object_mut().ok_or_else(|| {
            AppError::InvalidRequest("MCP JSON config is not an object".to_owned())
        })?;
        let servers = root
            .entry("mcpServers")
            .or_insert_with(|| Value::Object(serde_json::Map::new()))
            .as_object_mut()
            .ok_or_else(|| AppError::InvalidRequest("mcpServers is not an object".to_owned()))?;
        if servers.contains_key(name) {
            return Err(conflict());
        }
        servers.insert(name.to_owned(), config);
        pretty_json(&value, raw.is_empty() || raw.ends_with('\n'))?
    };
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    write_through_temp(
        path,
        updated.as_bytes(),
        existing.map(|(_, permissions)| permissions),
    )
}

/// Replaces the entry `origin` points at with `config` under `name`.
fn replace_mcp_entry(origin: &McpOrigin, name: &str, config: Value) -> Result<(), AppError> {
    let path = &origin.path;
    let raw = std::fs::read_to_string(path)?;
    let missing = || AppError::NotFound(format!("mcp server {} in {}", origin.key, path.display()));
    let conflict = || {
        AppError::Conflict(format!(
            "mcp server {name} already exists in {}",
            path.display()
        ))
    };
    let updated = match origin.format {
        McpConfigFormat::Toml => {
            let mut document = raw.parse::<toml_edit::DocumentMut>().map_err(|error| {
                AppError::InvalidRequest(format!("invalid MCP TOML config: {error}"))
            })?;
            let servers = document
                .get_mut("mcp_servers")
                .and_then(toml_edit::Item::as_table_like_mut)
                .ok_or_else(missing)?;
            if !servers.contains_key(&origin.key) {
                return Err(missing());
            }
            if name != origin.key && servers.contains_key(name) {
                return Err(conflict());
            }
            let table = json_to_toml_table(&config)?;
            if name == origin.key {
                servers.insert(name, toml_edit::Item::Table(table));
            } else {
                servers.remove(&origin.key);
                servers.insert(name, toml_edit::Item::Table(table));
            }
            document.to_string()
        }
        McpConfigFormat::Json => {
            let mut value: Value = serde_json::from_str(&raw).map_err(|error| {
                AppError::InvalidRequest(format!("invalid MCP JSON config: {error}"))
            })?;
            let servers = value
                .pointer_mut(&origin.map_pointer)
                .and_then(Value::as_object_mut)
                .ok_or_else(missing)?;
            if !servers.contains_key(&origin.key) {
                return Err(missing());
            }
            if name != origin.key && servers.contains_key(name) {
                return Err(conflict());
            }
            servers.remove(&origin.key);
            servers.insert(name.to_owned(), config);
            pretty_json(&value, raw.ends_with('\n'))?
        }
        McpConfigFormat::Native => {
            return Err(AppError::Unsupported(
                "natively managed MCP servers cannot be edited here".to_owned(),
            ))
        }
    };
    replace_file(path, updated.as_bytes())
}

fn pretty_json(value: &Value, trailing_newline: bool) -> Result<String, AppError> {
    let mut text =
        serde_json::to_string_pretty(value).map_err(|error| AppError::Anyhow(error.into()))?;
    if trailing_newline {
        text.push('\n');
    }
    Ok(text)
}

/// An MCP entry as a TOML table; nested objects become inline tables so
/// the entry stays one `[mcp_servers.<name>]` block.
fn json_to_toml_table(config: &Value) -> Result<toml_edit::Table, AppError> {
    let object = config
        .as_object()
        .ok_or_else(|| AppError::InvalidRequest("MCP config must be a JSON object".to_owned()))?;
    let mut table = toml_edit::Table::new();
    for (key, value) in object {
        table.insert(key, toml_edit::Item::Value(json_to_toml_value(value)?));
    }
    Ok(table)
}

pub(crate) fn json_to_toml_value(value: &Value) -> Result<toml_edit::Value, AppError> {
    Ok(match value {
        Value::Null => {
            return Err(AppError::InvalidRequest(
                "TOML has no null; remove the field instead".to_owned(),
            ))
        }
        Value::Bool(value) => (*value).into(),
        Value::Number(number) => match (number.as_i64(), number.as_f64()) {
            (Some(integer), _) => integer.into(),
            (None, Some(float)) => float.into(),
            _ => {
                return Err(AppError::InvalidRequest(format!(
                    "number {number} does not fit in TOML"
                )))
            }
        },
        Value::String(value) => value.as_str().into(),
        Value::Array(values) => {
            let mut array = toml_edit::Array::new();
            for value in values {
                array.push(json_to_toml_value(value)?);
            }
            toml_edit::Value::Array(array)
        }
        Value::Object(map) => {
            let mut table = toml_edit::InlineTable::new();
            for (key, value) in map {
                table.insert(key, json_to_toml_value(value)?);
            }
            toml_edit::Value::InlineTable(table)
        }
    })
}

/// Trims the entry name and checks the config is a JSON object; returns the
/// name to write.
fn validate_mcp_entry(name: String, config: &Value) -> Result<String, AppError> {
    let name = name.trim().to_owned();
    if name.is_empty() || name.chars().count() > 200 || name.chars().any(char::is_control) {
        return Err(AppError::InvalidRequest(
            "MCP server name must be 1-200 printable characters".to_owned(),
        ));
    }
    if !config.is_object() {
        return Err(AppError::InvalidRequest(
            "MCP config must be a JSON object".to_owned(),
        ));
    }
    Ok(name)
}

/// Skill directory names are a single path segment of letters, digits,
/// `.`, `_` and `-`.
fn validate_skill_directory_name(name: &str) -> Result<(), AppError> {
    let valid = !name.is_empty()
        && name.len() <= 100
        && !name.starts_with('.')
        && name
            .chars()
            .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '.' | '_' | '-'));
    if valid {
        Ok(())
    } else {
        Err(AppError::InvalidRequest(
            "skill name must be 1-100 letters, digits, '.', '_' or '-', not starting with '.'"
                .to_owned(),
        ))
    }
}

fn validate_skill_content(content: &str) -> Result<(), AppError> {
    if content.len() as u64 > MAX_SKILL_BYTES {
        return Err(AppError::InvalidRequest(format!(
            "SKILL.md exceeds {MAX_SKILL_BYTES} bytes"
        )));
    }
    parse_skill_front_matter(content).map(|_| ())
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

fn resource_id_for_mcp(name: &str, path: &Path) -> String {
    resource_id(&format!("mcp:{name}"), path)
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

    use serde_json::json;

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

    #[tokio::test]
    async fn mcp_entries_are_shown_raw_and_deleted_in_place() {
        let root = temp_dir("todex-catalog-mcp-edit");
        let home = root.join("home");
        let workspace_root = root.join("workspaces");
        let workspace = workspace_root.join("project");
        fs::create_dir_all(workspace.join(".codex")).unwrap();
        fs::create_dir_all(&home).unwrap();
        let claude = home.join(".claude.json");
        fs::write(
            &claude,
            r#"{"numStartups":3,"mcpServers":{"keep":{"command":"keep"}},"projects":{"/srv/a/b":{"mcpServers":{"drop":{"type":"http","url":"https://example.test/mcp","headers":{"Authorization":"Bearer t"}}}}}}
"#,
        )
        .unwrap();
        let codex = workspace.join(".codex/config.toml");
        fs::write(
            &codex,
            "# keep me\nmodel = \"x\"\n\n[mcp_servers.gone]\ncommand = \"npx\"\n\n[mcp_servers.gone.env]\nTOKEN = \"s\"\n\n[mcp_servers.stay]\nurl = \"https://stay.test\"\n",
        )
        .unwrap();

        let config = Arc::new(test_config(root.join("data"), workspace_root));
        let service = CatalogService::with_home(config, Some(home));

        let claude_catalog = service
            .mcp(ProviderKind::ClaudeCode, workspace.clone())
            .await
            .unwrap();
        let drop = claude_catalog
            .servers
            .iter()
            .find(|server| server.name == "drop")
            .unwrap();
        let raw = service
            .mcp_config(
                ProviderKind::ClaudeCode,
                workspace.clone(),
                &drop.resource_id,
            )
            .await
            .unwrap();
        assert_eq!(raw.format, McpConfigFormat::Json);
        assert_eq!(raw.config["headers"]["Authorization"], "Bearer t");
        service
            .delete_mcp(
                ProviderKind::ClaudeCode,
                workspace.clone(),
                &drop.resource_id,
            )
            .await
            .unwrap();
        let rewritten: Value = serde_json::from_str(&fs::read_to_string(&claude).unwrap()).unwrap();
        assert_eq!(rewritten["numStartups"], 3);
        assert!(rewritten["mcpServers"]["keep"].is_object());
        assert!(rewritten["projects"]["/srv/a/b"]["mcpServers"]
            .as_object()
            .unwrap()
            .is_empty());
        assert!(service
            .delete_mcp(
                ProviderKind::ClaudeCode,
                workspace.clone(),
                &drop.resource_id
            )
            .await
            .is_err());

        let codex_catalog = service
            .mcp(ProviderKind::Codex, workspace.clone())
            .await
            .unwrap();
        let gone = codex_catalog
            .servers
            .iter()
            .find(|server| server.name == "gone")
            .unwrap();
        let raw = service
            .mcp_config(ProviderKind::Codex, workspace.clone(), &gone.resource_id)
            .await
            .unwrap();
        assert_eq!(raw.format, McpConfigFormat::Toml);
        assert_eq!(raw.config["env"]["TOKEN"], "s");
        service
            .delete_mcp(ProviderKind::Codex, workspace.clone(), &gone.resource_id)
            .await
            .unwrap();
        let toml = fs::read_to_string(&codex).unwrap();
        assert!(toml.starts_with("# keep me\nmodel = \"x\"\n"));
        assert!(!toml.contains("gone"));
        assert!(toml.contains("[mcp_servers.stay]"));
        let _ = fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn mcp_entries_and_skills_are_created_edited_and_deleted() {
        let root = temp_dir("todex-catalog-write");
        let home = root.join("home");
        let workspace_root = root.join("workspaces");
        let workspace = workspace_root.join("project");
        fs::create_dir_all(&workspace).unwrap();
        fs::create_dir_all(home.join(".codex")).unwrap();
        let codex = home.join(".codex/config.toml");
        fs::write(
            &codex,
            "# top\nmodel = \"x\"\n\n[profiles.fast]\nmodel = \"y\"\n",
        )
        .unwrap();
        let config = Arc::new(test_config(root.join("data"), workspace_root));
        let service = CatalogService::with_home(config, Some(home.clone()));

        let catalog = service
            .mcp(ProviderKind::ClaudeCode, workspace.clone())
            .await
            .unwrap();
        assert!(catalog.editable);
        assert_eq!(
            catalog.creatable_scopes,
            vec![CatalogScope::User, CatalogScope::Project]
        );
        let created = service
            .create_mcp(
                ProviderKind::ClaudeCode,
                workspace.clone(),
                CatalogScope::Project,
                " docs ".to_owned(),
                json!({ "command": "npx", "args": ["docs-mcp"] }),
            )
            .await
            .unwrap();
        assert_eq!(created.descriptor.name, "docs");
        assert_eq!(created.path, workspace.join(".mcp.json").to_string_lossy());
        assert!(matches!(
            service
                .create_mcp(
                    ProviderKind::ClaudeCode,
                    workspace.clone(),
                    CatalogScope::Project,
                    "docs".to_owned(),
                    json!({}),
                )
                .await,
            Err(AppError::Conflict(_))
        ));
        let renamed = service
            .update_mcp(
                ProviderKind::ClaudeCode,
                workspace.clone(),
                &created.descriptor.resource_id,
                "manual".to_owned(),
                json!({ "type": "http", "url": "https://example.test/mcp" }),
            )
            .await
            .unwrap();
        assert_eq!(renamed.descriptor.transport, McpTransport::Http);
        let written: Value =
            serde_json::from_str(&fs::read_to_string(workspace.join(".mcp.json")).unwrap())
                .unwrap();
        assert_eq!(
            written,
            json!({ "mcpServers": { "manual": { "type": "http", "url": "https://example.test/mcp" } } })
        );

        let toml_entry = service
            .create_mcp(
                ProviderKind::Codex,
                workspace.clone(),
                CatalogScope::User,
                "files".to_owned(),
                json!({ "command": "npx", "args": ["-y", "files"], "env": { "TOKEN": "s" } }),
            )
            .await
            .unwrap();
        assert_eq!(toml_entry.config["env"]["TOKEN"], "s");
        service
            .update_mcp(
                ProviderKind::Codex,
                workspace.clone(),
                &toml_entry.descriptor.resource_id,
                "files".to_owned(),
                json!({ "url": "https://files.test", "enabled": false }),
            )
            .await
            .unwrap();
        let toml = fs::read_to_string(&codex).unwrap();
        assert!(
            toml.starts_with("# top\nmodel = \"x\"\n\n[profiles.fast]\nmodel = \"y\"\n"),
            "{toml}"
        );
        assert!(
            toml.contains("[mcp_servers.files]\nurl = \"https://files.test\"\nenabled = false\n"),
            "{toml}"
        );
        assert!(!toml.contains("TOKEN"));

        let skills = service
            .skills(ProviderKind::ClaudeCode, workspace.clone())
            .await
            .unwrap();
        assert!(skills.editable);
        assert!(skills.creatable_sources.contains(&"provider-project"));
        let content = "---\nname: review\ndescription: Review code\n---\n\nBody\n";
        assert!(service
            .create_skill(
                ProviderKind::ClaudeCode,
                workspace.clone(),
                "provider-project",
                "../x",
                content.to_owned()
            )
            .await
            .is_err());
        assert!(service
            .create_skill(
                ProviderKind::ClaudeCode,
                workspace.clone(),
                "provider-project",
                "review",
                "no front matter".to_owned()
            )
            .await
            .is_err());
        let skill = service
            .create_skill(
                ProviderKind::ClaudeCode,
                workspace.clone(),
                "provider-project",
                "review",
                content.to_owned(),
            )
            .await
            .unwrap();
        assert_eq!(skill.description, "Review code");
        let skill_dir = workspace.join(".claude/skills/review");
        fs::write(skill_dir.join("helper.sh"), "echo").unwrap();
        let edited = service
            .update_skill(
                ProviderKind::ClaudeCode,
                workspace.clone(),
                &skill.resource_id,
                "---\nname: review\ndescription: Review diffs\n---\n".to_owned(),
            )
            .await
            .unwrap();
        assert_eq!(edited.resource_id, skill.resource_id);
        assert_eq!(edited.description, "Review diffs");
        service
            .delete_skill(
                ProviderKind::ClaudeCode,
                workspace.clone(),
                &skill.resource_id,
            )
            .await
            .unwrap();
        assert!(!skill_dir.exists());
        assert!(workspace.join(".claude/skills").exists());
        let _ = fs::remove_dir_all(root);
    }

    /// Where each provider's skills and MCP servers are read from, frozen
    /// across the move to `ProviderProfile`.
    #[tokio::test]
    async fn provider_catalog_sources_match_golden() {
        let root = temp_dir("todex-catalog-golden");
        let home = root.join("home");
        let workspace_root = root.join("workspaces");
        let workspace = workspace_root.join("project");
        fs::create_dir_all(&workspace).unwrap();
        fs::create_dir_all(&home).unwrap();
        // One distinct server in every MCP config file any provider reads.
        let json_files = [
            ".claude.json",
            ".claude/settings.json",
            ".pi/agent/mcp.json",
            ".pi/agent/settings.json",
            ".config/devin/mcp_config.json",
            ".config/devin/mcp_config.local.json",
        ];
        let project_json = [
            ".mcp.json",
            ".claude/settings.json",
            ".pi/mcp.json",
            ".devin/mcp_config.json",
            ".devin/mcp_config.local.json",
        ];
        let write_json = |path: PathBuf, name: &str| {
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(
                path,
                format!(r#"{{"mcpServers":{{"{name}":{{"command":"tool"}}}}}}"#),
            )
            .unwrap();
        };
        for file in json_files {
            write_json(home.join(file), &format!("user:{file}"));
        }
        for file in project_json {
            write_json(workspace.join(file), &format!("project:{file}"));
        }
        let write_toml = |path: PathBuf, name: &str| {
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(
                path,
                format!("[mcp_servers.\"{name}\"]\ncommand = \"tool\"\n"),
            )
            .unwrap();
        };
        write_toml(home.join(".codex/config.toml"), "user:codex");
        write_toml(workspace.join(".codex/config.toml"), "project:codex");

        // Provider homes may be redirected by the environment; record the
        // default spelling either way so the snapshot is host independent.
        let redirects: Vec<(PathBuf, &str)> = [
            ("CODEX_HOME", "~/.codex"),
            ("PI_CODING_AGENT_DIR", "~/.pi/agent"),
            ("GROK_HOME", "~/.grok"),
        ]
        .into_iter()
        .filter_map(|(name, default)| {
            std::env::var_os(name).map(|value| (PathBuf::from(value), default))
        })
        .collect();
        // Forward slashes on every platform, so one golden file serves all.
        let normalize = |path: &Path| -> String {
            let shown = 'shown: {
                for (redirect, default) in &redirects {
                    if let Ok(rest) = path.strip_prefix(redirect) {
                        break 'shown Path::new(default).join(rest);
                    }
                }
                if let Ok(rest) = path.strip_prefix(&workspace) {
                    break 'shown Path::new("<workspace>").join(rest);
                }
                if let Ok(rest) = path.strip_prefix(&home) {
                    break 'shown Path::new("~").join(rest);
                }
                path.to_path_buf()
            };
            shown.display().to_string().replace('\\', "/")
        };
        let roots = |roots: Vec<SourceRoot>| -> Value {
            let mut roots = roots
                .into_iter()
                .map(|root| {
                    json!({
                        "path": normalize(&root.path),
                        "scope": root.scope,
                        "source": root.source,
                        "priority": root.priority,
                    })
                })
                .collect::<Vec<_>>();
            roots.sort_by_key(|root| root.to_string());
            Value::Array(roots)
        };

        let mut config = test_config(root.join("data"), workspace_root);
        config.agent.grok_bin = "/nonexistent/todex-golden/grok".to_owned();
        let service = CatalogService::with_home(Arc::new(config), Some(home.clone()));
        let outcome = |result: Result<Value, AppError>| {
            result.unwrap_or_else(|error| json!({ "error": error.to_string() }))
        };
        let mut snapshot = serde_json::Map::new();
        for provider in ProviderKind::ALL {
            let mcp = service
                .mcp(provider, workspace.clone())
                .await
                .map(|catalog| {
                    json!(catalog
                        .servers
                        .iter()
                        .map(|server| json!({
                            "name": server.name,
                            "source": server.source,
                            "scope": server.scope,
                            "active": server.active,
                        }))
                        .collect::<Vec<_>>())
                });
            let skills = service
                .skills(provider, workspace.clone())
                .await
                .map(|catalog| json!(catalog.skills.len()));
            let target = service
                .mcp_target(provider, workspace.clone(), "missing")
                .await
                .map(|_| json!("found"));
            snapshot.insert(
                provider.as_str().to_owned(),
                json!({
                    "skillRoots": roots(skill_roots(Some(&home), &workspace, provider)),
                    "mcpSources": roots(mcp_sources(Some(&home), &workspace, provider)),
                    "skills": outcome(skills),
                    "mcp": outcome(mcp),
                    "mcpTarget": outcome(target),
                }),
            );
        }
        crate::provider::golden_support::assert_golden("catalog_sources", &Value::Object(snapshot));
        let _ = fs::remove_dir_all(root);
    }

    fn test_config(data_dir: PathBuf, workspace_root: PathBuf) -> Config {
        Config {
            host: "127.0.0.1".to_owned(),
            port: 0,
            pairing_encryption: PairingEncryption::None,
            data_dir,
            workspace_roots: vec![workspace_root],
            history_retention_days: None,
            agent: AgentConfig {
                default_agent: "codex".to_owned(),
                codex_bin: "codex".to_owned(),
                claude_bin: "claude".to_owned(),
                pi_bin: "pi".to_owned(),
                grok_bin: "grok".to_owned(),
                grok_auth_method: None,
                grok_env_allowlist: Vec::new(),
                devin_bin: "devin".to_owned(),
                devin_auth_method: None,
                devin_api_key_env: None,
                devin_env_allowlist: Vec::new(),
                opencode_bin: "opencode".to_owned(),
                opencode_env_allowlist: Vec::new(),
                dsh_bin: "dsh".to_owned(),
                dsh_env_allowlist: Vec::new(),
                antigravity_bin: "agy".to_owned(),
                antigravity_env_allowlist: Vec::new(),
                acp_profiles: BTreeMap::new(),
                ssh_bin: "ssh".to_owned(),
                provider_idle_timeout_minutes: 0,
            },
            security: SecurityConfig {
                enable_auth: true,
                enable_tls: false,
            },
            api: Default::default(),
        }
    }

    fn temp_dir(prefix: &str) -> PathBuf {
        std::env::temp_dir().join(format!("{prefix}-{}", uuid::Uuid::new_v4().simple()))
    }
}
