//! SSH host inventory and OpenSSH invocation.
//!
//! Hosts come from the backend user's `~/.ssh/config` (read-only) and from a
//! TodeX-managed list rendered to `$DATA_DIR/ssh/hosts.conf`. Every ssh run
//! uses a generated `-F` wrapper that includes both plus the system config, so
//! OpenSSH itself handles keys, agents, ProxyJump and `known_hosts`. TodeX
//! never stores passwords.

pub(crate) mod config_file;
pub(crate) mod keys;

use std::{
    collections::{BTreeSet, HashMap},
    ffi::OsString,
    path::{Path, PathBuf},
    sync::Arc,
    time::{Duration, Instant},
};

use futures_util::{stream, StreamExt};
use serde::{Deserialize, Serialize};
use tokio::{
    process::Command,
    sync::{Mutex, RwLock},
};
use uuid::Uuid;

use crate::{
    error::AppError,
    external_command::{self, bounded_text, prepare_captured, CommandLimits, ExternalCommandError},
    secure_fs,
};

pub(crate) use config_file::{ManagedHost, SnippetImport};

const STORE_FILE: &str = "store.json";
const MANAGED_HOSTS_FILE: &str = "hosts.conf";
const WRAPPER_FILE: &str = "ssh_config";
const CONTROL_DIR: &str = "cm";
/// `ssh -G` output only changes when config files change; a short cache keeps
/// host lists fast without watching files.
const RESOLVE_TTL: Duration = Duration::from_secs(30);
const RESOLVE_CONCURRENCY: usize = 8;
const RESOLVE_LIMITS: CommandLimits = CommandLimits {
    timeout: Duration::from_secs(10),
    output_limit: 256 * 1024,
};
const TEST_LIMITS: CommandLimits = CommandLimits {
    timeout: Duration::from_secs(25),
    output_limit: 64 * 1024,
};
/// Shared masters stay up briefly after the last session so terminal, SFTP
/// and agent commands to the same host reuse one authenticated connection.
const CONTROL_PERSIST_SECONDS: u32 = 300;
/// sun_path is 104 bytes on macOS; `%C` expands to 40 hex characters.
const MAX_CONTROL_DIR_LEN: usize = 60;
const MAX_FTP_SITES: usize = 256;
const ERROR_DETAIL_LIMIT: usize = 2048;

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub(crate) enum FtpProtocol {
    Ftp,
    Ftps,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub(crate) struct FtpSite {
    pub id: String,
    pub name: String,
    pub protocol: FtpProtocol,
    pub host: String,
    pub port: u16,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub user: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub initial_directory: Option<String>,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct FtpSiteInput {
    pub name: String,
    pub protocol: FtpProtocol,
    pub host: String,
    #[serde(default)]
    pub port: Option<u16>,
    #[serde(default)]
    pub user: Option<String>,
    #[serde(default)]
    pub initial_directory: Option<String>,
}

#[derive(Debug, Default, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
struct SshStore {
    #[serde(default)]
    managed_hosts: Vec<ManagedHost>,
    #[serde(default)]
    ftp_sites: Vec<FtpSite>,
    /// Aliases agents may use. Empty by default: access is opt-in per host.
    #[serde(default)]
    agent_access: BTreeSet<String>,
}

#[derive(Clone, Copy, Debug, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub(crate) enum HostSource {
    SshConfig,
    Managed,
}

/// Effective connection target as reported by `ssh -G`.
#[derive(Clone, Debug, Default, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct ResolvedHost {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub host_name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub user: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub port: Option<u16>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub proxy_jump: Option<String>,
    pub identity_files: Vec<String>,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct SshHostView {
    pub alias: String,
    pub source: HostSource,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source_path: Option<String>,
    pub agent_access: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub resolved: Option<ResolvedHost>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub resolve_error: Option<String>,
    /// Editable definition; present only for TodeX-managed hosts.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub managed: Option<ManagedHost>,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct ConnectionTestResult {
    pub ok: bool,
    pub duration_ms: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub failure: Option<SshFailureKind>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
}

/// Classified `ssh` failure so clients can show an actionable hint.
#[derive(Clone, Copy, Debug, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub(crate) enum SshFailureKind {
    /// The host key is not in `known_hosts`; confirm it once in a terminal.
    HostKeyUnverified,
    /// The host key differs from `known_hosts` (possible MITM or reinstall).
    HostKeyChanged,
    /// Key/agent authentication failed or a password would be required.
    AuthenticationFailed,
    Unreachable,
    TimedOut,
    Other,
}

