use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::{Arc, LazyLock, Mutex, PoisonError};
use std::time::{Duration, SystemTime};

use base64::engine::general_purpose::{STANDARD, URL_SAFE_NO_PAD};
use base64::Engine as _;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};

use crate::config::Config;
use crate::conversation::ProviderKind;
use crate::error::AppError;
use crate::provider::profile::{CatalogProfile, CatalogSource, UserConfigFile};

const MAX_SKILL_BYTES: u64 = 256 * 1024;
/// Most files a skill bundle may hold.
const MAX_SKILL_BUNDLE_FILES: usize = 200;
/// Most bytes (decoded) across every file of a skill bundle.
const MAX_SKILL_BUNDLE_BYTES: usize = 4 * 1024 * 1024;
/// Most path components of a bundle file (`a/b/c` has three).
const MAX_SKILL_BUNDLE_DEPTH: usize = 8;
/// Longest bundle file path accepted on write.
const MAX_SKILL_BUNDLE_PATH_BYTES: usize = 1024;
/// Request body limit of the bundle write routes: the base64 of the largest
/// bundle plus room for 200 JSON-escaped paths and the framing.
pub const MAX_SKILL_BUNDLE_REQUEST_BYTES: usize =
    MAX_SKILL_BUNDLE_BYTES.div_ceil(3) * 4 + 2 * 1024 * 1024;
const MAX_CONFIG_BYTES: u64 = 2 * 1024 * 1024;
const MAX_SCAN_ENTRIES: usize = 2000;
const MAX_SCAN_VISITED_ENTRIES: usize = 20_000;
const MAX_SCAN_DEPTH: usize = 4;
/// Most (provider, workspace) scan results kept; the cache is emptied when
/// a new key would exceed it.
const MAX_CACHED_SCANS: usize = 256;
/// A scan that saw a file or directory modified this recently is not
/// cached: a further change within the same timestamp tick would leave its
/// stamp unchanged.
const RACY_STAMP_WINDOW: Duration = Duration::from_secs(2);
/// Most (path, mtime) pairs remembered for warn-once MCP config warnings.
const MAX_WARNED_CONFIGS: usize = 256;

#[derive(Clone)]
pub struct CatalogService {
    config: Arc<Config>,
    home: Option<PathBuf>,
    cache: Arc<ScanCache>,
}

type ScanKey = (ProviderKind, PathBuf);
type Validator = Vec<(PathBuf, Option<Stamp>)>;

/// Filesystem catalog scans per (provider, workspace), reused while every
/// file and directory the scan looked at is unchanged.
#[derive(Default)]
struct ScanCache {
    skills: Mutex<HashMap<ScanKey, Arc<CachedScan<Vec<SkillDescriptor>>>>>,
    mcp: Mutex<HashMap<ScanKey, Arc<CachedScan<Vec<McpServerDescriptor>>>>>,
    #[cfg(test)]
    builds: std::sync::atomic::AtomicUsize,
}

struct CachedScan<T> {
    /// Every path the scan read or listed, with what it looked like then
    /// (`None`: missing or unreadable).
    validator: Validator,
    value: T,
}

/// What a path looked like when a scan read it; any difference means the
/// scan has to run again.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct Stamp {
    modified: Option<SystemTime>,
    len: u64,
    directory: bool,
    symlink: bool,
}

impl Stamp {
    fn of(metadata: &std::fs::Metadata) -> Self {
        Self {
            modified: metadata.modified().ok(),
            len: metadata.len(),
            directory: metadata.is_dir(),
            symlink: metadata.file_type().is_symlink(),
        }
    }

    fn read(path: &Path) -> Option<Self> {
        std::fs::symlink_metadata(path)
            .ok()
            .map(|metadata| Self::of(&metadata))
    }
}

impl ScanCache {
    /// The cached result for `key` when every validator path still looks
    /// the same, else the result of `build` (cached unless it saw a racy
    /// stamp). Blocking: stats every validator path.
    fn get_or_build<T: Clone>(
        &self,
        map: &Mutex<HashMap<ScanKey, Arc<CachedScan<T>>>>,
        key: ScanKey,
        build: impl FnOnce(&mut Validator) -> Result<T, AppError>,
    ) -> Result<T, AppError> {
        let cached = map
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .get(&key)
            .cloned();
        if let Some(cached) = cached {
            if cached
                .validator
                .iter()
                .all(|(path, stamp)| Stamp::read(path) == *stamp)
            {
                return Ok(cached.value.clone());
            }
        }
        #[cfg(test)]
        self.builds
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let mut validator = Vec::new();
        let value = build(&mut validator)?;
        let horizon = SystemTime::now()
            .checked_sub(RACY_STAMP_WINDOW)
            .unwrap_or(SystemTime::UNIX_EPOCH);
        let settled = validator.iter().all(|(_, stamp)| {
            stamp.is_none_or(|stamp| stamp.modified.is_some_and(|modified| modified < horizon))
        });
        let mut map = map.lock().unwrap_or_else(PoisonError::into_inner);
        if settled {
            if map.len() >= MAX_CACHED_SCANS && !map.contains_key(&key) {
                map.clear();
            }
            map.insert(
                key,
                Arc::new(CachedScan {
                    validator,
                    value: value.clone(),
                }),
            );
        } else {
            map.remove(&key);
        }
        Ok(value)
    }

    /// Drops every cached scan; called after TodeX itself writes a skill or
    /// MCP config, which can be shared by several providers and workspaces.
    fn invalidate(&self) {
        self.skills
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clear();
        self.mcp
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clear();
    }
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
    /// Shipped and managed by the CLI itself (inside a hidden folder of a
    /// skill root, such as Codex's `skills/.system`); absent when false.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub built_in: bool,
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
    /// `dsh-mcp-client` rows of a DeepSeek Harness patch layer, shown and
    /// edited as entry JSON (see [`crate::dsh_mcp`]).
    DshPatch,
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

/// Every regular file of one skill's directory, for copying the skill to
/// another backend.
#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SkillBundle {
    pub resource_id: String,
    pub name: String,
    pub source: String,
    /// Sorted by `path`.
    pub files: Vec<SkillBundleFile>,
    /// Relative paths of symlinks, special files and non-UTF-8 names that
    /// were not read (a skipped directory's contents are not listed).
    pub skipped: Vec<String>,
}