/// A PTY launch of `ssh` (see [`SshService::terminal_command`]).
#[derive(Clone, Debug)]
pub(crate) struct TerminalCommand {
    pub program: String,
    pub args: Vec<OsString>,
    /// Complete environment; the daemon's own environment is not inherited.
    pub env: Vec<(String, OsString)>,
    pub cwd: PathBuf,
    pub ssh_host: String,
}

/// How an ssh process may interact with the user.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum SshMode {
    /// No prompts, unknown host keys rejected. Used for agents, SFTP, tests.
    Batch,
    /// Runs in a PTY: the user can answer host-key and password prompts.
    Interactive,
    /// No terminal; a single password prompt is answered by the program the
    /// caller sets as `SSH_ASKPASS`. Unknown host keys are still rejected.
    Askpass,
}

/// When `ssh -G` ran for an alias, and its outcome.
type CachedResolution = (Instant, Result<ResolvedHost, String>);

#[derive(Clone)]
pub struct SshService {
    inner: Arc<Inner>,
}

struct Inner {
    dir: PathBuf,
    ssh_bin: String,
    home: Option<PathBuf>,
    multiplex: bool,
    store: RwLock<SshStore>,
    resolve_cache: Mutex<HashMap<String, CachedResolution>>,
}

impl SshService {
    pub async fn new(data_dir: &Path, ssh_bin: String) -> Result<Self, AppError> {
        Self::with_home(data_dir, ssh_bin, home_dir()).await
    }

    pub(crate) async fn with_home(
        data_dir: &Path,
        ssh_bin: String,
        home: Option<PathBuf>,
    ) -> Result<Self, AppError> {
        let dir = data_dir.join("ssh");
        let control_dir = dir.join(CONTROL_DIR);
        {
            let control_dir = control_dir.clone();
            tokio::task::spawn_blocking(move || secure_fs::ensure_owner_only_dir(&control_dir))
                .await
                .map_err(|error| AppError::Anyhow(error.into()))??;
        }
        let store = match tokio::fs::read(dir.join(STORE_FILE)).await {
            Ok(bytes) => serde_json::from_slice(&bytes)?,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => SshStore::default(),
            Err(error) => return Err(error.into()),
        };
        let multiplex = cfg!(unix) && control_dir.as_os_str().len() <= MAX_CONTROL_DIR_LEN;
        let service = Self {
            inner: Arc::new(Inner {
                dir,
                ssh_bin,
                home,
                multiplex,
                store: RwLock::new(store),
                resolve_cache: Mutex::new(HashMap::new()),
            }),
        };
        {
            let store = service.inner.store.read().await;
            service.write_config_files(&store).await?;
        }
        Ok(service)
    }

    fn user_config(&self) -> Option<PathBuf> {
        self.inner
            .home
            .as_ref()
            .map(|home| home.join(".ssh").join("config"))
    }

    fn wrapper_path(&self) -> PathBuf {
        self.inner.dir.join(WRAPPER_FILE)
    }

    /// Writes `hosts.conf` and the `-F` wrapper. Called under the store lock.
    async fn write_config_files(&self, store: &SshStore) -> Result<(), AppError> {
        let managed_path = self.inner.dir.join(MANAGED_HOSTS_FILE);
        let wrapper = config_file::render_wrapper_config(
            &managed_path,
            &self
                .user_config()
                .unwrap_or_else(|| PathBuf::from("/nonexistent/.ssh/config")),
            system_config().as_deref(),
        );
        let managed = config_file::render_managed_hosts(&store.managed_hosts);
        let store_json = serde_json::to_vec_pretty(store)?;
        let dir = self.inner.dir.clone();
        tokio::task::spawn_blocking(move || -> std::io::Result<()> {
            secure_fs::write_owner_only_atomic(&dir.join(MANAGED_HOSTS_FILE), managed.as_bytes())?;
            secure_fs::write_owner_only_atomic(&dir.join(WRAPPER_FILE), wrapper.as_bytes())?;
            secure_fs::write_owner_only_atomic(&dir.join(STORE_FILE), &store_json)
        })
        .await
        .map_err(|error| AppError::Anyhow(error.into()))??;
        self.inner.resolve_cache.lock().await.clear();
        Ok(())
    }

    /// An `ssh` command that reads only TodeX's wrapper config. Callers append
    /// `--`, the alias, and an optional remote command.
    pub(crate) fn command(&self, mode: SshMode) -> Command {
        let mut command = external_command::secure_command(&self.inner.ssh_bin);
        command.args(self.options(mode));
        if mode == SshMode::Askpass {
            // Use SSH_ASKPASS even without a display or with a tty.
            command.env("SSH_ASKPASS_REQUIRE", "force");
        }
        command
    }

    /// `ssh -tt <alias>` for a PTY, with the same sanitized environment as
    /// [`Self::command`]. Host-key and password prompts reach the user.
    pub(crate) fn terminal_command(&self, alias: &str) -> TerminalCommand {
        let mut args = self.options(SshMode::Interactive);
        args.extend(["-tt".into(), "--".into(), alias.into()]);
        TerminalCommand {
            program: self.inner.ssh_bin.clone(),
            args,
            env: external_command::inherited_env()
                .into_iter()
                .map(|(key, value)| (key.to_owned(), value))
                .collect(),
            cwd: self
                .inner
                .home
                .clone()
                .unwrap_or_else(|| self.inner.dir.clone()),
            ssh_host: alias.to_owned(),
        }
    }

    fn options(&self, mode: SshMode) -> Vec<OsString> {
        let mut args: Vec<OsString> = vec!["-F".into(), self.wrapper_path().into()];
        if self.inner.multiplex {
            args.extend(self.control_options(mode));
        }
        let mode_options: &[&str] = match mode {
            SshMode::Batch => &["-o", "BatchMode=yes"],
            SshMode::Askpass => &["-o", "BatchMode=no", "-o", "NumberOfPasswordPrompts=1"],
            SshMode::Interactive => return args,
        };
        args.extend(mode_options.iter().map(OsString::from));
        args.extend(
            ["-o", "StrictHostKeyChecking=yes", "-o", "ConnectTimeout=15"].map(OsString::from),
        );
        args
    }

    fn control_options(&self, mode: SshMode) -> Vec<OsString> {
        let control_path = self.inner.dir.join(CONTROL_DIR).join("%C");
        // An askpass process carries a password in its environment; it may
        // use an existing master but must not become a long-lived one.
        let master = if mode == SshMode::Askpass {
            "no"
        } else {
            "auto"
        };
        vec![
            "-o".into(),
            format!("ControlMaster={master}").into(),
            "-o".into(),
            format!("ControlPath={}", control_path.display()).into(),
            "-o".into(),
            format!("ControlPersist={CONTROL_PERSIST_SECONDS}").into(),
        ]
    }

    /// Closes the shared master connection for `alias`, ending reuse by later
    /// commands. Returns false when no master was running.
    pub async fn disconnect(&self, alias: &str) -> Result<bool, AppError> {
        self.require_host(alias).await?;
        if !self.inner.multiplex {
            return Ok(false);
        }
        let mut command = external_command::secure_command(&self.inner.ssh_bin);
        command
            .arg("-F")
            .arg(self.wrapper_path())
            .args(self.control_options(SshMode::Batch))
            .args(["-O", "exit", "--", alias]);
        prepare_captured(&mut command, false);
        match external_command::run(command, None, RESOLVE_LIMITS).await {
            Ok(output) => Ok(output.status.success()),
            Err(ExternalCommandError::NotFound) => {
                Err(AppError::Unsupported("ssh executable not found".to_owned()))
            }
            Err(error) => Err(AppError::Anyhow(anyhow::anyhow!("ssh -O exit: {error}"))),
        }
    }

    pub async fn list_hosts(&self) -> Result<Vec<SshHostView>, AppError> {
        let (managed, agent_access) = {
            let store = self.inner.store.read().await;
            (store.managed_hosts.clone(), store.agent_access.clone())
        };
        let mut views: Vec<SshHostView> = managed
            .into_iter()
            .map(|host| SshHostView {
                alias: host.alias.clone(),
                source: HostSource::Managed,
                source_path: None,
                agent_access: agent_access.contains(&host.alias),
                resolved: None,
                resolve_error: None,
                managed: Some(host),
            })
            .collect();
        if let (Some(config), Some(home)) = (self.user_config(), self.inner.home.clone()) {
            let discovered =
                tokio::task::spawn_blocking(move || config_file::discover_aliases(&config, &home))
                    .await
                    .map_err(|error| AppError::Anyhow(error.into()))?;
            for alias in discovered {
                // Managed hosts come first in the wrapper, so they win.
                if views.iter().any(|view| view.alias == alias.alias) {
                    continue;
                }
                views.push(SshHostView {
                    agent_access: agent_access.contains(&alias.alias),
                    alias: alias.alias,
                    source: HostSource::SshConfig,
                    source_path: Some(alias.source.display().to_string()),
                    resolved: None,
                    resolve_error: None,
                    managed: None,
                });
            }
        }
        let resolved: Vec<Result<ResolvedHost, String>> = stream::iter(
            views
                .iter()
                .map(|view| view.alias.clone())
                .collect::<Vec<_>>(),
        )
        .map(|alias| async move { self.resolve(&alias).await })
        .buffered(RESOLVE_CONCURRENCY)
        .collect()
        .await;
        for (view, result) in views.iter_mut().zip(resolved) {
            match result {
                Ok(resolved) => view.resolved = Some(resolved),
                Err(error) => view.resolve_error = Some(error),
            }
        }
        Ok(views)
    }