/// One file of a [`SkillBundle`]; also the shape bundle writes accept.
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SkillBundleFile {
    /// Relative to the skill directory, `/`-separated.
    pub path: String,
    /// Standard base64 with padding.
    pub content_base64: String,
    /// Any execute bit set (Unix); written as 0755 instead of 0644.
    #[serde(default)]
    pub executable: bool,
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
            cache: Arc::default(),
        }
    }

    #[cfg(test)]
    pub fn with_home(config: Arc<Config>, home: Option<PathBuf>) -> Self {
        Self {
            config,
            home,
            cache: Arc::default(),
        }
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
        let cache = self.cache.clone();
        let skills = tokio::task::spawn_blocking(move || {
            cache.get_or_build(&cache.skills, (provider, workspace), |validator| {
                scan_skills(roots, validator)
            })
        })
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
        let cache = self.cache.clone();
        let servers = tokio::task::spawn_blocking(move || {
            cache.get_or_build(&cache.mcp, (provider, workspace.clone()), |validator| {
                scan_mcp(home.as_deref(), &workspace, provider, validator)
            })
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
            .mcp_descriptor(provider, workspace.clone(), resource_id)
            .await?;
        let origin = descriptor.origin.clone();
        let confine = (descriptor.scope == CatalogScope::Project).then(|| workspace.clone());
        let removed = tokio::task::spawn_blocking(move || {
            confine_project_file(&origin.path, confine.as_deref())?;
            remove_mcp_entry(&origin)
        })
        .await;
        self.cache.invalidate();
        removed.map_err(|error| AppError::Anyhow(error.into()))??;
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
        let confine = (source.scope == CatalogScope::Project).then(|| workspace.clone());
        let inserted = tokio::task::spawn_blocking(move || {
            confine_project_file(&path, confine.as_deref())?;
            insert_mcp_entry(&path, &entry_name, config)
        })
        .await;
        self.cache.invalidate();
        inserted.map_err(|error| AppError::Anyhow(error.into()))??;
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
        let confine = (descriptor.scope == CatalogScope::Project).then(|| workspace.clone());
        let replaced = tokio::task::spawn_blocking(move || {
            confine_project_file(&origin.path, confine.as_deref())?;
            replace_mcp_entry(&origin, &entry_name, config)
        })
        .await;
        self.cache.invalidate();
        replaced.map_err(|error| AppError::Anyhow(error.into()))??;
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
        let base = skill_write_base(&root, &workspace);
        let target = path.clone();
        let created = tokio::task::spawn_blocking(move || -> Result<(), AppError> {
            let directory = target.parent().expect("skill file has a parent");
            if std::fs::symlink_metadata(directory).is_ok() {
                return Err(AppError::Conflict(format!(
                    "{} already exists",
                    directory.display()
                )));
            }
            ensure_confined(directory, &base)?;
            std::fs::create_dir_all(directory)?;
            std::fs::write(&target, content)?;
            Ok(())
        })
        .await;
        self.cache.invalidate();
        created.map_err(|error| AppError::Anyhow(error.into()))??;
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
        let base = self.skill_write_base(provider, &workspace, &descriptor)?;
        let path = descriptor.path.clone();
        let replaced = tokio::task::spawn_blocking(move || {
            ensure_confined(&path, &base)?;
            replace_file(&path, content.as_bytes())
        })
        .await;
        self.cache.invalidate();
        replaced.map_err(|error| AppError::Anyhow(error.into()))??;
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
        let base = self.skill_write_base(provider, &workspace, &descriptor)?;
        let path = descriptor.path.clone();
        let removed = tokio::task::spawn_blocking(move || -> Result<PathBuf, AppError> {
            let directory = path
                .parent()
                .expect("skill file has a parent")
                .to_path_buf();
            if roots.iter().any(|root| root.path == directory) {
                ensure_confined(&path, &base)?;
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
            ensure_confined(&directory, &base)?;
            std::fs::remove_dir_all(&directory)?;
            Ok(directory)
        })
        .await;
        self.cache.invalidate();
        let removed = removed.map_err(|error| AppError::Anyhow(error.into()))??;
        Ok(SkillDeletion {
            resource_id: descriptor.resource_id,
            name: descriptor.name,
            path: removed.to_string_lossy().into_owned(),
        })
    }

    /// Every regular file in one skill's directory (just `SKILL.md` when it
    /// sits directly in a skill root). Symlinks and special files are listed
    /// in `skipped`, never followed.
    pub async fn skill_bundle(
        &self,
        provider: ProviderKind,
        workspace: PathBuf,
        resource_id: &str,
    ) -> Result<SkillBundle, AppError> {
        let descriptor = self
            .skill_descriptor(provider, workspace.clone(), resource_id)
            .await?;
        if !descriptor.valid {
            return Err(AppError::InvalidRequest(
                descriptor
                    .error
                    .clone()
                    .unwrap_or_else(|| "skill resource is not readable".to_owned()),
            ));
        }
        let root_level = self.is_root_level_skill(provider, &workspace, &descriptor);
        let path = descriptor.path.clone();
        let (files, skipped) =
            tokio::task::spawn_blocking(move || read_skill_bundle(&path, root_level))
                .await
                .map_err(|error| AppError::Anyhow(error.into()))??;
        Ok(SkillBundle {
            resource_id: descriptor.resource_id,
            name: descriptor.name,
            source: descriptor.source,
            files,
            skipped,
        })
    }

    /// Replaces one skill's directory with exactly `files`: they are written
    /// to a sibling staging directory that is then swapped in, so the skill
    /// is never seen half-written and nothing of the old directory remains.
    pub async fn replace_skill_bundle(
        &self,
        provider: ProviderKind,
        workspace: PathBuf,
        resource_id: &str,
        files: Vec<SkillBundleFile>,
    ) -> Result<SkillDescriptor, AppError> {
        let descriptor = self
            .writable_skill(provider, workspace.clone(), resource_id)
            .await?;
        let entries = decode_skill_bundle(files)?;
        if self.is_root_level_skill(provider, &workspace, &descriptor) {
            return Err(AppError::InvalidRequest(
                "this SKILL.md sits directly in a skill root, which has no skill directory to replace; edit it with PUT /v2/catalog/skills/{resourceId}".to_owned(),
            ));
        }
        let base = self.skill_write_base(provider, &workspace, &descriptor)?;
        let directory = descriptor
            .path
            .parent()
            .expect("skill file has a parent")
            .to_path_buf();
        let replaced = tokio::task::spawn_blocking(move || -> Result<(), AppError> {
            let metadata = std::fs::symlink_metadata(&directory)?;
            if metadata.file_type().is_symlink() || !metadata.is_dir() {
                return Err(AppError::InvalidRequest(format!(
                    "{} is not a regular directory",
                    directory.display()
                )));
            }
            ensure_confined(&directory, &base)?;
            replace_skill_directory(&directory, &entries)
        })
        .await;
        self.cache.invalidate();
        replaced.map_err(|error| AppError::Anyhow(error.into()))??;
        self.skill_descriptor(provider, workspace, resource_id)
            .await
    }

    /// Creates `<root>/<name>/` under the skill root of `source` holding
    /// exactly `files`, which must include a valid `SKILL.md`.
    pub async fn create_skill_bundle(
        &self,
        provider: ProviderKind,
        workspace: PathBuf,
        source: &str,
        name: &str,
        files: Vec<SkillBundleFile>,
    ) -> Result<SkillDescriptor, AppError> {
        if catalog_profile(provider).source == CatalogSource::NativeInspect {
            return Err(AppError::Unsupported(format!(
                "{} skills are managed by the provider itself",
                crate::provider::profile::profile(provider).display_name
            )));
        }
        validate_skill_directory_name(name)?;
        let entries = decode_skill_bundle(files)?;
        let root = skill_roots(self.home.as_deref(), &workspace, provider)
            .into_iter()
            .find(|root| root.source == source)
            .ok_or_else(|| AppError::InvalidRequest(format!("unknown skill source {source}")))?;
        let directory = root.path.join(name);
        let path = directory.join("SKILL.md");
        let base = skill_write_base(&root, &workspace);
        let created = tokio::task::spawn_blocking(move || -> Result<(), AppError> {
            if std::fs::symlink_metadata(&directory).is_ok() {
                return Err(AppError::Conflict(format!(
                    "{} already exists",
                    directory.display()
                )));
            }
            ensure_confined(&directory, &base)?;
            create_skill_directory(&directory, &entries)
        })
        .await;
        self.cache.invalidate();
        created.map_err(|error| AppError::Anyhow(error.into()))??;
        self.skill_descriptor(provider, workspace, &resource_id("skill", &path))
            .await
    }

    /// Whether `descriptor`'s `SKILL.md` sits directly in one of the skill
    /// roots, so its parent directory holds other skills too.
    fn is_root_level_skill(
        &self,
        provider: ProviderKind,
        workspace: &Path,
        descriptor: &SkillDescriptor,
    ) -> bool {
        let Some(directory) = descriptor.path.parent() else {
            return true;
        };
        skill_roots(self.home.as_deref(), workspace, provider)
            .iter()
            .any(|root| root.path == directory)
    }

    /// The directory a write to `descriptor`'s skill must stay inside once
    /// symlinks are resolved; see [`skill_write_base`].
    fn skill_write_base(
        &self,
        provider: ProviderKind,
        workspace: &Path,
        descriptor: &SkillDescriptor,
    ) -> Result<PathBuf, AppError> {
        skill_roots(self.home.as_deref(), workspace, provider)
            .into_iter()
            .find(|root| {
                root.source == descriptor.source && descriptor.path.starts_with(&root.path)
            })
            .map(|root| skill_write_base(&root, workspace))
            .ok_or_else(|| {
                AppError::InvalidRequest(format!(
                    "{} is not in a writable skill root",
                    descriptor.path.display()
                ))
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

/// Project skills must stay inside the workspace and user skills inside
/// their skill root once symlinks are resolved, so a repository shipping
/// e.g. `.claude -> /elsewhere` cannot redirect skill writes or deletions.
/// The user's own skill root may itself be a symlink.
fn skill_write_base(root: &SourceRoot, workspace: &Path) -> PathBuf {
    match root.scope {
        CatalogScope::Project => workspace.to_path_buf(),
        CatalogScope::User => root.path.clone(),
    }
}

/// Fails unless `path` resolves (through every symlink in its existing
/// part) to somewhere inside `base`, compared component-wise.
/// Project-scope MCP config files are written only inside `workspace`
/// (`None`: a user-level file, not confined), like skill files.
fn confine_project_file(path: &Path, workspace: Option<&Path>) -> Result<(), AppError> {
    match workspace {
        Some(workspace) => ensure_confined(path, workspace),
        None => Ok(()),
    }
}

fn ensure_confined(path: &Path, base: &Path) -> Result<(), AppError> {
    let resolved = resolve_existing_prefix(path)?;
    let base = resolve_existing_prefix(base)?;
    if resolved.starts_with(&base) {
        Ok(())
    } else {
        Err(AppError::InvalidRequest(format!(
            "{} resolves outside {}",
            path.display(),
            base.display()
        )))
    }
}

/// Canonicalizes the longest existing ancestor of `path` and appends the
/// components that do not exist yet.
fn resolve_existing_prefix(path: &Path) -> Result<PathBuf, AppError> {
    let mut existing = path;
    let mut missing = Vec::new();
    loop {
        match std::fs::symlink_metadata(existing) {
            Ok(_) => break,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                let (Some(name), Some(parent)) = (existing.file_name(), existing.parent()) else {
                    return Err(AppError::InvalidRequest(format!(
                        "{} cannot be resolved",
                        path.display()
                    )));
                };
                missing.push(name);
                existing = parent;
            }
            Err(error) => return Err(error.into()),
        }
    }
    let mut resolved = std::fs::canonicalize(existing)?;
    resolved.extend(missing.into_iter().rev());
    Ok(resolved)
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

/// A `SKILL.md` inside a hidden folder of its root (Codex's `skills/.system`)
/// belongs to the CLI, which reinstalls it on its own.
fn is_built_in_skill(root: &Path, skill_file: &Path) -> bool {
    skill_file
        .parent()
        .and_then(|directory| directory.strip_prefix(root).ok())
        .is_some_and(|relative| {
            relative.components().any(|component| {
                component
                    .as_os_str()
                    .to_str()
                    .is_some_and(|name| name.starts_with('.'))
            })
        })
}

/// Lists the skills under `roots`, recording in `validator` every directory
/// it listed and every `SKILL.md` it read (stamped before reading).
fn scan_skills(
    mut roots: Vec<SourceRoot>,
    validator: &mut Validator,
) -> Result<Vec<SkillDescriptor>, AppError> {
    roots.sort_by_key(|root| std::cmp::Reverse(root.priority));
    let mut skills = Vec::new();
    let mut seen_paths = HashSet::new();
    let mut visited_entries = 0;
    for root in roots {
        let mut files = Vec::new();
        collect_named_files(
            &root.path,
            "SKILL.md",
            0,
            &mut files,
            &mut visited_entries,
            validator,
        )?;
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
            let built_in = is_built_in_skill(&root.path, &path);
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
                    built_in,
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
                    built_in,
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
    validator: &mut Validator,
) -> Result<(), AppError> {
    if depth > MAX_SCAN_DEPTH
        || output.len() >= MAX_SCAN_ENTRIES
        || *visited_entries >= MAX_SCAN_VISITED_ENTRIES
    {
        return Ok(());
    }
    let metadata = match std::fs::symlink_metadata(directory) {
        Ok(metadata) => metadata,
        Err(error)
            if matches!(
                error.kind(),
                std::io::ErrorKind::NotFound | std::io::ErrorKind::PermissionDenied
            ) =>
        {
            validator.push((directory.to_path_buf(), None));
            return Ok(());
        }
        Err(error) => return Err(error.into()),
    };
    validator.push((directory.to_path_buf(), Some(Stamp::of(&metadata))));
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
            collect_named_files(
                &entry.path(),
                name,
                depth + 1,
                output,
                visited_entries,
                validator,
            )?;
        } else if file_type.is_file() && entry.file_name() == name {
            let path = entry.path();
            validator.push((path.clone(), Stamp::read(&path)));
            output.push(path);
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

/// Lists the MCP servers declared in `provider`'s config files, recording
/// in `validator` each file it looked at (stamped before reading).
fn scan_mcp(
    home: Option<&Path>,
    workspace: &Path,
    provider: ProviderKind,
    validator: &mut Validator,
) -> Result<Vec<McpServerDescriptor>, AppError> {
    let mut descriptors = Vec::new();
    for source in mcp_sources(home, workspace, provider) {
        let metadata = match std::fs::symlink_metadata(&source.path) {
            Ok(metadata) => metadata,
            Err(error)
                if matches!(
                    error.kind(),
                    std::io::ErrorKind::NotFound | std::io::ErrorKind::PermissionDenied
                ) =>
            {
                validator.push((source.path.clone(), None));
                continue;
            }
            Err(error) => return Err(error.into()),
        };
        validator.push((source.path.clone(), Some(Stamp::of(&metadata))));
        // A project file reached through a symlinked parent (a repository's
        // `.codex -> ~/.codex`) is not this workspace's config.
        if source.scope == CatalogScope::Project
            && ensure_confined(&source.path, workspace).is_err()
        {
            continue;
        }
        let modified = metadata.modified().ok();
        if metadata.file_type().is_symlink()
            || !metadata.is_file()
            || metadata.len() > MAX_CONFIG_BYTES
        {
            continue;
        }
        let raw = match std::fs::read_to_string(&source.path) {
            Ok(raw) => raw,
            Err(error) => {
                if first_config_warning(&source.path, modified) {
                    tracing::warn!(path = %source.path.display(), error = %error, "skipping unreadable MCP config");
                }
                continue;
            }
        };
        let format = file_format(&source.path);
        let parsed = match format {
            McpConfigFormat::Toml => parse_toml_mcp(&raw),
            McpConfigFormat::DshPatch => parse_dsh_mcp(&raw),
            _ => parse_json_mcp(&raw),
        };
        let entries = match parsed {
            Ok(entries) => entries,
            Err(error) => {
                if first_config_warning(&source.path, modified) {
                    tracing::warn!(path = %source.path.display(), error = %error, "skipping invalid MCP config");
                }
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

/// Whether a broken MCP config at `path` with this mtime has not been
/// warned about yet, so each version of a broken file is logged once rather
/// than on every scan.
fn first_config_warning(path: &Path, modified: Option<SystemTime>) -> bool {
    type Warned = HashSet<(PathBuf, Option<SystemTime>)>;
    static WARNED: LazyLock<Mutex<Warned>> = LazyLock::new(Mutex::default);
    let mut warned = WARNED.lock().unwrap_or_else(PoisonError::into_inner);
    let key = (path.to_path_buf(), modified);
    if warned.contains(&key) {
        return false;
    }
    if warned.len() >= MAX_WARNED_CONFIGS {
        warned.clear();
    }
    warned.insert(key)
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
            // Join the `/`-separated relative path segment by segment so the
            // listed path uses native separators on Windows.
            let (base, relative) = match file {
                UserConfigFile::Home(relative) => (home, relative),
                UserConfigFile::ConfigHome(relative) => (config_home.as_path(), relative),
            };
            let path = relative
                .split('/')
                .fold(base.to_path_buf(), |path, segment| path.join(segment));
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
                built_in: false,
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

/// An empty or whitespace-only file declares no servers.
fn parse_toml_mcp(raw: &str) -> Result<Vec<ParsedMcpServer>, AppError> {
    if raw.trim().is_empty() {
        return Ok(Vec::new());
    }
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

/// An empty or whitespace-only file declares no servers.
/// How the MCP config file at `path` is written, by its extension.
fn file_format(path: &Path) -> McpConfigFormat {
    match path.extension().and_then(|value| value.to_str()) {
        Some("toml") => McpConfigFormat::Toml,
        Some("yml" | "yaml") => McpConfigFormat::DshPatch,
        _ => McpConfigFormat::Json,
    }
}

fn parse_dsh_mcp(raw: &str) -> Result<Vec<ParsedMcpServer>, AppError> {
    Ok(crate::dsh_mcp::parse(raw)?
        .into_iter()
        .map(|server| ParsedMcpServer {
            name: server.name,
            key: server.row_id,
            map_pointer: String::new(),
            raw: server.entry,
            transport: if server.http {
                McpTransport::Http
            } else {
                McpTransport::Stdio
            },
            enabled: server.enabled,
            command: server.command,
            env: server.env,
            url: server.url,
            headers: server.headers,
        })
        .collect())
}

fn parse_json_mcp(raw: &str) -> Result<Vec<ParsedMcpServer>, AppError> {
    if raw.trim().is_empty() {
        return Ok(Vec::new());
    }
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
    if origin.format == McpConfigFormat::DshPatch {
        return crate::dsh_mcp::remove(&origin.path, &origin.key);
    }
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
        McpConfigFormat::Native | McpConfigFormat::DshPatch => {
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
    // Config files can hold credentials: the temp file is created 0600
    // before any byte is written and only then given the target's mode.
    crate::secure_fs::create_owner_only(&temp, contents)?;
    let result = match permissions {
        Some(permissions) => std::fs::set_permissions(&temp, permissions),
        None => Ok(()),
    }
    .and_then(|()| std::fs::rename(&temp, path));
    if result.is_err() {
        let _ = std::fs::remove_file(&temp);
    }
    result.map_err(Into::into)
}

/// Adds `name` to the config file at `path` (top-level `mcpServers` for
/// JSON, `[mcp_servers]` for TOML), creating the file when missing.
fn insert_mcp_entry(path: &Path, name: &str, config: Value) -> Result<(), AppError> {
    if file_format(path) == McpConfigFormat::DshPatch {
        return crate::dsh_mcp::insert(path, name, &config);
    }
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
    if origin.format == McpConfigFormat::DshPatch {
        return crate::dsh_mcp::replace(&origin.path, &origin.key, name, &config);
    }
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
        McpConfigFormat::Native | McpConfigFormat::DshPatch => {
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

fn bundle_too_large(detail: String) -> AppError {
    AppError::PayloadTooLarge(format!(
        "{detail}; a skill bundle holds at most {MAX_SKILL_BUNDLE_FILES} files, {MAX_SKILL_BUNDLE_BYTES} bytes in total and {MAX_SKILL_BUNDLE_DEPTH} path components per file"
    ))
}

/// Reads a skill's bundle: the whole directory holding `skill_file`, or
/// only `skill_file` when it sits directly in a skill root. Returns the
/// files and the skipped paths, both sorted.
fn read_skill_bundle(
    skill_file: &Path,
    root_level: bool,
) -> Result<(Vec<SkillBundleFile>, Vec<String>), AppError> {
    let mut reader = BundleReader::default();
    if root_level {
        let metadata = std::fs::symlink_metadata(skill_file)?;
        if metadata.file_type().is_symlink() || !metadata.is_file() {
            return Err(AppError::InvalidRequest(
                "catalog resource must be a regular file".to_owned(),
            ));
        }
        reader.read_file(skill_file, "SKILL.md".to_owned())?;
    } else {
        let directory = skill_file.parent().expect("skill file has a parent");
        let metadata = std::fs::symlink_metadata(directory)?;
        if metadata.file_type().is_symlink() || !metadata.is_dir() {
            return Err(AppError::InvalidRequest(format!(
                "{} is not a regular directory",
                directory.display()
            )));
        }
        reader.walk(directory, "", 0)?;
    }
    reader
        .files
        .sort_by(|left, right| left.path.cmp(&right.path));
    reader.skipped.sort();
    Ok((reader.files, reader.skipped))
}

#[derive(Default)]
struct BundleReader {
    files: Vec<SkillBundleFile>,
    skipped: Vec<String>,
    total_bytes: usize,
}

impl BundleReader {
    /// Reads `directory`, whose relative path is `prefix` (`""` for the
    /// skill directory) made of `depth` components.
    fn walk(&mut self, directory: &Path, prefix: &str, depth: usize) -> Result<(), AppError> {
        for entry in std::fs::read_dir(directory)? {
            let entry = entry?;
            let file_name = entry.file_name();
            let join = |name: &str| {
                if prefix.is_empty() {
                    name.to_owned()
                } else {
                    format!("{prefix}/{name}")
                }
            };
            let Some(name) = file_name.to_str() else {
                self.skipped.push(join(&file_name.to_string_lossy()));
                continue;
            };
            let relative = join(name);
            // `DirEntry::file_type` does not follow symlinks.
            let file_type = entry.file_type()?;
            if file_type.is_symlink() || !(file_type.is_file() || file_type.is_dir()) {
                self.skipped.push(relative);
                continue;
            }
            if depth + 1 > MAX_SKILL_BUNDLE_DEPTH {
                return Err(bundle_too_large(format!(
                    "{relative} is nested more than {MAX_SKILL_BUNDLE_DEPTH} levels deep"
                )));
            }
            if file_type.is_dir() {
                self.walk(&entry.path(), &relative, depth + 1)?;
            } else {
                self.read_file(&entry.path(), relative)?;
            }
        }
        Ok(())
    }

    fn read_file(&mut self, path: &Path, relative: String) -> Result<(), AppError> {
        use std::io::Read as _;

        if self.files.len() >= MAX_SKILL_BUNDLE_FILES {
            return Err(bundle_too_large(format!(
                "the skill has more than {MAX_SKILL_BUNDLE_FILES} files"
            )));
        }
        let mut options = std::fs::OpenOptions::new();
        options.read(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            // The entry was a regular file when listed; do not follow a
            // symlink swapped in since.
            options.custom_flags(libc::O_NOFOLLOW);
        }
        let file = options.open(path)?;
        let metadata = file.metadata()?;
        if !metadata.is_file() {
            self.skipped.push(relative);
            return Ok(());
        }
        let remaining = MAX_SKILL_BUNDLE_BYTES - self.total_bytes;
        let mut contents = Vec::new();
        file.take(remaining as u64 + 1).read_to_end(&mut contents)?;
        if contents.len() > remaining {
            return Err(bundle_too_large(format!(
                "the skill's files exceed {MAX_SKILL_BUNDLE_BYTES} bytes"
            )));
        }
        self.total_bytes += contents.len();
        #[cfg(unix)]
        let executable = {
            use std::os::unix::fs::PermissionsExt;
            metadata.permissions().mode() & 0o111 != 0
        };
        #[cfg(not(unix))]
        let executable = false;
        self.files.push(SkillBundleFile {
            path: relative,
            content_base64: STANDARD.encode(&contents),
            executable,
        });
        Ok(())
    }
}

/// A validated, decoded bundle file ready to be written.
struct BundleEntry {
    path: String,
    contents: Vec<u8>,
    /// Applied as the file mode on Unix; Windows has no executable bit.
    #[cfg_attr(not(unix), allow(dead_code))]
    executable: bool,
}

/// Validates a bundle write: file count and size limits, safe unique
/// paths, base64 contents, and a root `SKILL.md` that passes the same
/// checks as a single `SKILL.md` write.
fn decode_skill_bundle(files: Vec<SkillBundleFile>) -> Result<Vec<BundleEntry>, AppError> {
    if files.len() > MAX_SKILL_BUNDLE_FILES {
        return Err(bundle_too_large(format!(
            "the bundle has {} files",
            files.len()
        )));
    }
    let mut paths = HashSet::new();
    let mut entries = Vec::with_capacity(files.len());
    let mut total_bytes = 0usize;
    for file in files {
        validate_bundle_path(&file.path)?;
        if !paths.insert(file.path.clone()) {
            return Err(AppError::InvalidRequest(format!(
                "bundle path {:?} appears more than once",
                file.path
            )));
        }
        // Base64 decodes to at most 3/4 of its length.
        if file.content_base64.len() / 4 * 3 > MAX_SKILL_BUNDLE_BYTES + 2 {
            return Err(bundle_too_large(format!(
                "{} exceeds {MAX_SKILL_BUNDLE_BYTES} bytes",
                file.path
            )));
        }
        let contents = STANDARD.decode(&file.content_base64).map_err(|error| {
            AppError::InvalidRequest(format!(
                "contentBase64 of {:?} is not valid base64: {error}",
                file.path
            ))
        })?;
        total_bytes += contents.len();
        if total_bytes > MAX_SKILL_BUNDLE_BYTES {
            return Err(bundle_too_large(format!(
                "the bundle's files exceed {MAX_SKILL_BUNDLE_BYTES} bytes"
            )));
        }
        entries.push(BundleEntry {
            path: file.path,
            contents,
            executable: file.executable,
        });
    }
    for path in &paths {
        for (index, _) in path.match_indices('/') {
            if paths.contains(&path[..index]) {
                return Err(AppError::InvalidRequest(format!(
                    "bundle path {:?} is both a file and a directory",
                    &path[..index]
                )));
            }
        }
    }
    let skill = entries
        .iter()
        .find(|entry| entry.path == "SKILL.md")
        .ok_or_else(|| {
            AppError::InvalidRequest("a skill bundle must contain SKILL.md at its root".to_owned())
        })?;
    let content = std::str::from_utf8(&skill.contents)
        .map_err(|_| AppError::InvalidRequest("SKILL.md is not valid UTF-8".to_owned()))?;
    validate_skill_content(content)?;
    Ok(entries)
}

/// Bundle paths are relative, `/`-separated and made only of plain
/// components, so a write can never leave the skill directory.
fn validate_bundle_path(path: &str) -> Result<(), AppError> {
    let invalid =
        |reason: &str| AppError::InvalidRequest(format!("invalid bundle path {path:?}: {reason}"));
    if path.is_empty() {
        return Err(invalid("it is empty"));
    }
    if path.len() > MAX_SKILL_BUNDLE_PATH_BYTES {
        return Err(invalid(&format!(
            "it is longer than {MAX_SKILL_BUNDLE_PATH_BYTES} bytes"
        )));
    }
    if path.contains('\\') || path.contains('\0') {
        return Err(invalid("it contains '\\' or NUL"));
    }
    if path.starts_with('/') {
        return Err(invalid("it must be relative"));
    }
    let components = path.split('/').collect::<Vec<_>>();
    if components.len() > MAX_SKILL_BUNDLE_DEPTH {
        return Err(invalid(&format!(
            "it has more than {MAX_SKILL_BUNDLE_DEPTH} components"
        )));
    }
    if components
        .iter()
        .any(|component| component.is_empty() || *component == "." || *component == "..")
    {
        return Err(invalid("it has an empty, '.' or '..' component"));
    }
    let first = components[0].as_bytes();
    if first.len() >= 2 && first[0].is_ascii_alphabetic() && first[1] == b':' {
        return Err(invalid("it starts with a drive prefix"));
    }
    // Whatever else this platform treats as special (e.g. Windows prefixes).
    let parsed = Path::new(path).components().collect::<Vec<_>>();
    if parsed.len() != components.len()
        || !parsed
            .iter()
            .all(|component| matches!(component, std::path::Component::Normal(_)))
    {
        return Err(invalid("it is not a plain relative path"));
    }
    Ok(())
}

/// Writes `entries` into the empty directory `directory`: 0644 files, 0755
/// when executable (Unix; set explicitly so the umask does not apply).
fn write_bundle_entries(directory: &Path, entries: &[BundleEntry]) -> Result<(), AppError> {
    use std::io::Write as _;

    for entry in entries {
        let target = directory.join(&entry.path);
        if let Some(parent) = target.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let mut options = std::fs::OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        let mode = if entry.executable { 0o755 } else { 0o644 };
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(mode);
        }
        let mut file = options.open(&target).map_err(|error| {
            if error.kind() == std::io::ErrorKind::AlreadyExists {
                AppError::InvalidRequest(format!(
                    "bundle path {:?} collides with another bundle file on this filesystem",
                    entry.path
                ))
            } else {
                error.into()
            }
        })?;
        file.write_all(&entry.contents)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            file.set_permissions(std::fs::Permissions::from_mode(mode))?;
        }
    }
    Ok(())
}

/// A fresh staging directory next to `directory`, holding `entries`.
fn stage_skill_directory(directory: &Path, entries: &[BundleEntry]) -> Result<PathBuf, AppError> {
    let (Some(parent), Some(name)) = (directory.parent(), directory.file_name()) else {
        return Err(AppError::InvalidRequest(format!(
            "{} has no parent directory",
            directory.display()
        )));
    };
    let staging = parent.join(format!(
        ".{}.todex-{}",
        name.to_string_lossy(),
        uuid::Uuid::new_v4().simple()
    ));
    std::fs::create_dir(&staging)?;
    if let Err(error) = write_bundle_entries(&staging, entries) {
        let _ = std::fs::remove_dir_all(&staging);
        return Err(error);
    }
    Ok(staging)
}

/// Swaps the existing `directory` for one holding exactly `entries`: the
/// old directory is renamed aside, the staged one renamed in, and the old
/// one removed; the old one is put back when the swap fails.
fn replace_skill_directory(directory: &Path, entries: &[BundleEntry]) -> Result<(), AppError> {
    let staging = stage_skill_directory(directory, entries)?;
    let name = directory
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_default();
    let retired = directory.with_file_name(format!(
        ".{name}.todex-old-{}",
        uuid::Uuid::new_v4().simple()
    ));
    if let Err(error) = std::fs::rename(directory, &retired) {
        let _ = std::fs::remove_dir_all(&staging);
        return Err(error.into());
    }
    if let Err(error) = std::fs::rename(&staging, directory) {
        if let Err(restore) = std::fs::rename(&retired, directory) {
            tracing::error!(
                path = %directory.display(),
                retired = %retired.display(),
                error = %restore,
                "could not restore a skill directory after a failed bundle write"
            );
        }
        let _ = std::fs::remove_dir_all(&staging);
        return Err(error.into());
    }
    if let Err(error) = std::fs::remove_dir_all(&retired) {
        tracing::warn!(
            path = %retired.display(),
            error = %error,
            "could not remove a replaced skill directory"
        );
    }
    Ok(())
}

/// Creates `directory` (which must not exist) holding exactly `entries`,
/// creating missing parents such as the skill root itself.
fn create_skill_directory(directory: &Path, entries: &[BundleEntry]) -> Result<(), AppError> {
    if let Some(parent) = directory.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let staging = stage_skill_directory(directory, entries)?;
    let conflict = || AppError::Conflict(format!("{} already exists", directory.display()));
    if std::fs::symlink_metadata(directory).is_ok() {
        let _ = std::fs::remove_dir_all(&staging);
        return Err(conflict());
    }
    if let Err(error) = std::fs::rename(&staging, directory) {
        let _ = std::fs::remove_dir_all(&staging);
        return Err(if std::fs::symlink_metadata(directory).is_ok() {
            conflict()
        } else {
            error.into()
        });
    }
    Ok(())
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
    async fn dsh_mcp_servers_are_rows_of_the_acp_profile_patch() {
        let root = temp_dir("todex-catalog-dsh");
        let home = root.join("home");
        let workspace_root = root.join("workspaces");
        let workspace = workspace_root.join("project");
        fs::create_dir_all(&workspace).unwrap();
        let config = Arc::new(test_config(root.join("data"), workspace_root));
        let service = CatalogService::with_home(config, Some(home.clone()));
        let provider = ProviderKind::DeepseekHarness;

        let catalog = service.mcp(provider, workspace.clone()).await.unwrap();
        assert!(catalog.editable);
        assert_eq!(catalog.creatable_scopes, vec![CatalogScope::User]);
        let created = service
            .create_mcp(
                provider,
                workspace.clone(),
                CatalogScope::User,
                "docs".to_owned(),
                json!({ "command": "/usr/bin/docs-mcp", "args": ["--stdio"] }),
            )
            .await
            .unwrap();
        let patch = home
            .join(".dsh")
            .join("profiles")
            .join("acp")
            .join("cordis.patch.yml");
        assert_eq!(created.path, patch.to_string_lossy());
        assert_eq!(created.format, McpConfigFormat::DshPatch);
        assert_eq!(
            created.config,
            json!({ "type": "stdio", "command": "/usr/bin/docs-mcp", "args": ["--stdio"] })
        );
        assert!(fs::read_to_string(&patch)
            .unwrap()
            .contains("name: '@deepseek-ai/dsh-mcp-client'"));
        let updated = service
            .update_mcp(
                provider,
                workspace.clone(),
                &created.descriptor.resource_id,
                "docs".to_owned(),
                json!({ "type": "http", "url": "https://docs.test/mcp", "enabled": false }),
            )
            .await
            .unwrap();
        assert_eq!(updated.descriptor.transport, McpTransport::Http);
        assert!(!updated.descriptor.enabled);
        assert!(service
            .create_mcp(
                provider,
                workspace.clone(),
                CatalogScope::User,
                "todex_ssh".to_owned(),
                json!({ "command": "/bin/x" }),
            )
            .await
            .is_err());
        service
            .delete_mcp(provider, workspace.clone(), &updated.descriptor.resource_id)
            .await
            .unwrap();
        assert!(service
            .mcp(provider, workspace)
            .await
            .unwrap()
            .servers
            .is_empty());
        assert_eq!(fs::read_to_string(&patch).unwrap(), "[]\n");
        let _ = fs::remove_dir_all(root);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn project_mcp_configs_behind_a_symlinked_directory_are_left_alone() {
        let root = temp_dir("todex-catalog-mcp-escape");
        let home = root.join("home");
        let workspace_root = root.join("workspaces");
        let workspace = workspace_root.join("project");
        let outside = root.join("outside");
        fs::create_dir_all(&workspace).unwrap();
        fs::create_dir_all(&home).unwrap();
        fs::create_dir_all(&outside).unwrap();
        let foreign = "[mcp_servers.foreign]\ncommand = \"evil\"\n";
        fs::write(outside.join("config.toml"), foreign).unwrap();
        // A repository can ship `.codex` as a link to any directory.
        std::os::unix::fs::symlink(&outside, workspace.join(".codex")).unwrap();
        let config = Arc::new(test_config(root.join("data"), workspace_root));
        let service = CatalogService::with_home(config, Some(home));

        let catalog = service
            .mcp(ProviderKind::Codex, workspace.clone())
            .await
            .unwrap();
        assert!(
            catalog
                .servers
                .iter()
                .all(|server| server.name != "foreign"),
            "{:?}",
            catalog.servers
        );
        let created = service
            .create_mcp(
                ProviderKind::Codex,
                workspace.clone(),
                CatalogScope::Project,
                "docs".to_owned(),
                json!({ "command": "npx", "args": ["docs-mcp"] }),
            )
            .await;
        assert!(
            matches!(&created, Err(AppError::InvalidRequest(message)) if message.contains("resolves outside")),
            "{created:?}"
        );
        assert_eq!(
            fs::read_to_string(outside.join("config.toml")).unwrap(),
            foreign
        );
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

    #[test]
    fn empty_mcp_configs_declare_nothing_and_broken_ones_warn_once() {
        for raw in ["", "  \n\t\n"] {
            assert!(parse_json_mcp(raw).unwrap().is_empty());
            assert!(parse_toml_mcp(raw).unwrap().is_empty());
        }
        assert!(parse_json_mcp("{ malformed").is_err());
        assert!(parse_toml_mcp("[mcp_servers").is_err());

        let path = temp_dir("todex-catalog-warn").join(".mcp.json");
        let first = Some(SystemTime::UNIX_EPOCH + Duration::from_secs(1));
        let second = Some(SystemTime::UNIX_EPOCH + Duration::from_secs(2));
        assert!(first_config_warning(&path, first));
        assert!(!first_config_warning(&path, first));
        assert!(first_config_warning(&path, second));
        assert!(!first_config_warning(&path, second));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn skill_writes_cannot_escape_through_workspace_symlinks() {
        let root = temp_dir("todex-catalog-escape");
        let home = root.join("home");
        let workspace_root = root.join("workspaces");
        let workspace = workspace_root.join("project");
        let outside = root.join("outside");
        let victim = outside.join("skills/victim");
        fs::create_dir_all(&workspace).unwrap();
        fs::create_dir_all(&home).unwrap();
        fs::create_dir_all(&victim).unwrap();
        let victim_content = "---\nname: victim\ndescription: outside\n---\n";
        fs::write(victim.join("SKILL.md"), victim_content).unwrap();
        // The repository ships `.claude` pointing outside the workspace.
        std::os::unix::fs::symlink(&outside, workspace.join(".claude")).unwrap();
        let config = Arc::new(test_config(root.join("data"), workspace_root));
        let service = CatalogService::with_home(config, Some(home));

        let content = "---\nname: planted\ndescription: x\n---\n";
        assert!(matches!(
            service
                .create_skill(
                    ProviderKind::ClaudeCode,
                    workspace.clone(),
                    "provider-project",
                    "planted",
                    content.to_owned(),
                )
                .await,
            Err(AppError::InvalidRequest(_))
        ));
        assert!(!outside.join("skills/planted").exists());

        let listed = service
            .skills(ProviderKind::ClaudeCode, workspace.clone())
            .await
            .unwrap()
            .skills
            .into_iter()
            .find(|skill| skill.name == "victim")
            .expect("the scan reads through the symlinked directory");
        assert!(matches!(
            service
                .update_skill(
                    ProviderKind::ClaudeCode,
                    workspace.clone(),
                    &listed.resource_id,
                    content.to_owned(),
                )
                .await,
            Err(AppError::InvalidRequest(_))
        ));
        assert!(matches!(
            service
                .delete_skill(
                    ProviderKind::ClaudeCode,
                    workspace.clone(),
                    &listed.resource_id
                )
                .await,
            Err(AppError::InvalidRequest(_))
        ));
        assert_eq!(
            fs::read_to_string(victim.join("SKILL.md")).unwrap(),
            victim_content
        );

        // A skill root inside the workspace still works.
        let created = service
            .create_skill(
                ProviderKind::ClaudeCode,
                workspace.clone(),
                "shared-project",
                "inside",
                content.to_owned(),
            )
            .await
            .unwrap();
        service
            .delete_skill(
                ProviderKind::ClaudeCode,
                workspace.clone(),
                &created.resource_id,
            )
            .await
            .unwrap();
        assert!(!workspace.join(".agents/skills/inside").exists());
        let _ = fs::remove_dir_all(root);
    }

    #[cfg(unix)]
    #[test]
    fn config_writes_use_owner_only_temp_files_and_keep_existing_modes() {
        use std::os::unix::fs::PermissionsExt as _;
        let root = temp_dir("todex-catalog-temp");
        fs::create_dir_all(&root).unwrap();
        let mode = |path: &Path| fs::metadata(path).unwrap().permissions().mode() & 0o777;

        let created = root.join("config.toml");
        insert_mcp_entry(&created, "tools", json!({ "command": "x" })).unwrap();
        assert_eq!(mode(&created), 0o600);

        let existing = root.join(".claude.json");
        fs::write(&existing, "{}\n").unwrap();
        fs::set_permissions(&existing, fs::Permissions::from_mode(0o640)).unwrap();
        insert_mcp_entry(&existing, "tools", json!({ "command": "x" })).unwrap();
        assert_eq!(mode(&existing), 0o640);
        assert!(fs::read_to_string(&existing).unwrap().contains("tools"));

        // No temp file is left behind.
        let names = fs::read_dir(&root)
            .unwrap()
            .map(|entry| entry.unwrap().file_name())
            .collect::<Vec<_>>();
        assert_eq!(names.len(), 2, "no temp files left: {names:?}");
        let _ = fs::remove_dir_all(root);
    }

    /// Moves every mtime under `root` into the past, so the next scan is not
    /// racy and gets cached.
    fn settle(root: &Path) {
        let past = SystemTime::now() - Duration::from_secs(60);
        let metadata = fs::symlink_metadata(root).unwrap();
        if metadata.file_type().is_symlink() {
            return;
        }
        if metadata.is_dir() {
            for entry in fs::read_dir(root).unwrap() {
                settle(&entry.unwrap().path());
            }
        }
        open_to_set_times(root).set_modified(past).unwrap();
    }

    #[cfg(not(windows))]
    fn open_to_set_times(path: &Path) -> fs::File {
        fs::File::open(path).unwrap()
    }

    /// Windows needs write-attribute access to change times, and backup
    /// semantics to open a directory at all.
    #[cfg(windows)]
    fn open_to_set_times(path: &Path) -> fs::File {
        use std::os::windows::fs::OpenOptionsExt as _;
        const FILE_WRITE_ATTRIBUTES: u32 = 0x0100;
        const FILE_FLAG_BACKUP_SEMANTICS: u32 = 0x0200_0000;
        fs::OpenOptions::new()
            .access_mode(FILE_WRITE_ATTRIBUTES)
            .custom_flags(FILE_FLAG_BACKUP_SEMANTICS)
            .open(path)
            .unwrap()
    }

    fn builds(service: &CatalogService) -> usize {
        service
            .cache
            .builds
            .load(std::sync::atomic::Ordering::Relaxed)
    }

    #[tokio::test]
    async fn catalog_scans_are_cached_until_a_source_changes() {
        let root = temp_dir("todex-catalog-cache");
        let home = root.join("home");
        let workspace_root = root.join("workspaces");
        let workspace = workspace_root.join("project");
        let skill = |name: &str, description: &str| {
            let path = workspace.join(format!(".claude/skills/{name}/SKILL.md"));
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(
                path,
                format!("---\nname: {name}\ndescription: {description}\n---\n"),
            )
            .unwrap();
        };
        fs::create_dir_all(&home).unwrap();
        skill("alpha", "first");
        let mcp = workspace.join(".mcp.json");
        fs::write(&mcp, r#"{"mcpServers":{"one":{"command":"x"}}}"#).unwrap();
        settle(&root);
        let config = Arc::new(test_config(root.join("data"), workspace_root));
        let service = CatalogService::with_home(config, Some(home));
        let provider = ProviderKind::ClaudeCode;
        let skill_names = |catalog: SkillCatalog| {
            catalog
                .skills
                .into_iter()
                .map(|skill| format!("{}:{}", skill.name, skill.description))
                .collect::<Vec<_>>()
        };
        let server_names = |catalog: McpCatalog| {
            catalog
                .servers
                .into_iter()
                .map(|server| server.name)
                .collect::<Vec<_>>()
        };

        // Hit: the second call reuses the first scan.
        let first = service.skills(provider, workspace.clone()).await.unwrap();
        assert_eq!(skill_names(first), ["alpha:first"]);
        assert_eq!(builds(&service), 1);
        let again = service.skills(provider, workspace.clone()).await.unwrap();
        assert_eq!(skill_names(again), ["alpha:first"]);
        assert_eq!(builds(&service), 1);

        // Modify a SKILL.md.
        skill("alpha", "second version");
        let modified = service.skills(provider, workspace.clone()).await.unwrap();
        assert_eq!(skill_names(modified), ["alpha:second version"]);
        settle(&root);
        service.skills(provider, workspace.clone()).await.unwrap();
        let built = builds(&service);
        service.skills(provider, workspace.clone()).await.unwrap();
        assert_eq!(builds(&service), built, "settled scan is cached again");

        // Add a skill directory.
        skill("beta", "added");
        let added = service.skills(provider, workspace.clone()).await.unwrap();
        assert_eq!(skill_names(added), ["alpha:second version", "beta:added"]);
        settle(&root);
        service.skills(provider, workspace.clone()).await.unwrap();

        // Delete a skill directory.
        fs::remove_dir_all(workspace.join(".claude/skills/alpha")).unwrap();
        let deleted = service.skills(provider, workspace.clone()).await.unwrap();
        assert_eq!(skill_names(deleted), ["beta:added"]);

        // MCP: hit, modify, delete and re-add the config file.
        let built = builds(&service);
        assert_eq!(
            server_names(service.mcp(provider, workspace.clone()).await.unwrap()),
            ["one"]
        );
        service.mcp(provider, workspace.clone()).await.unwrap();
        assert_eq!(builds(&service), built + 1);
        fs::write(&mcp, r#"{"mcpServers":{"two":{"command":"xy"}}}"#).unwrap();
        assert_eq!(
            server_names(service.mcp(provider, workspace.clone()).await.unwrap()),
            ["two"]
        );
        fs::remove_file(&mcp).unwrap();
        assert!(server_names(service.mcp(provider, workspace.clone()).await.unwrap()).is_empty());
        fs::write(&mcp, r#"{"mcpServers":{"three":{"command":"x"}}}"#).unwrap();
        assert_eq!(
            server_names(service.mcp(provider, workspace.clone()).await.unwrap()),
            ["three"]
        );
        let _ = fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn catalog_cache_is_per_workspace_and_cleared_by_writes() {
        let root = temp_dir("todex-catalog-cache-scope");
        let home = root.join("home");
        let workspace_root = root.join("workspaces");
        let first = workspace_root.join("first");
        let second = workspace_root.join("second");
        fs::create_dir_all(&home).unwrap();
        for (workspace, name) in [(&first, "one"), (&second, "two")] {
            fs::create_dir_all(workspace).unwrap();
            fs::write(
                workspace.join(".mcp.json"),
                format!(r#"{{"mcpServers":{{"{name}":{{"command":"x"}}}}}}"#),
            )
            .unwrap();
        }
        settle(&root);
        let config = Arc::new(test_config(root.join("data"), workspace_root));
        let service = CatalogService::with_home(config, Some(home));
        let provider = ProviderKind::ClaudeCode;
        let names = |catalog: McpCatalog| {
            catalog
                .servers
                .into_iter()
                .map(|server| server.name)
                .collect::<Vec<_>>()
        };

        assert_eq!(
            names(service.mcp(provider, first.clone()).await.unwrap()),
            ["one"]
        );
        assert_eq!(
            names(service.mcp(provider, second.clone()).await.unwrap()),
            ["two"]
        );
        assert_eq!(
            names(service.mcp(provider, first.clone()).await.unwrap()),
            ["one"]
        );
        assert_eq!(
            names(service.mcp(provider, second.clone()).await.unwrap()),
            ["two"]
        );
        assert_eq!(builds(&service), 2, "each workspace is scanned once");

        // A write through the service drops cached scans right away.
        service
            .create_mcp(
                provider,
                first.clone(),
                CatalogScope::Project,
                "added".to_owned(),
                json!({ "command": "y" }),
            )
            .await
            .unwrap();
        assert_eq!(
            names(service.mcp(provider, first.clone()).await.unwrap()),
            ["added", "one"]
        );
        let skills = service.skills(provider, first.clone()).await.unwrap();
        assert!(skills.skills.is_empty());
        let created = service
            .create_skill(
                provider,
                first.clone(),
                "provider-project",
                "fresh",
                "---\nname: fresh\ndescription: new\n---\n".to_owned(),
            )
            .await
            .unwrap();
        assert_eq!(created.name, "fresh");
        settle(&root);
        let before = builds(&service);
        let skills = service.skills(provider, first.clone()).await.unwrap();
        assert_eq!(skills.skills.len(), 1);
        service.skills(provider, first.clone()).await.unwrap();
        assert_eq!(builds(&service), before + 1);
        service
            .delete_skill(provider, first.clone(), &created.resource_id)
            .await
            .unwrap();
        assert_eq!(
            builds(&service),
            before + 1,
            "deletion only looked the skill up"
        );
        assert!(service
            .skills(provider, first.clone())
            .await
            .unwrap()
            .skills
            .is_empty());
        assert_eq!(
            builds(&service),
            before + 2,
            "deletion invalidated the cache"
        );
        let _ = fs::remove_dir_all(root);
    }

    fn bundle_file(path: &str, contents: &[u8], executable: bool) -> SkillBundleFile {
        SkillBundleFile {
            path: path.to_owned(),
            content_base64: STANDARD.encode(contents),
            executable,
        }
    }

    fn bundle_paths(files: &[SkillBundleFile]) -> Vec<&str> {
        files.iter().map(|file| file.path.as_str()).collect()
    }

    #[cfg(unix)]
    #[test]
    fn skills_in_hidden_folders_of_a_root_are_built_in() {
        let root = Path::new("/home/u/.codex/skills");
        assert!(is_built_in_skill(
            root,
            &root.join(".system/imagegen/SKILL.md")
        ));
        assert!(!is_built_in_skill(root, &root.join("mine/SKILL.md")));
        assert!(!is_built_in_skill(root, &root.join("SKILL.md")));
        // Hidden folders above the root (`.codex`) do not count.
        assert!(!is_built_in_skill(
            root,
            Path::new("/home/u/.codex/skills/a/b/SKILL.md")
        ));
    }
    // Checks file modes and symlinks, which are Unix-only.
    #[cfg(unix)]
    #[tokio::test]
    async fn skill_bundles_round_trip_and_replace_the_whole_directory() {
        use std::os::unix::fs::PermissionsExt;

        let root = temp_dir("todex-catalog-bundle");
        let home = root.join("home");
        let workspace_root = root.join("workspaces");
        let workspace = workspace_root.join("project");
        fs::create_dir_all(&workspace).unwrap();
        let skill = home.join(".agents/skills/tool");
        fs::create_dir_all(skill.join("scripts")).unwrap();
        fs::create_dir_all(skill.join("assets/deep")).unwrap();
        let skill_md = "---\nname: tool\ndescription: Tool skill\n---\nRun scripts/run.sh\n";
        fs::write(skill.join("SKILL.md"), skill_md).unwrap();
        fs::write(skill.join("scripts/run.sh"), "#!/bin/sh\necho hi\n").unwrap();
        fs::set_permissions(
            skill.join("scripts/run.sh"),
            fs::Permissions::from_mode(0o750),
        )
        .unwrap();
        let binary = (0..=255u8).collect::<Vec<_>>();
        fs::write(skill.join("assets/deep/data.bin"), &binary).unwrap();
        let outside = root.join("outside");
        fs::create_dir_all(&outside).unwrap();
        fs::write(outside.join("secret.txt"), "secret").unwrap();
        std::os::unix::fs::symlink(outside.join("secret.txt"), skill.join("link.txt")).unwrap();
        std::os::unix::fs::symlink(&outside, skill.join("assets/linked-dir")).unwrap();
        let config = Arc::new(test_config(root.join("data"), workspace_root));
        let service = CatalogService::with_home(config, Some(home.clone()));
        let provider = ProviderKind::ClaudeCode;

        let listed = service
            .skills(provider, workspace.clone())
            .await
            .unwrap()
            .skills
            .into_iter()
            .find(|skill| skill.name == "tool")
            .unwrap();
        let bundle = service
            .skill_bundle(provider, workspace.clone(), &listed.resource_id)
            .await
            .unwrap();
        assert_eq!(bundle.resource_id, listed.resource_id);
        assert_eq!(bundle.source, "shared-user");
        assert_eq!(
            bundle_paths(&bundle.files),
            ["SKILL.md", "assets/deep/data.bin", "scripts/run.sh"]
        );
        assert_eq!(bundle.skipped, ["assets/linked-dir", "link.txt"]);
        assert!(!bundle.files[0].executable);
        assert!(bundle.files[2].executable);
        assert_eq!(
            STANDARD.decode(&bundle.files[1].content_base64).unwrap(),
            binary
        );
        let json = serde_json::to_value(&bundle).unwrap();
        assert!(json["files"][0]["contentBase64"].is_string());
        assert!(json["resourceId"].is_string());

        // GET -> POST elsewhere preserves contents and the execute bit.
        let copied = service
            .create_skill_bundle(
                provider,
                workspace.clone(),
                "provider-user",
                "tool-copy",
                bundle.files.clone(),
            )
            .await
            .unwrap();
        assert_eq!(copied.source, "provider-user");
        let copy = home.join(".claude/skills/tool-copy");
        assert_eq!(fs::read_to_string(copy.join("SKILL.md")).unwrap(), skill_md);
        assert_eq!(fs::read(copy.join("assets/deep/data.bin")).unwrap(), binary);
        let mode = |path: &Path| fs::metadata(path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode(&copy.join("scripts/run.sh")), 0o755);
        assert_eq!(mode(&copy.join("SKILL.md")), 0o644);
        let reread = service
            .skill_bundle(provider, workspace.clone(), &copied.resource_id)
            .await
            .unwrap();
        assert_eq!(
            serde_json::to_value(&reread.files).unwrap(),
            serde_json::to_value(&bundle.files).unwrap()
        );
        assert!(reread.skipped.is_empty());

        // An existing directory is a conflict and stays untouched.
        assert!(matches!(
            service
                .create_skill_bundle(
                    provider,
                    workspace.clone(),
                    "provider-user",
                    "tool-copy",
                    vec![bundle_file("SKILL.md", skill_md.as_bytes(), false)],
                )
                .await,
            Err(AppError::Conflict(_))
        ));
        assert!(copy.join("scripts/run.sh").exists());

        // PUT replaces the directory with exactly the given files.
        let updated_md = "---\nname: tool\ndescription: Updated\n---\n";
        let updated = service
            .replace_skill_bundle(
                provider,
                workspace.clone(),
                &listed.resource_id,
                vec![
                    bundle_file("SKILL.md", updated_md.as_bytes(), false),
                    bundle_file("bin/tool", b"#!/bin/sh\n", true),
                ],
            )
            .await
            .unwrap();
        assert_eq!(updated.resource_id, listed.resource_id);
        assert_eq!(updated.description, "Updated");
        let mut remaining = fs::read_dir(&skill)
            .unwrap()
            .map(|entry| entry.unwrap().file_name().into_string().unwrap())
            .collect::<Vec<_>>();
        remaining.sort();
        assert_eq!(remaining, ["SKILL.md", "bin"]);
        assert_eq!(mode(&skill.join("bin/tool")), 0o755);
        assert_eq!(
            fs::read_to_string(outside.join("secret.txt")).unwrap(),
            "secret"
        );
        // No staging or retired directory is left next to the skill.
        let mut siblings = fs::read_dir(home.join(".agents/skills"))
            .unwrap()
            .map(|entry| entry.unwrap().file_name().into_string().unwrap())
            .collect::<Vec<_>>();
        siblings.sort();
        assert_eq!(siblings, ["tool"]);

        // An invalid bundle leaves the skill as it was.
        assert!(matches!(
            service
                .replace_skill_bundle(
                    provider,
                    workspace.clone(),
                    &listed.resource_id,
                    vec![
                        bundle_file("SKILL.md", updated_md.as_bytes(), false),
                        bundle_file("../escape.txt", b"x", false),
                    ],
                )
                .await,
            Err(AppError::InvalidRequest(_))
        ));
        assert!(!home.join(".agents/skills/escape.txt").exists());
        assert!(skill.join("bin/tool").exists());

        // Grok Build manages its skills itself: bundle writes are refused.
        assert!(matches!(
            service
                .create_skill_bundle(
                    ProviderKind::GrokBuild,
                    workspace.clone(),
                    "shared-user",
                    "grok-copy",
                    vec![bundle_file("SKILL.md", skill_md.as_bytes(), false)],
                )
                .await,
            Err(AppError::Unsupported(_))
        ));
        assert!(matches!(
            service
                .replace_skill_bundle(
                    ProviderKind::GrokBuild,
                    workspace.clone(),
                    &listed.resource_id,
                    vec![bundle_file("SKILL.md", skill_md.as_bytes(), false)],
                )
                .await,
            Err(AppError::Unsupported(_))
        ));
        assert!(matches!(
            service
                .skill_bundle(provider, workspace.clone(), "res_missing")
                .await,
            Err(AppError::NotFound(_))
        ));
        let _ = fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn skill_bundle_limits_and_root_level_skills() {
        let root = temp_dir("todex-catalog-bundle-limits");
        let home = root.join("home");
        let workspace_root = root.join("workspaces");
        let workspace = workspace_root.join("project");
        fs::create_dir_all(&workspace).unwrap();
        let skills_root = home.join(".agents/skills");
        let content = "---\nname: many\ndescription: x\n---\n";
        let many = skills_root.join("many");
        fs::create_dir_all(&many).unwrap();
        fs::write(many.join("SKILL.md"), content).unwrap();
        for index in 0..MAX_SKILL_BUNDLE_FILES {
            fs::write(many.join(format!("f{index}.txt")), "x").unwrap();
        }
        let deep = skills_root.join("deep");
        let mut nested = deep.clone();
        for level in 0..MAX_SKILL_BUNDLE_DEPTH {
            nested = nested.join(format!("d{level}"));
        }
        fs::create_dir_all(&nested).unwrap();
        fs::write(
            deep.join("SKILL.md"),
            "---\nname: deep\ndescription: x\n---\n",
        )
        .unwrap();
        fs::write(nested.join("too-deep.txt"), "x").unwrap();
        let big = skills_root.join("big");
        fs::create_dir_all(&big).unwrap();
        fs::write(
            big.join("SKILL.md"),
            "---\nname: big\ndescription: x\n---\n",
        )
        .unwrap();
        fs::write(big.join("blob"), vec![0u8; MAX_SKILL_BUNDLE_BYTES]).unwrap();
        let root_level_md = "---\nname: loose\ndescription: x\n---\n";
        let project_root = workspace.join(".agents/skills");
        fs::create_dir_all(project_root.join("other")).unwrap();
        fs::write(project_root.join("SKILL.md"), root_level_md).unwrap();
        fs::write(project_root.join("other/notes.txt"), "x").unwrap();

        let config = Arc::new(test_config(root.join("data"), workspace_root));
        let service = CatalogService::with_home(config, Some(home));
        let provider = ProviderKind::ClaudeCode;
        let skills = service
            .skills(provider, workspace.clone())
            .await
            .unwrap()
            .skills;
        let id = |name: &str| {
            skills
                .iter()
                .find(|skill| skill.name == name)
                .unwrap()
                .resource_id
                .clone()
        };
        for name in ["many", "deep", "big"] {
            let result = service
                .skill_bundle(provider, workspace.clone(), &id(name))
                .await;
            assert!(
                matches!(result, Err(AppError::PayloadTooLarge(_))),
                "{name}: {result:?}"
            );
        }

        // A SKILL.md directly in a skill root bundles alone and cannot be
        // replaced as a directory.
        let loose = service
            .skill_bundle(provider, workspace.clone(), &id("loose"))
            .await
            .unwrap();
        assert_eq!(bundle_paths(&loose.files), ["SKILL.md"]);
        assert!(matches!(
            service
                .replace_skill_bundle(
                    provider,
                    workspace.clone(),
                    &id("loose"),
                    loose.files.clone(),
                )
                .await,
            Err(AppError::InvalidRequest(_))
        ));
        assert!(project_root.join("other/notes.txt").exists());
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn skill_bundle_writes_are_validated() {
        let skill_md = b"---\nname: x\ndescription: y\n---\n";
        let with_skill = |extra: Vec<SkillBundleFile>| {
            let mut files = vec![bundle_file("SKILL.md", skill_md, false)];
            files.extend(extra);
            decode_skill_bundle(files)
        };
        assert!(with_skill(vec![bundle_file("a/b/c.txt", b"x", true)]).is_ok());
        for path in [
            "",
            "/etc/passwd",
            "../escape",
            "a/../../escape",
            "a/./b",
            "a//b",
            "a/",
            ".",
            "a\\b",
            "a\0b",
            "C:/windows",
            "c:file",
            "1/2/3/4/5/6/7/8/9",
        ] {
            assert!(
                matches!(
                    with_skill(vec![bundle_file(path, b"x", false)]),
                    Err(AppError::InvalidRequest(_))
                ),
                "{path:?}"
            );
        }
        let invalid = |result: Result<Vec<BundleEntry>, AppError>| {
            matches!(result, Err(AppError::InvalidRequest(_)))
        };
        // Duplicate paths, a path that is both file and directory, bad
        // base64 (URL-safe alphabet and missing padding included).
        assert!(invalid(with_skill(vec![
            bundle_file("a", b"x", false),
            bundle_file("a", b"y", false),
        ])));
        assert!(invalid(with_skill(vec![
            bundle_file("a", b"x", false),
            bundle_file("a/b", b"y", false),
        ])));
        for encoded in ["not base64!", "-_8=", "YQ"] {
            assert!(invalid(with_skill(vec![SkillBundleFile {
                path: "a".to_owned(),
                content_base64: encoded.to_owned(),
                executable: false,
            }])));
        }
        // SKILL.md must be present at the root and valid.
        assert!(invalid(decode_skill_bundle(vec![bundle_file(
            "sub/SKILL.md",
            skill_md,
            false
        )])));
        assert!(invalid(decode_skill_bundle(vec![bundle_file(
            "SKILL.md",
            b"no front matter",
            false
        )])));
        assert!(invalid(decode_skill_bundle(vec![bundle_file(
            "SKILL.md",
            &[0xff, 0xfe],
            false
        )])));
        // Limits: file count and total size.
        let too_many = (0..MAX_SKILL_BUNDLE_FILES)
            .map(|index| bundle_file(&format!("f{index}"), b"x", false))
            .collect();
        assert!(matches!(
            with_skill(too_many),
            Err(AppError::PayloadTooLarge(_))
        ));
        let half = vec![0u8; MAX_SKILL_BUNDLE_BYTES / 2];
        assert!(matches!(
            with_skill(vec![
                bundle_file("a", &half, false),
                bundle_file("b", &half, false),
            ]),
            Err(AppError::PayloadTooLarge(_))
        ));
        assert!(with_skill(vec![bundle_file("a", &half, false)]).is_ok());
    }

    fn test_config(data_dir: PathBuf, workspace_root: PathBuf) -> Config {
        Config {
            host: "127.0.0.1".to_owned(),
            port: 0,
            pairing_encryption: PairingEncryption::None,
            data_dir,
            workspace_roots: vec![workspace_root],
            history_retention_days: None,
            history_seal_min_bytes: crate::config::DEFAULT_HISTORY_SEAL_MIN_BYTES,
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
                provider_keepalive_seconds: 300,
                provider_prewarm: true,
            },
            security: SecurityConfig {
                enable_auth: true,
                enable_tls: false,
            },
            api: Default::default(),
            deployment: Default::default(),
        }
    }

    fn temp_dir(prefix: &str) -> PathBuf {
        std::env::temp_dir().join(format!("{prefix}-{}", uuid::Uuid::new_v4().simple()))
    }
}