    /// Fails unless `alias` is a known host. Every ssh entry point checks this
    /// so clients and agents can only reach configured hosts.
    pub(crate) async fn require_host(&self, alias: &str) -> Result<SshHostView, AppError> {
        if !config_file::valid_alias(alias) {
            return Err(AppError::InvalidRequest(format!(
                "invalid ssh host: {alias}"
            )));
        }
        self.list_hosts()
            .await?
            .into_iter()
            .find(|view| view.alias == alias)
            .ok_or_else(|| AppError::NotFound(format!("ssh host {alias}")))
    }

    async fn resolve(&self, alias: &str) -> Result<ResolvedHost, String> {
        if let Some((at, result)) = self.inner.resolve_cache.lock().await.get(alias) {
            if at.elapsed() < RESOLVE_TTL {
                return result.clone();
            }
        }
        let mut command = external_command::secure_command(&self.inner.ssh_bin);
        command
            .arg("-F")
            .arg(self.wrapper_path())
            .arg("-G")
            .arg("--")
            .arg(alias);
        prepare_captured(&mut command, false);
        let result = match external_command::run(command, None, RESOLVE_LIMITS).await {
            Ok(output) if output.status.success() => {
                Ok(parse_resolved(&String::from_utf8_lossy(&output.stdout)))
            }
            Ok(output) => Err(bounded_text(&output.stderr, ERROR_DETAIL_LIMIT)
                .trim()
                .to_owned()),
            Err(ExternalCommandError::NotFound) => Err("ssh executable not found".to_owned()),
            Err(error) => Err(error.to_string()),
        };
        self.inner
            .resolve_cache
            .lock()
            .await
            .insert(alias.to_owned(), (Instant::now(), result.clone()));
        result
    }

    pub async fn create_host(&self, host: ManagedHost) -> Result<ManagedHost, AppError> {
        let host = host.normalized()?;
        self.ensure_alias_free(&host.alias, None).await?;
        let mut store = self.inner.store.write().await;
        if store.managed_hosts.len() >= config_file::MAX_MANAGED_HOSTS {
            return Err(AppError::ResourceExhausted("too many ssh hosts".to_owned()));
        }
        if store.managed_hosts.iter().any(|h| h.alias == host.alias) {
            return Err(AppError::Conflict(format!(
                "ssh host {} exists",
                host.alias
            )));
        }
        store.managed_hosts.push(host.clone());
        self.write_config_files(&store).await?;
        Ok(host)
    }

    pub async fn update_host(
        &self,
        alias: &str,
        host: ManagedHost,
    ) -> Result<ManagedHost, AppError> {
        let host = host.normalized()?;
        if host.alias != alias {
            self.ensure_alias_free(&host.alias, Some(alias)).await?;
        }
        let mut store = self.inner.store.write().await;
        if host.alias != alias && store.managed_hosts.iter().any(|h| h.alias == host.alias) {
            return Err(AppError::Conflict(format!(
                "ssh host {} exists",
                host.alias
            )));
        }
        let Some(slot) = store.managed_hosts.iter_mut().find(|h| h.alias == alias) else {
            return Err(AppError::NotFound(format!("managed ssh host {alias}")));
        };
        *slot = host.clone();
        if host.alias != alias && store.agent_access.remove(alias) {
            store.agent_access.insert(host.alias.clone());
        }
        self.write_config_files(&store).await?;
        Ok(host)
    }

    pub async fn delete_host(&self, alias: &str) -> Result<(), AppError> {
        let mut store = self.inner.store.write().await;
        let before = store.managed_hosts.len();
        store.managed_hosts.retain(|h| h.alias != alias);
        if store.managed_hosts.len() == before {
            return Err(AppError::NotFound(format!("managed ssh host {alias}")));
        }
        store.agent_access.remove(alias);
        self.write_config_files(&store).await
    }

    /// Imports every valid host of a pasted snippet; per-host problems are
    /// returned instead of failing the whole import.
    pub async fn import_snippet(&self, text: &str) -> Result<SnippetImport, AppError> {
        let parsed = config_file::parse_snippet(text);
        let mut result = SnippetImport {
            hosts: Vec::new(),
            errors: parsed.errors,
        };
        for host in parsed.hosts {
            let alias = host.alias.clone();
            match self.create_host(host).await {
                Ok(host) => result.hosts.push(host),
                Err(error) => result.errors.push(format!("{alias}: {error}")),
            }
        }
        Ok(result)
    }

    async fn ensure_alias_free(&self, alias: &str, renaming: Option<&str>) -> Result<(), AppError> {
        let (Some(config), Some(home)) = (self.user_config(), self.inner.home.clone()) else {
            return Ok(());
        };
        let discovered =
            tokio::task::spawn_blocking(move || config_file::discover_aliases(&config, &home))
                .await
                .map_err(|error| AppError::Anyhow(error.into()))?;
        if renaming != Some(alias) && discovered.iter().any(|found| found.alias == alias) {
            return Err(AppError::Conflict(format!(
                "{alias} is already defined in ~/.ssh/config"
            )));
        }
        Ok(())
    }

    pub async fn set_agent_access(&self, alias: &str, enabled: bool) -> Result<(), AppError> {
        self.require_host(alias).await?;
        let mut store = self.inner.store.write().await;
        let changed = if enabled {
            store.agent_access.insert(alias.to_owned())
        } else {
            store.agent_access.remove(alias)
        };
        if changed {
            self.write_config_files(&store).await?;
        }
        Ok(())
    }

    /// Aliases agents may use, restricted to hosts that still exist.
    pub async fn agent_hosts(&self) -> Result<Vec<SshHostView>, AppError> {
        Ok(self
            .list_hosts()
            .await?
            .into_iter()
            .filter(|view| view.agent_access)
            .collect())
    }

    pub async fn has_agent_hosts(&self) -> bool {
        !self.inner.store.read().await.agent_access.is_empty()
    }

    pub async fn test_connection(&self, alias: &str) -> Result<ConnectionTestResult, AppError> {
        self.require_host(alias).await?;
        let mut command = self.command(SshMode::Batch);
        // `exit 0` works in sh, cmd.exe and PowerShell login shells alike.
        command.arg("--").arg(alias).arg("exit 0");
        prepare_captured(&mut command, false);
        let started = Instant::now();
        let outcome = external_command::run(command, None, TEST_LIMITS).await;
        let duration_ms = started.elapsed().as_millis() as u64;
        Ok(match outcome {
            Ok(output) if output.status.success() => ConnectionTestResult {
                ok: true,
                duration_ms,
                failure: None,
                detail: None,
            },
            Ok(output) => {
                let detail = bounded_text(&output.stderr, ERROR_DETAIL_LIMIT)
                    .trim()
                    .to_owned();
                ConnectionTestResult {
                    ok: false,
                    duration_ms,
                    failure: Some(classify_failure(&detail)),
                    detail: Some(detail),
                }
            }
            Err(ExternalCommandError::NotFound) => {
                return Err(AppError::Unsupported("ssh executable not found".to_owned()))
            }
            Err(ExternalCommandError::TimedOut) => ConnectionTestResult {
                ok: false,
                duration_ms,
                failure: Some(SshFailureKind::TimedOut),
                detail: None,
            },
            Err(error) => ConnectionTestResult {
                ok: false,
                duration_ms,
                failure: Some(SshFailureKind::Other),
                detail: Some(error.to_string()),
            },
        })
    }

    pub async fn ftp_sites(&self) -> Vec<FtpSite> {
        self.inner.store.read().await.ftp_sites.clone()
    }

    pub async fn ftp_site(&self, id: &str) -> Result<FtpSite, AppError> {
        self.inner
            .store
            .read()
            .await
            .ftp_sites
            .iter()
            .find(|site| site.id == id)
            .cloned()
            .ok_or_else(|| AppError::NotFound(format!("ftp site {id}")))
    }

    pub async fn upsert_ftp_site(
        &self,
        id: Option<&str>,
        input: FtpSiteInput,
    ) -> Result<FtpSite, AppError> {
        let site = normalize_ftp_site(id.map(str::to_owned), input)?;
        let mut store = self.inner.store.write().await;
        match store.ftp_sites.iter_mut().find(|s| s.id == site.id) {
            Some(slot) => *slot = site.clone(),
            None if id.is_some() => {
                return Err(AppError::NotFound(format!("ftp site {}", site.id)))
            }
            None => {
                if store.ftp_sites.len() >= MAX_FTP_SITES {
                    return Err(AppError::ResourceExhausted("too many ftp sites".to_owned()));
                }
                store.ftp_sites.push(site.clone());
            }
        }
        self.write_config_files(&store).await?;
        Ok(site)
    }

    pub async fn delete_ftp_site(&self, id: &str) -> Result<(), AppError> {
        let mut store = self.inner.store.write().await;
        let before = store.ftp_sites.len();
        store.ftp_sites.retain(|site| site.id != id);
        if store.ftp_sites.len() == before {
            return Err(AppError::NotFound(format!("ftp site {id}")));
        }
        self.write_config_files(&store).await
    }
}

fn normalize_ftp_site(id: Option<String>, input: FtpSiteInput) -> Result<FtpSite, AppError> {
    let invalid = |message: &str| AppError::InvalidRequest(message.to_owned());
    let name = input.name.trim().to_owned();
    if name.is_empty() || name.len() > 128 || name.chars().any(char::is_control) {
        return Err(invalid("name must be 1-128 printable characters"));
    }
    let host = input.host.trim().to_owned();
    if host.is_empty()
        || host.len() > 255
        || host.chars().any(|c| c.is_whitespace() || c.is_control())
    {
        return Err(invalid("host must be 1-255 characters without whitespace"));
    }
    let clean = |value: Option<String>| {
        value
            .map(|value| value.trim().to_owned())
            .filter(|value| !value.is_empty())
    };
    let user = clean(input.user);
    if user
        .as_ref()
        .is_some_and(|user| user.len() > 128 || user.chars().any(char::is_control))
    {
        return Err(invalid("user must be at most 128 printable characters"));
    }
    let initial_directory = clean(input.initial_directory);
    if initial_directory
        .as_ref()
        .is_some_and(|dir| dir.len() > 1024 || dir.chars().any(char::is_control))
    {
        return Err(invalid(
            "initialDirectory must be at most 1024 printable characters",
        ));
    }
    let port = match input.port {
        Some(0) => return Err(invalid("port must be 1-65535")),
        Some(port) => port,
        None => 21,
    };
    Ok(FtpSite {
        id: id.unwrap_or_else(|| Uuid::new_v4().to_string()),
        name,
        protocol: input.protocol,
        host,
        port,
        user,
        initial_directory,
    })
}

fn parse_resolved(output: &str) -> ResolvedHost {
    let mut resolved = ResolvedHost::default();
    for line in output.lines() {
        let Some((key, value)) = line.split_once(' ') else {
            continue;
        };
        let value = value.trim();
        match key {
            "hostname" => resolved.host_name = Some(value.to_owned()),
            "user" => resolved.user = Some(value.to_owned()),
            "port" => resolved.port = value.parse().ok(),
            "proxyjump" if value != "none" => resolved.proxy_jump = Some(value.to_owned()),
            "identityfile" => resolved.identity_files.push(value.to_owned()),
            _ => {}
        }
    }
    resolved
}

pub(crate) fn classify_failure(stderr: &str) -> SshFailureKind {
    let text = stderr.to_ascii_lowercase();
    if text.contains("remote host identification has changed") {
        SshFailureKind::HostKeyChanged
    } else if text.contains("host key verification failed")
        || text.contains("no ") && text.contains("host key is known")
    {
        SshFailureKind::HostKeyUnverified
    } else if text.contains("permission denied") || text.contains("too many authentication") {
        SshFailureKind::AuthenticationFailed
    } else if text.contains("timed out") {
        SshFailureKind::TimedOut
    } else if text.contains("could not resolve hostname")
        || text.contains("connection refused")
        || text.contains("no route to host")
        || text.contains("network is unreachable")
        || text.contains("connection closed by")
    {
        SshFailureKind::Unreachable
    } else {
        SshFailureKind::Other
    }
}

fn home_dir() -> Option<PathBuf> {
    let key = if cfg!(windows) { "USERPROFILE" } else { "HOME" };
    std::env::var_os(key)
        .filter(|home| !home.is_empty())
        .map(PathBuf::from)
}

fn system_config() -> Option<PathBuf> {
    let path = if cfg!(windows) {
        PathBuf::from(std::env::var_os("PROGRAMDATA")?)
            .join("ssh")
            .join("ssh_config")
    } else {
        PathBuf::from("/etc/ssh/ssh_config")
    };
    path.is_file().then_some(path)
}

#[cfg(all(test, unix))]
pub(crate) mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    pub(crate) struct Fixture {
        pub root: PathBuf,
        pub service: SshService,
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.root);
        }
    }

    /// A fake `ssh` that answers `-G` like OpenSSH and logs every argv; hosts
    /// named `bad*` fail authentication.
    pub(crate) async fn fixture(user_config: &str) -> Fixture {
        let root = std::env::temp_dir().join(format!("todex-ssh-{}", Uuid::new_v4().simple()));
        let home = root.join("home");
        std::fs::create_dir_all(home.join(".ssh")).unwrap();
        std::fs::write(home.join(".ssh/config"), user_config).unwrap();
        let fake = root.join("fake-ssh");
        std::fs::write(
            &fake,
            format!(
                r#"#!/bin/sh
printf '%s\n' "$*" >> "{log}"
last=""; for arg in "$@"; do prev="$last"; last="$arg"; done
case " $* " in
  *" -G "*) printf 'hostname %s.example\nuser ops\nport 22\nidentityfile ~/.ssh/id_ed25519\n' "$last"; exit 0;;
esac
case "$prev" in
  bad*) echo "ops@$prev: Permission denied (publickey)." >&2; exit 255;;
esac
exit 0
"#,
                log = root.join("ssh.log").display()
            ),
        )
        .unwrap();
        std::fs::set_permissions(&fake, std::fs::Permissions::from_mode(0o755)).unwrap();
        let service =
            SshService::with_home(&root.join("data"), fake.display().to_string(), Some(home))
                .await
                .unwrap();
        Fixture { root, service }
    }

    fn managed(alias: &str) -> ManagedHost {
        ManagedHost {
            alias: alias.into(),
            host_name: "10.0.0.5".into(),
            ..ManagedHost::default()
        }
    }

    #[tokio::test]
    async fn lists_config_and_managed_hosts_with_resolution() {
        let fixture = fixture("Host web\n  HostName web.internal\nHost *\n").await;
        fixture.service.create_host(managed("db")).await.unwrap();
        let hosts = fixture.service.list_hosts().await.unwrap();
        let aliases: Vec<(&str, HostSource)> =
            hosts.iter().map(|h| (h.alias.as_str(), h.source)).collect();
        assert_eq!(
            aliases,
            [("db", HostSource::Managed), ("web", HostSource::SshConfig)]
        );
        let web = &hosts[1].resolved.as_ref().unwrap();
        assert_eq!(web.host_name.as_deref(), Some("web.example"));
        assert_eq!(web.port, Some(22));
        assert!(hosts.iter().all(|host| !host.agent_access));

        let data = fixture.root.join("data/ssh");
        let rendered = std::fs::read_to_string(data.join("hosts.conf")).unwrap();
        assert!(rendered.contains("Host db\n  HostName 10.0.0.5\n"));
        let wrapper = std::fs::read_to_string(data.join("ssh_config")).unwrap();
        assert!(wrapper.contains(&format!(
            "Include \"{}\"",
            data.join("hosts.conf").display()
        )));
        let mode = std::fs::metadata(data.join("store.json"))
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o600);
    }

    #[tokio::test]
    async fn rejects_alias_clashes_and_unknown_hosts() {
        let fixture = fixture("Host web\n").await;
        let error = fixture
            .service
            .create_host(managed("web"))
            .await
            .unwrap_err();
        assert!(matches!(error, AppError::Conflict(_)));
        fixture.service.create_host(managed("db")).await.unwrap();
        assert!(matches!(
            fixture
                .service
                .create_host(managed("db"))
                .await
                .unwrap_err(),
            AppError::Conflict(_)
        ));
        assert!(matches!(
            fixture
                .service
                .set_agent_access("nope", true)
                .await
                .unwrap_err(),
            AppError::NotFound(_)
        ));
        assert!(matches!(
            fixture
                .service
                .require_host("-oProxyCommand=x")
                .await
                .unwrap_err(),
            AppError::InvalidRequest(_)
        ));
    }

    #[tokio::test]
    async fn agent_access_is_opt_in_and_follows_renames() {
        let fixture = fixture("").await;
        fixture.service.create_host(managed("db")).await.unwrap();
        assert!(!fixture.service.has_agent_hosts().await);
        fixture.service.set_agent_access("db", true).await.unwrap();
        fixture
            .service
            .update_host("db", managed("db2"))
            .await
            .unwrap();
        let agent: Vec<String> = fixture
            .service
            .agent_hosts()
            .await
            .unwrap()
            .into_iter()
            .map(|h| h.alias)
            .collect();
        assert_eq!(agent, ["db2"]);
        fixture.service.delete_host("db2").await.unwrap();
        assert!(!fixture.service.has_agent_hosts().await);

        // State survives a restart.
        fixture.service.create_host(managed("db")).await.unwrap();
        fixture.service.set_agent_access("db", true).await.unwrap();
        let reloaded = SshService::with_home(
            &fixture.root.join("data"),
            "ssh".into(),
            Some(fixture.root.join("home")),
        )
        .await
        .unwrap();
        assert!(reloaded.has_agent_hosts().await);
    }

    #[tokio::test]
    async fn connection_test_uses_batch_mode_and_classifies_failures() {
        let fixture = fixture("Host good bad1\n").await;
        let ok = fixture.service.test_connection("good").await.unwrap();
        assert!(ok.ok);
        let failed = fixture.service.test_connection("bad1").await.unwrap();
        assert!(!failed.ok);
        assert_eq!(failed.failure, Some(SshFailureKind::AuthenticationFailed));
        let log = std::fs::read_to_string(fixture.root.join("ssh.log")).unwrap();
        let test_line = log
            .lines()
            .find(|line| line.ends_with("good exit 0"))
            .unwrap();
        assert!(test_line.contains("-o BatchMode=yes -o StrictHostKeyChecking=yes"));
        assert!(test_line.contains("-F "));
        assert!(test_line.contains("-- good exit 0"));
    }

    #[tokio::test]
    async fn askpass_mode_allows_one_password_prompt() {
        let fixture = fixture("").await;
        let command = fixture.service.command(SshMode::Askpass);
        let args: Vec<String> = command
            .as_std()
            .get_args()
            .map(|arg| arg.to_string_lossy().into_owned())
            .collect();
        let args = args.join(" ");
        assert!(args
            .contains("-o BatchMode=no -o NumberOfPasswordPrompts=1 -o StrictHostKeyChecking=yes"));
        assert!(!args.contains("ControlMaster=auto"));
        assert!(command
            .as_std()
            .get_envs()
            .any(|(key, value)| key == "SSH_ASKPASS_REQUIRE" && value == Some("force".as_ref())));
    }

    #[tokio::test]
    async fn imports_snippets_and_ftp_sites() {
        let fixture = fixture("Host web\n").await;
        let import = fixture
            .service
            .import_snippet("Host web\n  HostName x\nHost new\n  HostName 1.2.3.4\n  User root\n")
            .await
            .unwrap();
        assert_eq!(import.hosts.len(), 1);
        assert_eq!(import.hosts[0].alias, "new");
        assert_eq!(import.errors.len(), 1, "{:?}", import.errors);

        let site = fixture
            .service
            .upsert_ftp_site(
                None,
                FtpSiteInput {
                    name: "Files".into(),
                    protocol: FtpProtocol::Ftps,
                    host: "ftp.example".into(),
                    port: None,
                    user: Some(" alice ".into()),
                    initial_directory: None,
                },
            )
            .await
            .unwrap();
        assert_eq!(site.port, 21);
        assert_eq!(site.user.as_deref(), Some("alice"));
        assert_eq!(fixture.service.ftp_sites().await.len(), 1);
        fixture.service.delete_ftp_site(&site.id).await.unwrap();
        assert!(fixture.service.ftp_site(&site.id).await.is_err());
    }

    #[test]
    fn classifies_common_ssh_errors() {
        assert_eq!(
            classify_failure("Host key verification failed."),
            SshFailureKind::HostKeyUnverified
        );
        assert_eq!(
            classify_failure(
                "No ED25519 host key is known for x and you have requested strict checking."
            ),
            SshFailureKind::HostKeyUnverified
        );
        assert_eq!(
            classify_failure("@ WARNING: REMOTE HOST IDENTIFICATION HAS CHANGED! @"),
            SshFailureKind::HostKeyChanged
        );
        assert_eq!(
            classify_failure("ssh: Could not resolve hostname x: nodename nor servname"),
            SshFailureKind::Unreachable
        );
    }

    /// Manual smoke test against the real OpenSSH and `~/.ssh/config`:
    /// `TODEX_SSH_REAL=1 cargo test real_ssh_inventory -- --nocapture`.
    #[tokio::test]
    async fn real_ssh_inventory() {
        if std::env::var_os("TODEX_SSH_REAL").is_none() {
            return;
        }
        let root = std::env::temp_dir().join(format!("todex-ssh-real-{}", Uuid::new_v4().simple()));
        let service = SshService::new(&root, "ssh".into()).await.unwrap();
        for host in service.list_hosts().await.unwrap() {
            println!(
                "{} {:?} {:?} {:?}",
                host.alias, host.source, host.resolved, host.resolve_error
            );
        }
        let _ = std::fs::remove_dir_all(root);
    }
}
