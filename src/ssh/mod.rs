//! SSH host inventory and OpenSSH invocation.
//!
//! Hosts come from the backend user's `~/.ssh/config` (read-only) and from a
//! TodeX-managed list rendered to `$DATA_DIR/ssh/hosts.conf`. Every ssh run
//! uses a generated `-F` wrapper that includes both, TodeX's connection
//! defaults (`defaults.conf`) and the system config, so OpenSSH itself handles
//! keys, agents, ProxyJump and `known_hosts`. TodeX never stores passwords: a
//! client may send one per connection, answered via the askpass helper from
//! a one-shot file (see [`askpass`]).

pub(crate) mod config_file;
pub(crate) mod forward;
pub(crate) mod ip_pool;
pub(crate) mod keys;
pub(crate) mod login;
mod tunnel;
#[cfg(all(test, unix))]
mod tunnel_tests;

use std::{
    collections::{BTreeMap, BTreeSet, HashMap},
    ffi::OsString,
    net::Ipv4Addr,
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
    privacy_filter::AgentOutputFilter,
    remote_fs::askpass::{self, AskpassSecret},
    secure_fs,
};

pub(crate) use config_file::{ConnectionSettings, ManagedHost, SnippetImport, SshAuthMethod};
use forward::{ForwardInput, ForwardRuntime, ForwardSpec, ForwardView};
use ip_pool::{
    AssignmentInput, AssignmentUpdate, AssignmentView, HostTraits, PoolConfigInput, PoolRuntime,
    PoolView, StoredIpPool,
};

const STORE_FILE: &str = "store.json";
const MANAGED_HOSTS_FILE: &str = "hosts.conf";
const DEFAULTS_FILE: &str = "defaults.conf";
const WRAPPER_FILE: &str = "ssh_config";
const CONTROL_DIR: &str = "cm";
/// Pids of running forwards and pool masters (see [`tunnel::ChildRegistry`]).
const CHILDREN_FILE: &str = "children.json";
/// One-shot password files for the askpass helper.
const ASKPASS_DIR: &str = "askpass";
/// How long a terminal's password file may wait for ssh's prompt. The
/// terminal's ssh may become the long-lived shared master, so the file is
/// not tied to that process.
pub(crate) const TERMINAL_ASKPASS_DEADLINE: Duration = Duration::from_secs(120);
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

#[derive(Clone, Debug, Default)]
struct SshStore {
    managed_hosts: Vec<ManagedHost>,
    ftp_sites: Vec<FtpSite>,
    /// Aliases agents may use. Empty by default: access is opt-in per host.
    agent_access: BTreeSet<String>,
    connection: ConnectionSettings,
    /// Manual port forwards (configuration only; never auto-started).
    forwards: Vec<ForwardSpec>,
    ip_pool: StoredIpPool,
    /// Redaction of `ssh_exec` output before agents read it.
    agent_output_filter: AgentOutputFilter,
    /// The [`AGENT_LOCAL_HOST`] pseudo-host for agents; off by default.
    agent_local: AgentLocal,
}

/// The alias agents use to run commands on the daemon's own machine; real
/// hosts with this name (in any case) are kept from agents while it is on.
pub(crate) const AGENT_LOCAL_HOST: &str = "Local";

pub(crate) fn is_agent_local_alias(alias: &str) -> bool {
    alias.eq_ignore_ascii_case(AGENT_LOCAL_HOST)
}

/// `/v2/ssh/agent-local`: whether agents get the [`AGENT_LOCAL_HOST`].
#[derive(Clone, Copy, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct AgentLocal {
    pub enabled: bool,
}

impl AgentLocal {
    /// Anything unreadable in the store keeps it off.
    fn from_stored(value: serde_json::Value) -> Self {
        Self {
            enabled: value
                .get("enabled")
                .and_then(serde_json::Value::as_bool)
                .unwrap_or(false),
        }
    }
}

/// `store.json`. Older daemons ignore unknown top-level fields but reject
/// unknown fields in host records, so everything newer than v2.3.0 that
/// belongs to a host is kept beside the records, keyed by alias.
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct PersistedStore {
    #[serde(default)]
    managed_hosts: Vec<config_file::StoredHost>,
    #[serde(default)]
    ftp_sites: Vec<FtpSite>,
    #[serde(default)]
    agent_access: BTreeSet<String>,
    /// Non-key auth methods by alias; hosts not listed use keys.
    #[serde(default)]
    auth_methods: BTreeMap<String, String>,
    #[serde(default)]
    connection: Option<config_file::StoredConnection>,
    /// Read entry by entry so one invalid forward does not drop the rest;
    /// anything but an array (also `null`) counts as no forwards.
    #[serde(default)]
    forwards: Option<serde_json::Value>,
    #[serde(default)]
    ip_pool: Option<serde_json::Value>,
    #[serde(default)]
    agent_output_filter: Option<serde_json::Value>,
    #[serde(default)]
    agent_local: Option<serde_json::Value>,
}

/// The written form of [`SshStore`], read back as [`PersistedStore`].
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct PersistedStoreRef<'a> {
    managed_hosts: Vec<config_file::StoredHost>,
    ftp_sites: &'a [FtpSite],
    agent_access: &'a BTreeSet<String>,
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    auth_methods: BTreeMap<&'a str, &'static str>,
    connection: &'a ConnectionSettings,
    #[serde(skip_serializing_if = "<[ForwardSpec]>::is_empty")]
    forwards: &'a [ForwardSpec],
    #[serde(skip_serializing_if = "Option::is_none")]
    ip_pool: Option<&'a StoredIpPool>,
    agent_output_filter: &'a AgentOutputFilter,
    agent_local: &'a AgentLocal,
}

impl SshStore {
    /// Reads `store.json` from any version. Inline auth methods (v2.3.1)
    /// move to the side table with the next write; invalid parts fall back
    /// to defaults with a warning instead of keeping the daemon from starting.
    fn from_json(bytes: &[u8]) -> Result<Self, AppError> {
        let persisted: PersistedStore = serde_json::from_slice(bytes)?;
        let mut auth_methods = persisted.auth_methods;
        let parse = |alias: &str, value: &str| {
            SshAuthMethod::from_stored(value).unwrap_or_else(|| {
                tracing::warn!(alias, value, "unknown ssh auth method in store; using keys");
                SshAuthMethod::Key
            })
        };
        let managed_hosts = persisted
            .managed_hosts
            .into_iter()
            .map(|mut host| {
                let stored = auth_methods.remove(&host.alias);
                // An inline value was written by v2.3.1 after it dropped the
                // table, so it is the newer one.
                let method = match host.auth_method.take().or(stored) {
                    Some(value) => parse(&host.alias, &value),
                    None => SshAuthMethod::Key,
                };
                host.into_host(method)
            })
            .collect();
        Ok(Self {
            managed_hosts,
            ftp_sites: persisted.ftp_sites,
            agent_access: persisted.agent_access,
            connection: persisted.connection.map(Into::into).unwrap_or_default(),
            forwards: dedup_forwards(match persisted.forwards {
                Some(serde_json::Value::Array(entries)) => entries
                    .into_iter()
                    .filter_map(forward::from_stored)
                    .collect(),
                None | Some(serde_json::Value::Null) => Vec::new(),
                Some(_) => {
                    tracing::warn!("ignoring stored ssh forwards that are not a list");
                    Vec::new()
                }
            }),
            ip_pool: persisted
                .ip_pool
                .map(StoredIpPool::from_stored)
                .unwrap_or_default(),
            agent_output_filter: persisted
                .agent_output_filter
                .map(AgentOutputFilter::from_stored)
                .unwrap_or_default(),
            agent_local: persisted
                .agent_local
                .map(AgentLocal::from_stored)
                .unwrap_or_default(),
        })
    }

    fn to_json(&self) -> Result<Vec<u8>, AppError> {
        // Derived from the hosts on every write, so renames and deletes
        // carry over to the table.
        let persisted = PersistedStoreRef {
            managed_hosts: self
                .managed_hosts
                .iter()
                .map(config_file::StoredHost::from_host)
                .collect(),
            ftp_sites: &self.ftp_sites,
            agent_access: &self.agent_access,
            auth_methods: self
                .managed_hosts
                .iter()
                .filter(|host| host.auth_method != SshAuthMethod::Key)
                .map(|host| (host.alias.as_str(), host.auth_method.as_str()))
                .collect(),
            connection: &self.connection,
            forwards: &self.forwards,
            ip_pool: (!self.ip_pool.is_default()).then_some(&self.ip_pool),
            agent_output_filter: &self.agent_output_filter,
            agent_local: &self.agent_local,
        };
        Ok(serde_json::to_vec_pretty(&persisted)?)
    }
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
    /// Internal only: whether a proxy program runs (it inherits ssh's env).
    #[serde(skip)]
    pub proxy_command: Option<String>,
    pub identity_files: Vec<String>,
    /// Internal only: `LocalForward`/`RemoteForward`/`DynamicForward` set
    /// in the config; ssh would add them to a manual forward's process.
    #[serde(skip)]
    pub config_forwards: bool,
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
#[derive(Clone)]
pub(crate) struct TerminalCommand {
    pub program: String,
    pub args: Vec<OsString>,
    /// Complete environment; the daemon's own environment is not inherited.
    pub env: Vec<(String, OsString)>,
    pub cwd: PathBuf,
    pub ssh_host: String,
}

/// Environment values are left out: an ssh child's environment may carry
/// credentials.
impl std::fmt::Debug for TerminalCommand {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TerminalCommand")
            .field("program", &self.program)
            .field("args", &self.args)
            .field(
                "env",
                &self.env.iter().map(|(key, _)| key).collect::<Vec<_>>(),
            )
            .field("cwd", &self.cwd)
            .field("ssh_host", &self.ssh_host)
            .finish()
    }
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
    /// Aliases from the user's `~/.ssh/config`, re-read only when it or a
    /// file it includes changed.
    user_aliases: Arc<config_file::AliasCache>,
    /// Logins agents wait on (see [`login`]).
    logins: login::LoginBroker,
    launcher: Launcher,
    forwards: ForwardRuntime,
    pool: PoolRuntime,
    /// Serializes pool reconciliation, so the latest store always wins.
    pool_sync: Mutex<()>,
}

/// Builds ssh commands that read only TodeX's wrapper config. Cheap to
/// clone, so long-running forward tasks need no handle on the service.
#[derive(Clone)]
pub(crate) struct Launcher {
    ssh_bin: String,
    wrapper: PathBuf,
    /// Long-running children, recorded so a later daemon can end orphans.
    children: Arc<tunnel::ChildRegistry>,
}

impl Launcher {
    /// `ssh -F <wrapper> <control> <mode options>`; callers append the rest.
    pub(crate) fn command(&self, mode: SshMode, control: Vec<OsString>) -> Command {
        let mut command = external_command::secure_command(&self.ssh_bin);
        command.args(self.options(mode, control));
        if mode == SshMode::Askpass {
            // Use SSH_ASKPASS even without a display or with a tty.
            command.env("SSH_ASKPASS_REQUIRE", "force");
        }
        command
    }

    /// `control` comes right after `-F`, before any other `-o`: for options
    /// given more than once, OpenSSH uses the first value.
    fn options(&self, mode: SshMode, control: Vec<OsString>) -> Vec<OsString> {
        let mut args: Vec<OsString> = vec!["-F".into(), self.wrapper.clone().into()];
        args.extend(control);
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
            // The parent holds the managed config, host store and control
            // sockets, so it is owner-only too (also tightening older data
            // directories created with the default umask).
            // Password files left by a daemon that did not exit cleanly
            // are removed.
            let dir = dir.clone();
            let control_dir = control_dir.clone();
            tokio::task::spawn_blocking(move || {
                secure_fs::ensure_owner_only_dir(&dir)?;
                secure_fs::ensure_owner_only_dir(&control_dir)?;
                // Forwards and pool masters of a daemon that was killed.
                tunnel::ChildRegistry::reap_stale(
                    &dir.join(CHILDREN_FILE),
                    &dir.join(WRAPPER_FILE),
                );
                askpass::purge(&dir.join(ASKPASS_DIR))
            })
            .await
            .map_err(|error| AppError::Anyhow(error.into()))??;
        }
        let store = match tokio::fs::read(dir.join(STORE_FILE)).await {
            Ok(bytes) => SshStore::from_json(&bytes)?,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => SshStore::default(),
            Err(error) => return Err(error.into()),
        };
        let multiplex = cfg!(unix) && control_dir.as_os_str().len() <= MAX_CONTROL_DIR_LEN;
        let launcher = Launcher {
            ssh_bin: ssh_bin.clone(),
            wrapper: dir.join(WRAPPER_FILE),
            children: Arc::new(tunnel::ChildRegistry::new(
                cfg!(unix).then(|| dir.join(CHILDREN_FILE)),
            )),
        };
        let pool = {
            let launcher = launcher.clone();
            let dir = dir.clone();
            tokio::task::spawn_blocking(move || PoolRuntime::new(launcher, &dir))
                .await
                .map_err(|error| AppError::Anyhow(error.into()))?
        };
        let service = Self {
            inner: Arc::new(Inner {
                dir,
                ssh_bin,
                home,
                multiplex,
                store: RwLock::new(store),
                resolve_cache: Mutex::new(HashMap::new()),
                user_aliases: Arc::default(),
                logins: login::LoginBroker::default(),
                launcher,
                forwards: ForwardRuntime::default(),
                pool,
                pool_sync: Mutex::new(()),
            }),
        };
        {
            let store = service.inner.store.read().await;
            service.write_generated_files(&store).await?;
            // Rewritten in the current format (migrating older fields);
            // the store loaded fine, so a failed write need not stop ssh.
            if let Err(error) = service.write_store(&store).await {
                tracing::warn!(%error, "cannot rewrite ssh store");
            }
        }
        // Listeners of an enabled pool come back after a restart.
        service.sync_pool().await;
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

    /// Makes `next` the store: `store.json` first, then the generated ssh
    /// config. If any write fails, the files are rewritten from `current`,
    /// which stays in memory, so what the API reports matches what ssh
    /// reads. Called under the store write lock.
    async fn save(&self, current: &mut SshStore, next: SshStore) -> Result<(), AppError> {
        let written = match self.write_store(&next).await {
            Ok(()) => self.write_generated_files(&next).await,
            Err(error) => Err(error),
        };
        if let Err(error) = written {
            let restored = match self.write_store(current).await {
                Ok(()) => self.write_generated_files(current).await,
                Err(error) => Err(error),
            };
            if let Err(restore_error) = restored {
                tracing::warn!(%restore_error, "cannot restore ssh config files");
            }
            self.inner.resolve_cache.lock().await.clear();
            return Err(error);
        }
        *current = next;
        Ok(())
    }

    async fn write_store(&self, store: &SshStore) -> Result<(), AppError> {
        let store_json = store.to_json()?;
        let path = self.inner.dir.join(STORE_FILE);
        tokio::task::spawn_blocking(move || secure_fs::write_owner_only_atomic(&path, &store_json))
            .await
            .map_err(|error| AppError::Anyhow(error.into()))??;
        Ok(())
    }

    /// Writes `hosts.conf`, `defaults.conf` and the `-F` wrapper.
    async fn write_generated_files(&self, store: &SshStore) -> Result<(), AppError> {
        let managed_path = self.inner.dir.join(MANAGED_HOSTS_FILE);
        let defaults_path = self.inner.dir.join(DEFAULTS_FILE);
        let wrapper = config_file::render_wrapper_config(
            &managed_path,
            &self
                .user_config()
                .unwrap_or_else(|| PathBuf::from("/nonexistent/.ssh/config")),
            &defaults_path,
            system_config().as_deref(),
        );
        let managed = config_file::render_managed_hosts(&store.managed_hosts);
        let defaults = config_file::render_connection_defaults(&store.connection);
        let dir = self.inner.dir.clone();
        tokio::task::spawn_blocking(move || -> std::io::Result<()> {
            secure_fs::write_owner_only_atomic(&dir.join(MANAGED_HOSTS_FILE), managed.as_bytes())?;
            secure_fs::write_owner_only_atomic(&dir.join(DEFAULTS_FILE), defaults.as_bytes())?;
            secure_fs::write_owner_only_atomic(&dir.join(WRAPPER_FILE), wrapper.as_bytes())
        })
        .await
        .map_err(|error| AppError::Anyhow(error.into()))??;
        self.inner.resolve_cache.lock().await.clear();
        Ok(())
    }

    /// An `ssh` command that reads only TodeX's wrapper config. Callers append
    /// `--`, the alias, and an optional remote command.
    pub(crate) fn command(&self, mode: SshMode) -> Command {
        let control = if self.inner.multiplex {
            self.control_options(mode)
        } else {
            Vec::new()
        };
        self.command_with(mode, control)
    }

    /// [`Self::command`] that neither uses nor becomes a shared master, so
    /// the connection really authenticates.
    fn unshared_command(&self, mode: SshMode) -> Command {
        self.command_with(
            mode,
            ["-o", "ControlMaster=no", "-o", "ControlPath=none"]
                .map(OsString::from)
                .into(),
        )
    }

    fn command_with(&self, mode: SshMode, control: Vec<OsString>) -> Command {
        self.inner.launcher.command(mode, control)
    }

    /// `ssh -tt <alias>` for a PTY, with the same sanitized environment as
    /// [`Self::command`]. Host-key and password prompts reach the user.
    ///
    /// With `password`, the askpass helper answers the one password prompt
    /// instead. Unknown host keys are then rejected rather than asked about,
    /// so the client can retry without a password to confirm them. Hosts
    /// behind ProxyJump/ProxyCommand ignore the password and prompt as usual.
    /// The caller keeps the returned secret until ssh started, then hands it
    /// to [`AskpassSecret::expire_after`] with [`TERMINAL_ASKPASS_DEADLINE`].
    pub(crate) async fn terminal_command(
        &self,
        alias: &str,
        password: Option<&str>,
    ) -> Result<(TerminalCommand, Option<AskpassSecret>), AppError> {
        let mut args = self.options(SshMode::Interactive);
        let mut env: Vec<(String, OsString)> = external_command::inherited_env()
            .into_iter()
            .map(|(key, value)| (key.to_owned(), value))
            .collect();
        if let Some(password) = password {
            // Checked here: the fallback below must not swallow it.
            validate_password(password)?;
        }
        let askpass = match password {
            Some(password) => match self.askpass_env(alias, password).await {
                Ok(vars) => Some(vars),
                // Behind a jump host or proxy the password must not travel;
                // the user signs in at the terminal prompt instead.
                Err(AppError::InvalidRequest(_)) => None,
                Err(error) => return Err(error),
            },
            None => None,
        };
        let secret = askpass.map(|(vars, secret)| {
            env.extend(vars.map(|(key, value)| (key.to_owned(), value)));
            env.push(("SSH_ASKPASS_REQUIRE".to_owned(), "force".into()));
            args.extend(
                [
                    "-o",
                    "NumberOfPasswordPrompts=1",
                    "-o",
                    "StrictHostKeyChecking=yes",
                ]
                .map(OsString::from),
            );
            secret
        });
        args.extend(["-tt".into(), "--".into(), alias.into()]);
        let command = TerminalCommand {
            program: self.inner.ssh_bin.clone(),
            args,
            env,
            cwd: self
                .inner
                .home
                .clone()
                .unwrap_or_else(|| self.inner.dir.clone()),
            ssh_host: alias.to_owned(),
        };
        Ok((command, secret))
    }

    /// Environment that lets one ssh child answer a password prompt with
    /// `password` through the askpass helper, and the guard of the file that
    /// holds it (the environment only names the file). Refused for hosts
    /// behind a jump host or proxy program: those inherit ssh's environment,
    /// and a jump host's own password prompt would receive the secret.
    pub(crate) async fn askpass_env(
        &self,
        alias: &str,
        password: &str,
    ) -> Result<([(&'static str, OsString); 2], AskpassSecret), AppError> {
        validate_password(password)?;
        // Fresh, not cached: a ProxyJump added within the cache lifetime
        // must not see the password.
        self.require_host(alias).await?;
        let resolved = self
            .resolve_with(alias, false)
            .await
            .map_err(AppError::InvalidRequest)?;
        if resolved.proxy_jump.is_some() || resolved.proxy_command.is_some() {
            return Err(AppError::InvalidRequest(
                "password sign-in is not supported for hosts behind ProxyJump or \
                 ProxyCommand; log in from the TodeX Terminal first and later \
                 sessions reuse that connection"
                    .to_owned(),
            ));
        }
        let program = askpass_program()?;
        let dir = self.inner.dir.join(ASKPASS_DIR);
        let password = zeroize::Zeroizing::new(password.to_owned());
        let secret = tokio::task::spawn_blocking(move || AskpassSecret::create(&dir, &password))
            .await
            .map_err(|error| AppError::Anyhow(error.into()))??;
        Ok((
            [
                ("SSH_ASKPASS", program.into()),
                (askpass::FILE_ENV, secret.path().into()),
            ],
            secret,
        ))
    }

    fn options(&self, mode: SshMode) -> Vec<OsString> {
        let control = if self.inner.multiplex {
            self.control_options(mode)
        } else {
            Vec::new()
        };
        self.options_with(mode, control)
    }

    /// `control` comes right after `-F`, before any other `-o`: for options
    /// given more than once, OpenSSH uses the first value.
    fn options_with(&self, mode: SshMode, control: Vec<OsString>) -> Vec<OsString> {
        self.inner.launcher.options(mode, control)
    }

    fn control_options(&self, mode: SshMode) -> Vec<OsString> {
        // An askpass process may use an existing master but must not become
        // a long-lived one that outlives the request.
        let master = if mode == SshMode::Askpass {
            "no"
        } else {
            "auto"
        };
        self.control_options_with(master)
    }

    fn control_options_with(&self, master: &str) -> Vec<OsString> {
        let control_path = self.inner.dir.join(CONTROL_DIR).join("%C");
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

    /// Whether a shared master for `alias` is up (`ssh -O check`, which
    /// only talks to the local control socket).
    pub(crate) async fn master_alive(&self, alias: &str) -> bool {
        if !self.inner.multiplex {
            return false;
        }
        let mut command = external_command::secure_command(&self.inner.ssh_bin);
        command
            .arg("-F")
            .arg(self.wrapper_path())
            .args(self.control_options(SshMode::Batch))
            .args(["-O", "check", "--", alias]);
        prepare_captured(&mut command, false);
        matches!(
            external_command::run(command, None, RESOLVE_LIMITS).await,
            Ok(output) if output.status.success()
        )
    }

    /// Whether a password can open a shared master for `alias`: connection
    /// sharing works here and the host is not behind a jump host or proxy
    /// program (see [`Self::askpass_env`]).
    pub(crate) async fn password_login_supported(&self, alias: &str) -> bool {
        self.inner.multiplex
            && matches!(
                self.resolve_with(alias, false).await,
                Ok(resolved) if resolved.proxy_jump.is_none() && resolved.proxy_command.is_none()
            )
    }

    /// Logs in to `alias` with `password` and leaves the connection as the
    /// shared master, so later terminal, SFTP and agent commands reuse it
    /// until it has been idle for [`CONTROL_PERSIST_SECONDS`]. An existing
    /// master is reused instead (the password then goes unused).
    pub(crate) async fn connect_shared(
        &self,
        alias: &str,
        password: &str,
    ) -> Result<ConnectionTestResult, AppError> {
        if !self.inner.multiplex {
            return Err(AppError::Unsupported(
                "shared SSH connections are not available on this system".to_owned(),
            ));
        }
        // The password file lives until ssh exited; the master it leaves
        // behind has authenticated by then.
        let (vars, _secret) = self.askpass_env(alias, password).await?;
        let mut command = self.command_with(SshMode::Askpass, self.control_options_with("auto"));
        command.envs(vars);
        command.arg("--").arg(alias).arg("exit 0");
        prepare_captured(&mut command, false);
        let started = Instant::now();
        let outcome = external_command::run(command, None, TEST_LIMITS).await;
        test_result(outcome, started.elapsed())
    }

    /// The login broker agents and clients meet at.
    pub(crate) fn logins(&self) -> &login::LoginBroker {
        &self.inner.logins
    }

    pub async fn list_hosts(&self) -> Result<Vec<SshHostView>, AppError> {
        let mut views = self.host_views().await?;
        self.resolve_views(&mut views).await;
        Ok(views)
    }

    /// Managed hosts plus `~/.ssh/config` aliases, without running ssh. Only
    /// config files are read, so this is cheap enough for every request.
    async fn host_views(&self) -> Result<Vec<SshHostView>, AppError> {
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
            let cache = self.inner.user_aliases.clone();
            let discovered = tokio::task::spawn_blocking(move || cache.discover(&config, &home))
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
        Ok(views)
    }

    async fn resolve_views(&self, views: &mut [SshHostView]) {
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
    }

    /// Fails unless `alias` is a known host. Every ssh entry point checks this
    /// so clients and agents can only reach configured hosts. Does not run
    /// `ssh -G`, so it never blocks on slow resolution.
    pub(crate) async fn require_host(&self, alias: &str) -> Result<(), AppError> {
        if !config_file::valid_alias(alias) {
            return Err(AppError::InvalidRequest(format!(
                "invalid ssh host: {alias}"
            )));
        }
        if self
            .host_views()
            .await?
            .iter()
            .any(|view| view.alias == alias)
        {
            Ok(())
        } else {
            Err(AppError::NotFound(format!("ssh host {alias}")))
        }
    }

    async fn resolve(&self, alias: &str) -> Result<ResolvedHost, String> {
        self.resolve_with(alias, true).await
    }

    /// `ssh -G` for `alias`; `cached` allows a result up to [`RESOLVE_TTL`]
    /// old. A fresh result refreshes the cache either way.
    async fn resolve_with(&self, alias: &str, cached: bool) -> Result<ResolvedHost, String> {
        if cached {
            if let Some((at, result)) = self.inner.resolve_cache.lock().await.get(alias) {
                if at.elapsed() < RESOLVE_TTL {
                    return result.clone();
                }
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
        let mut next = store.clone();
        next.managed_hosts.push(host.clone());
        self.save(&mut store, next).await?;
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
        let mut next = store.clone();
        let Some(slot) = next.managed_hosts.iter_mut().find(|h| h.alias == alias) else {
            return Err(AppError::NotFound(format!("managed ssh host {alias}")));
        };
        *slot = host.clone();
        if host.alias != alias && next.agent_access.remove(alias) {
            next.agent_access.insert(host.alias.clone());
        }
        if host.alias != alias {
            // Forwards and the pool address follow a renamed host.
            for forward in next.forwards.iter_mut().filter(|f| f.alias == alias) {
                forward.alias = host.alias.clone();
            }
            for assignment in next
                .ip_pool
                .assignments
                .iter_mut()
                .filter(|a| a.alias == alias)
            {
                assignment.alias = host.alias.clone();
            }
        }
        self.save(&mut store, next).await?;
        drop(store);
        // A new name or sign-in method gets a fresh pool master.
        self.sync_pool().await;
        Ok(host)
    }

    /// Deletes a managed host with its forwards and pool address (stopped
    /// and closed first).
    pub async fn delete_host(&self, alias: &str) -> Result<(), AppError> {
        let (forward_ids, pool_ips) = {
            let store = self.inner.store.read().await;
            if !store.managed_hosts.iter().any(|h| h.alias == alias) {
                return Err(AppError::NotFound(format!("managed ssh host {alias}")));
            }
            (
                store
                    .forwards
                    .iter()
                    .filter(|f| f.alias == alias)
                    .map(|f| f.id.clone())
                    .collect::<Vec<_>>(),
                store
                    .ip_pool
                    .assignments
                    .iter()
                    .filter(|a| a.alias == alias)
                    .map(|a| a.ip)
                    .collect::<Vec<_>>(),
            )
        };
        for id in &forward_ids {
            self.inner.forwards.forget(id).await;
        }
        for ip in pool_ips {
            self.inner.pool.remove(ip).await;
        }
        let mut store = self.inner.store.write().await;
        let mut next = store.clone();
        next.managed_hosts.retain(|h| h.alias != alias);
        if next.managed_hosts.len() == store.managed_hosts.len() {
            return Err(AppError::NotFound(format!("managed ssh host {alias}")));
        }
        next.agent_access.remove(alias);
        next.forwards.retain(|f| f.alias != alias);
        next.ip_pool.assignments.retain(|a| a.alias != alias);
        let result = self.save(&mut store, next).await;
        drop(store);
        if result.is_ok() {
            // A start that slipped in before the save cannot find the
            // forwards any more, but may have begun: end it.
            for id in &forward_ids {
                self.inner.forwards.forget(id).await;
            }
        }
        // Restores the assignment if the write failed.
        self.sync_pool().await;
        result
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
        let cache = self.inner.user_aliases.clone();
        let discovered = tokio::task::spawn_blocking(move || cache.discover(&config, &home))
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
        // Revoking must work even after the alias left ~/.ssh/config, or a
        // stale grant would come back when the alias is defined again.
        if enabled {
            self.require_host(alias).await?;
        }
        let mut store = self.inner.store.write().await;
        let mut next = store.clone();
        let changed = if enabled {
            next.agent_access.insert(alias.to_owned())
        } else {
            next.agent_access.remove(alias)
        };
        if changed {
            self.save(&mut store, next).await?;
        }
        Ok(())
    }

    pub async fn connection_settings(&self) -> ConnectionSettings {
        self.inner.store.read().await.connection.clone()
    }

    /// Applies to ssh processes started afterwards; a running shared master
    /// keeps its options until it exits.
    pub async fn set_connection_settings(
        &self,
        settings: ConnectionSettings,
    ) -> Result<ConnectionSettings, AppError> {
        settings.validate()?;
        let mut store = self.inner.store.write().await;
        if store.connection != settings {
            let next = SshStore {
                connection: settings,
                ..store.clone()
            };
            self.save(&mut store, next).await?;
        }
        Ok(store.connection.clone())
    }

    pub async fn agent_output_filter(&self) -> AgentOutputFilter {
        self.inner.store.read().await.agent_output_filter
    }

    /// Applies from the next `ssh_exec` result on.
    pub async fn set_agent_output_filter(
        &self,
        filter: AgentOutputFilter,
    ) -> Result<AgentOutputFilter, AppError> {
        let mut store = self.inner.store.write().await;
        if store.agent_output_filter != filter {
            let next = SshStore {
                agent_output_filter: filter,
                ..store.clone()
            };
            self.save(&mut store, next).await?;
        }
        Ok(store.agent_output_filter)
    }

    pub async fn agent_local(&self) -> AgentLocal {
        self.inner.store.read().await.agent_local
    }

    /// Turning it off applies to the next call; turning it on, to
    /// conversations started afterwards (the MCP server is injected then).
    pub async fn set_agent_local(&self, settings: AgentLocal) -> Result<AgentLocal, AppError> {
        let mut store = self.inner.store.write().await;
        if store.agent_local != settings {
            let next = SshStore {
                agent_local: settings,
                ..store.clone()
            };
            self.save(&mut store, next).await?;
        }
        Ok(store.agent_local)
    }

    /// Whether a granted host is usable by agents: one named like
    /// [`AGENT_LOCAL_HOST`] is not while the local host is on.
    async fn agent_visible(&self, view: &SshHostView) -> bool {
        view.agent_access
            && !(is_agent_local_alias(&view.alias) && self.agent_local().await.enabled)
    }

    /// Agent-enabled hosts that still exist, with resolved parameters.
    pub async fn agent_hosts(&self) -> Result<Vec<SshHostView>, AppError> {
        let mut views = Vec::new();
        for view in self.host_views().await? {
            if self.agent_visible(&view).await {
                views.push(view);
            }
        }
        self.resolve_views(&mut views).await;
        Ok(views)
    }

    /// Whether agents may use `alias` right now (granted and still defined).
    pub async fn is_agent_host(&self, alias: &str) -> Result<bool, AppError> {
        for view in self.host_views().await? {
            if view.alias == alias && self.agent_visible(&view).await {
                return Ok(true);
            }
        }
        Ok(false)
    }

    /// Whether any existing host is granted to agents; decides MCP injection.
    pub async fn has_agent_hosts(&self) -> bool {
        if self.inner.store.read().await.agent_access.is_empty() {
            return false;
        }
        match self.host_views().await {
            Ok(views) => {
                for view in &views {
                    if self.agent_visible(view).await {
                        return true;
                    }
                }
                false
            }
            Err(error) => {
                tracing::warn!(%error, "cannot read ssh hosts; agent ssh tools disabled");
                false
            }
        }
    }

    /// Runs `exit 0` on `alias`. Without a password only keys and agents are
    /// tried; with one, a single password prompt is answered on a connection
    /// of its own: reusing a master would report success for any password.
    pub async fn test_connection(
        &self,
        alias: &str,
        password: Option<&str>,
    ) -> Result<ConnectionTestResult, AppError> {
        self.require_host(alias).await?;
        // The password file lives until ssh exited.
        let (mut command, _secret) = match password {
            Some(password) => {
                let mut command = self.unshared_command(SshMode::Askpass);
                let (vars, secret) = self.askpass_env(alias, password).await?;
                command.envs(vars);
                (command, Some(secret))
            }
            None => (self.command(SshMode::Batch), None),
        };
        // `exit 0` works in sh, cmd.exe and PowerShell login shells alike.
        command.arg("--").arg(alias).arg("exit 0");
        prepare_captured(&mut command, false);
        let started = Instant::now();
        let outcome = external_command::run(command, None, TEST_LIMITS).await;
        test_result(outcome, started.elapsed())
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
        let mut next = store.clone();
        match next.ftp_sites.iter_mut().find(|s| s.id == site.id) {
            Some(slot) => *slot = site.clone(),
            None if id.is_some() => {
                return Err(AppError::NotFound(format!("ftp site {}", site.id)))
            }
            None => {
                if next.ftp_sites.len() >= MAX_FTP_SITES {
                    return Err(AppError::ResourceExhausted("too many ftp sites".to_owned()));
                }
                next.ftp_sites.push(site.clone());
            }
        }
        self.save(&mut store, next).await?;
        Ok(site)
    }

    pub async fn delete_ftp_site(&self, id: &str) -> Result<(), AppError> {
        let mut store = self.inner.store.write().await;
        let mut next = store.clone();
        next.ftp_sites.retain(|site| site.id != id);
        if next.ftp_sites.len() == store.ftp_sites.len() {
            return Err(AppError::NotFound(format!("ftp site {id}")));
        }
        self.save(&mut store, next).await
    }

    /// Stops every forward and closes the IP pool (daemon shutdown).
    pub async fn shutdown(&self) {
        tokio::join!(self.inner.forwards.stop_all(), self.inner.pool.shutdown());
    }

    /// Listeners of the daemon itself that forwards must not take.
    pub(crate) fn set_reserved_listeners(&self, listeners: Vec<forward::ReservedListener>) {
        self.inner.forwards.set_reserved(listeners);
    }

    // ------------------------------------------------------------ forwards

    fn forward_view(&self, spec: &ForwardSpec) -> ForwardView {
        ForwardView {
            spec: spec.clone(),
            run: self.inner.forwards.info(&spec.id),
        }
    }

    async fn forward_spec(&self, id: &str) -> Result<ForwardSpec, AppError> {
        self.inner
            .store
            .read()
            .await
            .forwards
            .iter()
            .find(|f| f.id == id)
            .cloned()
            .ok_or_else(|| AppError::NotFound(format!("ssh forward {id}")))
    }

    pub(crate) async fn list_forwards(&self) -> Vec<ForwardView> {
        let store = self.inner.store.read().await;
        store
            .forwards
            .iter()
            .map(|f| self.forward_view(f))
            .collect()
    }

    pub(crate) async fn create_forward(
        &self,
        input: ForwardInput,
    ) -> Result<ForwardView, AppError> {
        let spec = forward::normalize(None, input)?;
        self.require_host(&spec.alias).await?;
        let mut store = self.inner.store.write().await;
        if store.forwards.len() >= forward::MAX_FORWARDS {
            return Err(AppError::Conflict(format!(
                "at most {} forwards can be saved",
                forward::MAX_FORWARDS
            )));
        }
        let mut next = store.clone();
        next.forwards.push(spec.clone());
        self.save(&mut store, next).await?;
        Ok(self.forward_view(&spec))
    }

    pub(crate) async fn update_forward(
        &self,
        id: &str,
        input: ForwardInput,
    ) -> Result<ForwardView, AppError> {
        let spec = forward::normalize(Some(id.to_owned()), input)?;
        self.require_host(&spec.alias).await?;
        let mut store = self.inner.store.write().await;
        let mut next = store.clone();
        let Some(slot) = next.forwards.iter_mut().find(|f| f.id == id) else {
            return Err(AppError::NotFound(format!("ssh forward {id}")));
        };
        if self.inner.forwards.is_active(id) {
            return Err(AppError::Conflict(
                "stop the forward before changing it".to_owned(),
            ));
        }
        *slot = spec.clone();
        self.save(&mut store, next).await?;
        Ok(self.forward_view(&spec))
    }

    pub(crate) async fn delete_forward(&self, id: &str) -> Result<(), AppError> {
        self.forward_spec(id).await?;
        self.inner.forwards.forget(id).await;
        let mut store = self.inner.store.write().await;
        let mut next = store.clone();
        next.forwards.retain(|f| f.id != id);
        if next.forwards.len() == store.forwards.len() {
            return Err(AppError::NotFound(format!("ssh forward {id}")));
        }
        self.save(&mut store, next).await?;
        drop(store);
        // Starts begin under the store read lock and re-check the spec, so
        // none can begin after the save; one that began before it ends here.
        self.inner.forwards.forget(id).await;
        Ok(())
    }

    /// Starts a forward and waits until it is ready or failed. Runtime
    /// failures are reported in the returned state, not as errors.
    pub(crate) async fn start_forward(
        &self,
        id: &str,
        password: Option<&str>,
    ) -> Result<ForwardView, AppError> {
        let spec = self.forward_spec(id).await?;
        self.require_host(&spec.alias).await?;
        if let Some(password) = password {
            validate_password(password)?;
        }
        // Begun under the store lock with the spec as stored right now, so
        // a concurrent update (which refuses active forwards) or delete
        // cannot leave a run of an outdated or deleted spec.
        let generation = {
            let store = self.inner.store.read().await;
            match store.forwards.iter().find(|f| f.id == id) {
                Some(current) if *current == spec => self.inner.forwards.begin(&spec)?,
                Some(_) => {
                    return Err(AppError::Conflict(
                        "the forward changed while starting; try again".to_owned(),
                    ))
                }
                None => return Err(AppError::NotFound(format!("ssh forward {id}"))),
            }
        };
        let Some(generation) = generation else {
            return Ok(self.forward_view(&spec));
        };
        // ssh adds forwards from ssh_config to every connection to the
        // host; `ClearAllForwardings` would drop this forward too.
        if let Ok(resolved) = self.resolve_with(&spec.alias, false).await {
            if resolved.config_forwards {
                self.inner.forwards.fail(
                    id,
                    generation,
                    SshFailureKind::Other,
                    format!(
                        "{} defines LocalForward, RemoteForward or DynamicForward in ssh_config, \
                         which ssh would also open with this forward; remove them from the \
                         host's config or use the IP pool for this host",
                        spec.alias
                    ),
                );
                return Ok(self.forward_view(&spec));
            }
        }
        let (command, secret) = match self.forward_command(&spec, password).await {
            Ok(built) => built,
            Err(error) => {
                self.inner.forwards.stop(id).await;
                return Err(error);
            }
        };
        self.inner
            .forwards
            .run(
                &spec,
                generation,
                command,
                secret,
                self.inner.launcher.children.clone(),
            )
            .await;
        Ok(self.forward_view(&spec))
    }

    /// The dedicated ssh process of a forward: no shared master either way.
    async fn forward_command(
        &self,
        spec: &ForwardSpec,
        password: Option<&str>,
    ) -> Result<(Command, Option<AskpassSecret>), AppError> {
        let (mut command, secret) = match password {
            Some(password) => {
                let (vars, secret) = self.askpass_env(&spec.alias, password).await?;
                let mut command = self.unshared_command(SshMode::Askpass);
                command.envs(vars);
                (command, Some(secret))
            }
            None => (self.unshared_command(SshMode::Batch), None),
        };
        // Debug logging reports when the listener or the server-side
        // forward is ready; it never contains the password.
        command.args([
            "-N",
            "-o",
            "ExitOnForwardFailure=yes",
            "-o",
            "LogLevel=DEBUG1",
        ]);
        command.args(forward::forward_args(spec));
        command.arg("--").arg(&spec.alias);
        Ok((command, secret))
    }

    pub(crate) async fn stop_forward(&self, id: &str) -> Result<ForwardView, AppError> {
        let spec = self.forward_spec(id).await?;
        self.inner.forwards.stop(id).await;
        Ok(self.forward_view(&spec))
    }

    // ------------------------------------------------------------- IP pool

    /// Applies the stored pool to the runtime.
    async fn sync_pool(&self) {
        let _guard = self.inner.pool_sync.lock().await;
        let (pool, hosts) = {
            let store = self.inner.store.read().await;
            let hosts: HashMap<String, HostTraits> = store
                .managed_hosts
                .iter()
                .map(|host| {
                    (
                        host.alias.clone(),
                        HostTraits {
                            password: host.auth_method == SshAuthMethod::Password,
                        },
                    )
                })
                .collect();
            (store.ip_pool.clone(), hosts)
        };
        self.inner.pool.reconcile(&pool, &hosts).await;
    }

    pub(crate) async fn ip_pool(&self) -> PoolView {
        let store = self.inner.store.read().await;
        self.inner.pool.view(&store.ip_pool)
    }

    pub(crate) async fn set_ip_pool(&self, input: PoolConfigInput) -> Result<PoolView, AppError> {
        {
            let mut store = self.inner.store.write().await;
            let pool = ip_pool::apply_config(&store.ip_pool, input)?;
            if pool != store.ip_pool {
                let next = SshStore {
                    ip_pool: pool,
                    ..store.clone()
                };
                self.save(&mut store, next).await?;
            }
        }
        self.sync_pool().await;
        Ok(self.ip_pool().await)
    }

    async fn assignment_view(&self, ip: Ipv4Addr) -> Result<AssignmentView, AppError> {
        let store = self.inner.store.read().await;
        let stored = store.ip_pool.assignment(ip)?;
        Ok(self.inner.pool.assignment_view(&store.ip_pool, stored))
    }

    pub(crate) async fn create_ip_assignment(
        &self,
        input: AssignmentInput,
    ) -> Result<AssignmentView, AppError> {
        self.require_host(input.alias.trim()).await?;
        let ip = {
            let mut store = self.inner.store.write().await;
            let assignment = ip_pool::new_assignment(&store.ip_pool, input)?;
            let ip = assignment.ip;
            let mut next = store.clone();
            next.ip_pool.assignments.push(assignment);
            self.save(&mut store, next).await?;
            ip
        };
        self.sync_pool().await;
        self.assignment_view(ip).await
    }

    pub(crate) async fn update_ip_assignment(
        &self,
        ip: Ipv4Addr,
        input: AssignmentUpdate,
    ) -> Result<AssignmentView, AppError> {
        {
            let mut store = self.inner.store.write().await;
            let updated = ip_pool::update_assignment(&store.ip_pool, ip, input)?;
            let mut next = store.clone();
            if let Some(slot) = next.ip_pool.assignments.iter_mut().find(|a| a.ip == ip) {
                *slot = updated;
            }
            if next.ip_pool != store.ip_pool {
                self.save(&mut store, next).await?;
            }
        }
        self.sync_pool().await;
        self.assignment_view(ip).await
    }

    pub(crate) async fn delete_ip_assignment(&self, ip: Ipv4Addr) -> Result<(), AppError> {
        self.assignment_view(ip).await?;
        self.inner.pool.remove(ip).await;
        let result = {
            let mut store = self.inner.store.write().await;
            let mut next = store.clone();
            next.ip_pool.assignments.retain(|a| a.ip != ip);
            self.save(&mut store, next).await
        };
        self.sync_pool().await;
        result
    }

    /// Starts the assignment's master now; with a password for password
    /// hosts. Sign-in failures are reported in the returned state.
    pub(crate) async fn connect_ip_assignment(
        &self,
        ip: Ipv4Addr,
        password: Option<&str>,
    ) -> Result<AssignmentView, AppError> {
        let (alias, enabled) = {
            let store = self.inner.store.read().await;
            let stored = store.ip_pool.assignment(ip)?;
            (stored.alias.clone(), store.ip_pool.enabled)
        };
        if !enabled || !ip_pool::SUPPORTED {
            return Err(AppError::Conflict("the IP pool is disabled".to_owned()));
        }
        self.require_host(&alias).await?;
        let askpass = match password {
            Some(password) => Some(self.askpass_env(&alias, password).await?),
            None => None,
        };
        self.inner.pool.connect(ip, askpass).await;
        self.assignment_view(ip).await
    }

    pub(crate) async fn disconnect_ip_assignment(
        &self,
        ip: Ipv4Addr,
    ) -> Result<AssignmentView, AppError> {
        self.assignment_view(ip).await?;
        self.inner.pool.disconnect(ip).await;
        self.assignment_view(ip).await
    }
}

/// Keeps the first forward of each id.
fn dedup_forwards(forwards: Vec<ForwardSpec>) -> Vec<ForwardSpec> {
    let mut seen = std::collections::HashSet::new();
    forwards
        .into_iter()
        .filter(|forward| seen.insert(forward.id.clone()))
        .take(forward::MAX_FORWARDS)
        .collect()
}

/// Longest password accepted for askpass; far above real passwords.
pub(crate) const MAX_PASSWORD_BYTES: usize = 4096;

/// Rejects passwords ssh cannot receive intact: askpass answers one line
/// (CR/LF would cut it short) and NUL cannot be passed on. The message
/// never includes the password.
pub(crate) fn validate_password(password: &str) -> Result<(), AppError> {
    if password.len() > MAX_PASSWORD_BYTES {
        return Err(AppError::InvalidRequest(format!(
            "password must be at most {MAX_PASSWORD_BYTES} bytes"
        )));
    }
    if password.contains(['\0', '\r', '\n']) {
        return Err(AppError::InvalidRequest(
            "password must not contain NUL, CR or LF characters".to_owned(),
        ));
    }
    Ok(())
}

/// This executable, run by ssh as `SSH_ASKPASS`. Resolved once: after an
/// in-place upgrade Linux reports the running binary as `<path> (deleted)`,
/// while the path itself holds the new binary, which has the same helper.
fn askpass_program() -> Result<PathBuf, AppError> {
    static PROGRAM: std::sync::OnceLock<Result<PathBuf, String>> = std::sync::OnceLock::new();
    PROGRAM
        .get_or_init(|| {
            let path = std::env::current_exe().map_err(|error| error.to_string())?;
            Ok(path
                .to_str()
                .and_then(|raw| raw.strip_suffix(" (deleted)"))
                .map(PathBuf::from)
                .unwrap_or(path))
        })
        .clone()
        .map_err(|error| AppError::Unsupported(format!("askpass helper unavailable: {error}")))
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
            "proxycommand" if value != "none" => resolved.proxy_command = Some(value.to_owned()),
            "identityfile" => resolved.identity_files.push(value.to_owned()),
            "localforward" | "remoteforward" | "dynamicforward" => resolved.config_forwards = true,
            _ => {}
        }
    }
    resolved
}

/// What a `exit 0` run of ssh came to.
fn test_result(
    outcome: Result<external_command::CommandOutput, ExternalCommandError>,
    elapsed: Duration,
) -> Result<ConnectionTestResult, AppError> {
    let duration_ms = elapsed.as_millis() as u64;
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
    /// named `bad*` fail authentication, and `-O check <alias>` succeeds
    /// only while `<root>/master-<alias>` exists. With an askpass file it also logs
    /// its environment and whether the file existed to `askpass.log`.
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
if [ -n "$TODEX_SSH_ASKPASS_FILE" ]; then
  env >> "{askpass_log}"
  [ -f "$TODEX_SSH_ASKPASS_FILE" ] && echo "askpass-file-present" >> "{askpass_log}"
fi
last=""; for arg in "$@"; do prev="$last"; last="$arg"; done
case " $* " in
  *" -G "*) printf 'hostname %s.example\nuser ops\nport 22\nidentityfile ~/.ssh/id_ed25519\n' "$last"; exit 0;;
  *" -O check "*) [ -f "{root}/master-$last" ] && exit 0; exit 255;;
esac
case "$prev" in
  bad*) echo "ops@$prev: Permission denied (publickey)." >&2; exit 255;;
esac
exit 0
"#,
                log = root.join("ssh.log").display(),
                askpass_log = root.join("askpass.log").display(),
                root = root.display(),
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
    async fn ssh_data_dir_is_owner_only() {
        let root = std::env::temp_dir().join(format!("todex-ssh-{}", Uuid::new_v4().simple()));
        let data = root.join("data");
        // An older daemon created the directory with the default umask.
        std::fs::create_dir_all(data.join("ssh")).unwrap();
        std::fs::set_permissions(data.join("ssh"), std::fs::Permissions::from_mode(0o755)).unwrap();
        let _service = SshService::with_home(&data, "ssh".to_owned(), Some(root.join("home")))
            .await
            .unwrap();
        for dir in [data.join("ssh"), data.join("ssh").join(CONTROL_DIR)] {
            let mode = std::fs::metadata(&dir).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode, 0o700, "{}", dir.display());
        }
        let _ = std::fs::remove_dir_all(root);
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
        let web = hosts[1]
            .resolved
            .as_ref()
            .unwrap_or_else(|| panic!("{:?}", hosts[1].resolve_error));
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
    async fn the_local_host_is_opt_in_persists_and_reserves_its_name() {
        let fixture = fixture("").await;
        assert_eq!(
            fixture.service.agent_local().await,
            AgentLocal { enabled: false }
        );
        fixture.service.create_host(managed("local")).await.unwrap();
        fixture
            .service
            .set_agent_access("local", true)
            .await
            .unwrap();
        assert!(fixture.service.is_agent_host("local").await.unwrap());

        fixture
            .service
            .set_agent_local(AgentLocal { enabled: true })
            .await
            .unwrap();
        // A real host by that name is kept from agents while Local is on.
        assert!(!fixture.service.is_agent_host("local").await.unwrap());
        assert!(fixture.service.agent_hosts().await.unwrap().is_empty());
        assert!(!fixture.service.has_agent_hosts().await);

        let reloaded = SshService::with_home(
            &fixture.root.join("data"),
            "ssh".into(),
            Some(fixture.root.join("home")),
        )
        .await
        .unwrap();
        assert!(reloaded.agent_local().await.enabled);
        assert_eq!(
            AgentLocal::from_stored(serde_json::json!("garbage")),
            AgentLocal { enabled: false }
        );
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
    async fn stale_agent_grants_are_revocable_and_ignored() {
        let fixture = fixture("Host web\n").await;
        fixture.service.set_agent_access("web", true).await.unwrap();
        assert!(fixture.service.has_agent_hosts().await);
        assert!(fixture.service.is_agent_host("web").await.unwrap());

        // The alias disappears from ~/.ssh/config.
        std::fs::write(fixture.root.join("home/.ssh/config"), "").unwrap();
        assert!(!fixture.service.has_agent_hosts().await);
        assert!(!fixture.service.is_agent_host("web").await.unwrap());
        assert!(fixture.service.agent_hosts().await.unwrap().is_empty());
        fixture
            .service
            .set_agent_access("web", false)
            .await
            .unwrap();

        // Defining the alias again does not silently restore access.
        std::fs::write(fixture.root.join("home/.ssh/config"), "Host web\n").unwrap();
        assert!(!fixture.service.is_agent_host("web").await.unwrap());
    }

    #[tokio::test]
    async fn connection_test_uses_batch_mode_and_classifies_failures() {
        let fixture = fixture("Host good bad1\n").await;
        let ok = fixture.service.test_connection("good", None).await.unwrap();
        assert!(ok.ok);
        let failed = fixture.service.test_connection("bad1", None).await.unwrap();
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

    pub(crate) fn askpass_files(fixture: &Fixture) -> Vec<PathBuf> {
        std::fs::read_dir(fixture.root.join("data/ssh").join(ASKPASS_DIR))
            .unwrap()
            .map(|entry| entry.unwrap().path())
            .collect()
    }

    #[tokio::test]
    async fn terminal_password_goes_to_askpass_only() {
        let fixture = fixture("Host good\n").await;
        let (plain, secret) = fixture
            .service
            .terminal_command("good", None)
            .await
            .unwrap();
        assert!(secret.is_none());
        assert!(!plain.env.iter().any(|(key, _)| key == askpass::FILE_ENV));
        assert!(!plain
            .args
            .iter()
            .any(|arg| arg == "NumberOfPasswordPrompts=1"));

        let (command, secret) = fixture
            .service
            .terminal_command("good", Some("hunter2"))
            .await
            .unwrap();
        let secret = secret.unwrap();
        let env = |name: &str| {
            command
                .env
                .iter()
                .find(|(key, _)| key == name)
                .map(|(_, value)| value.clone())
        };
        assert_eq!(env(askpass::FILE_ENV), Some(secret.path().into()));
        assert_eq!(env("SSH_ASKPASS_REQUIRE"), Some("force".into()));
        assert!(env("SSH_ASKPASS").is_some());
        assert!(!command
            .env
            .iter()
            .any(|(_, value)| value.to_string_lossy().contains("hunter2")));
        assert_eq!(std::fs::read(secret.path()).unwrap(), b"hunter2");
        let args: Vec<String> = command
            .args
            .iter()
            .map(|arg| arg.to_string_lossy().into_owned())
            .collect();
        let args = args.join(" ");
        assert!(
            args.contains("-o NumberOfPasswordPrompts=1 -o StrictHostKeyChecking=yes -tt -- good")
        );
        assert!(!args.contains("hunter2"));

        // The terminal deadline deletes a file ssh never asked for.
        let path = secret.path().to_owned();
        secret.expire_after(Duration::from_millis(20));
        tokio::time::sleep(Duration::from_millis(200)).await;
        assert!(!path.exists());
        assert!(askpass_files(&fixture).is_empty());
    }

    #[tokio::test]
    async fn password_test_keeps_the_secret_out_of_the_environment() {
        let fixture = fixture("Host good\n").await;
        let result = fixture
            .service
            .test_connection("good", Some("hunter2-secret"))
            .await
            .unwrap();
        assert!(result.ok);
        let log = std::fs::read_to_string(fixture.root.join("askpass.log")).unwrap();
        assert!(log.contains("askpass-file-present"), "{log}");
        assert!(log.contains(askpass::FILE_ENV), "{log}");
        assert!(!log.contains("hunter2-secret"), "{log}");
        assert!(askpass_files(&fixture).is_empty());
    }

    /// A shared master would let any password pass, so a password test
    /// gets a connection of its own; a key test may still reuse one.
    #[tokio::test]
    async fn password_test_never_shares_a_connection() {
        let fixture = fixture("Host good\n").await;
        fixture
            .service
            .test_connection("good", Some("hunter2"))
            .await
            .unwrap();
        let log = std::fs::read_to_string(fixture.root.join("ssh.log")).unwrap();
        let line = log.lines().find(|l| l.ends_with("good exit 0")).unwrap();
        let rest = line
            .split_once(&format!("{} ", fixture.service.wrapper_path().display()))
            .unwrap()
            .1;
        assert!(
            rest.starts_with("-o ControlMaster=no -o ControlPath=none -o BatchMode=no"),
            "{line}"
        );
        assert!(!line.contains("ControlPersist"), "{line}");
        assert!(!line.contains("ControlMaster=auto"), "{line}");
        assert_eq!(line.matches("ControlPath").count(), 1, "{line}");

        std::fs::remove_file(fixture.root.join("ssh.log")).unwrap();
        fixture.service.test_connection("good", None).await.unwrap();
        let log = std::fs::read_to_string(fixture.root.join("ssh.log")).unwrap();
        let line = log.lines().find(|l| l.ends_with("good exit 0")).unwrap();
        assert!(!line.contains("ControlPath=none"), "{line}");
        if fixture.service.inner.multiplex {
            assert!(line.contains("ControlPersist=300"), "{line}");
        }
    }

    /// A login for agents becomes the shared master; its password still
    /// reaches ssh only through the askpass file.
    #[tokio::test]
    async fn shared_login_becomes_the_master() {
        let fixture = fixture("Host good\n").await;
        if !fixture.service.inner.multiplex {
            return;
        }
        assert!(!fixture.service.master_alive("good").await);
        std::fs::write(fixture.root.join("master-good"), "").unwrap();
        assert!(fixture.service.master_alive("good").await);

        let result = fixture
            .service
            .connect_shared("good", "hunter2")
            .await
            .unwrap();
        assert!(result.ok);
        let log = std::fs::read_to_string(fixture.root.join("ssh.log")).unwrap();
        let line = log.lines().find(|l| l.ends_with("good exit 0")).unwrap();
        let rest = line
            .split_once(&format!("{} ", fixture.service.wrapper_path().display()))
            .unwrap()
            .1;
        assert!(
            rest.starts_with("-o ControlMaster=auto -o ControlPath="),
            "{line}"
        );
        assert!(
            line.contains("ControlPersist=300 -o BatchMode=no"),
            "{line}"
        );
        assert!(!line.contains("hunter2"), "{line}");
        let env = std::fs::read_to_string(fixture.root.join("askpass.log")).unwrap();
        assert!(env.contains("askpass-file-present"));
        assert!(!env.contains("hunter2"));
        assert!(askpass_files(&fixture).is_empty());

        assert!(
            fixture
                .service
                .connect_shared("nope", "hunter2")
                .await
                .is_err(),
            "unknown hosts are refused"
        );
    }

    #[tokio::test]
    async fn password_content_is_checked_before_use() {
        let fixture = fixture("Host good\n").await;
        let long = "x".repeat(MAX_PASSWORD_BYTES + 1);
        for bad in ["sec\0ret", "sec\rret", "sec\nret", long.as_str()] {
            let error = fixture
                .service
                .test_connection("good", Some(bad))
                .await
                .unwrap_err();
            assert!(matches!(error, AppError::InvalidRequest(_)), "{error}");
            assert!(!error.to_string().contains("sec"), "{error}");
            assert!(!error.to_string().contains("xxxx"), "{error}");
            // Not swallowed by the terminal's ProxyJump fallback.
            assert!(matches!(
                fixture.service.terminal_command("good", Some(bad)).await,
                Err(AppError::InvalidRequest(_))
            ));
        }
        assert!(askpass_files(&fixture).is_empty());
        assert!(!fixture.root.join("askpass.log").exists());

        let longest = "x".repeat(MAX_PASSWORD_BYTES);
        for good in [longest.as_str(), "pässwörd 密码 🔑", " spaced "] {
            let result = fixture
                .service
                .test_connection("good", Some(good))
                .await
                .unwrap();
            assert!(result.ok);
        }
    }

    /// The ProxyJump guard must see the config as it is now, not as it was
    /// when the host list was last resolved.
    #[tokio::test]
    async fn password_use_resolves_the_host_afresh() {
        let fixture = fixture("Host good\n").await;
        let resolutions = || {
            std::fs::read_to_string(fixture.root.join("ssh.log"))
                .unwrap_or_default()
                .lines()
                .filter(|line| line.contains(" -G "))
                .count()
        };
        fixture.service.list_hosts().await.unwrap();
        assert_eq!(resolutions(), 1);
        fixture.service.list_hosts().await.unwrap();
        assert_eq!(resolutions(), 1, "cached");
        fixture
            .service
            .test_connection("good", Some("hunter2"))
            .await
            .unwrap();
        assert_eq!(resolutions(), 2);
        let (_command, _secret) = fixture
            .service
            .terminal_command("good", Some("hunter2"))
            .await
            .unwrap();
        assert_eq!(resolutions(), 3);
    }

    #[test]
    fn terminal_command_debug_omits_environment_values() {
        let command = TerminalCommand {
            program: "ssh".into(),
            args: vec!["-tt".into()],
            env: vec![("SECRET_THING".into(), "hunter2".into())],
            cwd: PathBuf::from("/home/u"),
            ssh_host: "good".into(),
        };
        let debug = format!("{command:?}");
        assert!(debug.contains("SECRET_THING"), "{debug}");
        assert!(!debug.contains("hunter2"), "{debug}");
        let options = crate::local_terminal::TerminalStartOptions {
            request_id: "r".into(),
            terminal_id: None,
            tenant_id: "local".into(),
            workspace_id: None,
            cwd: String::new(),
            shell: None,
            rows: None,
            cols: None,
            command: Some(command),
            shell_integration: false,
        };
        assert!(!format!("{options:?}").contains("hunter2"));
        let target = crate::server::protocol::TerminalSshTarget {
            host: "good".into(),
            password: Some("hunter2".into()),
        };
        assert!(!format!("{target:?}").contains("hunter2"));
    }

    /// A failed write leaves memory and every file as before, so the API
    /// reports what ssh actually reads.
    #[tokio::test]
    async fn failed_writes_restore_files_and_memory() {
        let fixture = fixture("").await;
        let dir = fixture.root.join("data/ssh");
        let before = std::fs::read(dir.join(STORE_FILE)).unwrap();
        // A directory cannot be replaced by the atomic rename.
        std::fs::remove_file(dir.join(DEFAULTS_FILE)).unwrap();
        std::fs::create_dir(dir.join(DEFAULTS_FILE)).unwrap();
        let enabled = ConnectionSettings {
            server_alive_enabled: true,
            ..ConnectionSettings::default()
        };
        assert!(fixture
            .service
            .set_connection_settings(enabled.clone())
            .await
            .is_err());
        assert_eq!(
            fixture.service.connection_settings().await,
            ConnectionSettings::default()
        );
        assert_eq!(std::fs::read(dir.join(STORE_FILE)).unwrap(), before);
        assert!(fixture.service.create_host(managed("db")).await.is_err());
        assert!(fixture.service.host_views().await.unwrap().is_empty());
        assert!(!std::fs::read_to_string(dir.join(MANAGED_HOSTS_FILE))
            .unwrap()
            .contains("Host db"));
        assert_eq!(std::fs::read(dir.join(STORE_FILE)).unwrap(), before);

        std::fs::remove_dir(dir.join(DEFAULTS_FILE)).unwrap();
        fixture
            .service
            .set_connection_settings(enabled)
            .await
            .unwrap();
        assert!(std::fs::read_to_string(dir.join(DEFAULTS_FILE))
            .unwrap()
            .contains("ServerAliveInterval 15"));
    }

    #[tokio::test]
    async fn startup_purges_leftover_askpass_files() {
        let fixture = fixture("").await;
        let dir = fixture.root.join("data/ssh").join(ASKPASS_DIR);
        std::fs::write(dir.join("leftover"), "old").unwrap();
        SshService::with_home(
            &fixture.root.join("data"),
            "ssh".into(),
            Some(fixture.root.join("home")),
        )
        .await
        .unwrap();
        assert!(askpass_files(&fixture).is_empty());
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

    #[test]
    fn stores_without_connection_settings_load_with_defaults() {
        let store = SshStore::from_json(br#"{"managedHosts":[],"agentAccess":["web"]}"#).unwrap();
        assert_eq!(store.connection, ConnectionSettings::default());
        assert!(!store.connection.server_alive_enabled);
    }

    /// The store as another daemon version wrote it: partial, out of range
    /// or with fields this version does not know.
    #[test]
    fn stored_connection_settings_load_leniently_and_clamp() {
        let load = |json: &str| SshStore::from_json(json.as_bytes()).unwrap().connection;
        assert_eq!(
            load(r#"{"connection":{"serverAliveEnabled":true}}"#),
            ConnectionSettings {
                server_alive_enabled: true,
                ..ConnectionSettings::default()
            }
        );
        assert_eq!(
            load(
                r#"{"connection":{"serverAliveEnabled":true,"serverAliveInterval":1,
                "serverAliveCountMax":99,"futureField":"x"}}"#
            ),
            ConnectionSettings {
                server_alive_enabled: true,
                server_alive_interval: 5,
                server_alive_count_max: 10,
            }
        );
        assert_eq!(
            load(r#"{"connection":{"serverAliveInterval":9000,"serverAliveCountMax":0}}"#),
            ConnectionSettings {
                server_alive_enabled: false,
                server_alive_interval: 600,
                server_alive_count_max: 1,
            }
        );
        assert_eq!(load(r#"{"connection":{}}"#), ConnectionSettings::default());
    }

    /// `ManagedHost` exactly as v2.3.0 and v2.3.1 before password hosts
    /// declared it (`git show v2.3.0:src/ssh/config_file.rs`). Those
    /// versions refuse to start when a stored record does not fit it.
    #[derive(Debug, Deserialize)]
    #[serde(rename_all = "camelCase", deny_unknown_fields)]
    #[allow(dead_code)]
    struct ManagedHostV230 {
        alias: String,
        host_name: String,
        #[serde(default)]
        user: Option<String>,
        #[serde(default)]
        port: Option<u16>,
        #[serde(default)]
        identity_file: Option<String>,
        #[serde(default)]
        proxy_jump: Option<String>,
        #[serde(default)]
        options: Vec<SshOptionV230>,
    }

    #[derive(Debug, Deserialize)]
    #[serde(rename_all = "camelCase", deny_unknown_fields)]
    #[allow(dead_code)]
    struct SshOptionV230 {
        key: String,
        value: String,
    }

    /// `SshStore` of v2.3.0: unknown top-level fields are ignored.
    #[derive(Debug, Deserialize)]
    #[serde(rename_all = "camelCase")]
    #[allow(dead_code)]
    struct SshStoreV230 {
        #[serde(default)]
        managed_hosts: Vec<ManagedHostV230>,
        #[serde(default)]
        ftp_sites: Vec<FtpSite>,
        #[serde(default)]
        agent_access: BTreeSet<String>,
    }

    async fn service_with_store(store_json: &str) -> (PathBuf, SshService) {
        let root = std::env::temp_dir().join(format!("todex-ssh-{}", Uuid::new_v4().simple()));
        std::fs::create_dir_all(root.join("data/ssh")).unwrap();
        std::fs::write(root.join("data/ssh").join(STORE_FILE), store_json).unwrap();
        let service = SshService::with_home(&root.join("data"), "ssh".into(), None)
            .await
            .unwrap();
        (root, service)
    }

    fn stored_json(root: &Path) -> serde_json::Value {
        serde_json::from_slice(&std::fs::read(root.join("data/ssh").join(STORE_FILE)).unwrap())
            .unwrap()
    }

    async fn auth_methods(service: &SshService) -> Vec<(String, SshAuthMethod)> {
        service
            .inner
            .store
            .read()
            .await
            .managed_hosts
            .iter()
            .map(|host| (host.alias.clone(), host.auth_method))
            .collect()
    }

    #[tokio::test]
    async fn inline_auth_methods_migrate_to_the_side_table() {
        // As v2.3.1 wrote it.
        let (root, service) = service_with_store(
            r#"{"managedHosts":[
                {"alias":"db","hostName":"db.example","authMethod":"password"},
                {"alias":"web","hostName":"web.example","authMethod":"key"}
            ],"agentAccess":["db"],"connection":{"serverAliveEnabled":false,
            "serverAliveInterval":15,"serverAliveCountMax":3}}"#,
        )
        .await;
        assert_eq!(
            auth_methods(&service).await,
            [
                ("db".to_owned(), SshAuthMethod::Password),
                ("web".to_owned(), SshAuthMethod::Key)
            ]
        );
        let stored = stored_json(&root);
        assert_eq!(stored["authMethods"], serde_json::json!({"db": "password"}));
        for host in stored["managedHosts"].as_array().unwrap() {
            assert!(host.get("authMethod").is_none(), "{host}");
        }
        // API views still carry the method inline.
        let views = service.host_views().await.unwrap();
        assert_eq!(
            serde_json::to_value(&views[0]).unwrap()["managed"]["authMethod"],
            "password"
        );
        let _ = std::fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn new_stores_stay_readable_by_v2_3_0() {
        let (root, service) = service_with_store("{}").await;
        service
            .create_host(ManagedHost {
                auth_method: SshAuthMethod::Password,
                options: vec![config_file::SshOption {
                    key: "Compression".into(),
                    value: "yes".into(),
                }],
                ..managed("db")
            })
            .await
            .unwrap();
        service.create_host(managed("web")).await.unwrap();
        service
            .set_connection_settings(ConnectionSettings {
                server_alive_enabled: true,
                ..ConnectionSettings::default()
            })
            .await
            .unwrap();
        let bytes = std::fs::read(root.join("data/ssh").join(STORE_FILE)).unwrap();
        let old: SshStoreV230 = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(old.managed_hosts.len(), 2);
        assert_eq!(
            stored_json(&root)["authMethods"],
            serde_json::json!({"db": "password"})
        );

        // Renames and deletes carry over to the side table.
        service
            .update_host(
                "db",
                ManagedHost {
                    auth_method: SshAuthMethod::Password,
                    ..managed("db2")
                },
            )
            .await
            .unwrap();
        assert_eq!(
            stored_json(&root)["authMethods"],
            serde_json::json!({"db2": "password"})
        );
        service.delete_host("db2").await.unwrap();
        assert!(stored_json(&root).get("authMethods").is_none());
        let _ = std::fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn side_table_entries_load_and_unknown_methods_fall_back_to_keys() {
        let (root, service) = service_with_store(
            r#"{"managedHosts":[
                {"alias":"db","hostName":"db.example"},
                {"alias":"web","hostName":"web.example","futureField":1}
            ],"authMethods":{"db":"password","web":"passkey","gone":"password"}}"#,
        )
        .await;
        assert_eq!(
            auth_methods(&service).await,
            [
                ("db".to_owned(), SshAuthMethod::Password),
                ("web".to_owned(), SshAuthMethod::Key)
            ]
        );
        // Entries for unknown aliases and methods are not written back.
        assert_eq!(
            stored_json(&root)["authMethods"],
            serde_json::json!({"db": "password"})
        );
        let _ = std::fs::remove_dir_all(root);
    }

    /// OpenSSH itself must read the defaults after the user's config, and a
    /// `Host *` there must not leak into the includes that follow.
    #[tokio::test]
    async fn connection_defaults_reach_openssh_below_user_config() {
        let fixture = fixture("Host web\n  ServerAliveInterval 7\n").await;
        let saved = fixture
            .service
            .set_connection_settings(ConnectionSettings {
                server_alive_enabled: true,
                server_alive_interval: 20,
                server_alive_count_max: 4,
            })
            .await
            .unwrap();
        assert_eq!(saved, fixture.service.connection_settings().await);
        let wrapper = fixture.root.join("data/ssh").join(WRAPPER_FILE);
        let effective = |alias: &str| match std::process::Command::new("ssh")
            .arg("-F")
            .arg(&wrapper)
            .args(["-G", alias])
            .env("HOME", fixture.root.join("home"))
            .output()
        {
            Ok(output) => {
                assert!(output.status.success(), "{output:?}");
                Some(String::from_utf8(output.stdout).unwrap())
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
            Err(error) => panic!("ssh -G: {error}"),
        };
        let Some(web) = effective("web") else {
            eprintln!("skipping: ssh not installed");
            return;
        };
        assert!(web.contains("\nserveraliveinterval 7\n"), "{web}");
        assert!(web.contains("\nserveralivecountmax 4\n"), "{web}");
        let other = effective("other").unwrap();
        assert!(other.contains("\nserveraliveinterval 20\n"), "{other}");

        fixture
            .service
            .set_connection_settings(ConnectionSettings::default())
            .await
            .unwrap();
        let other = effective("other").unwrap();
        assert!(!other.contains("\nserveraliveinterval 20\n"), "{other}");
        assert!(matches!(
            fixture
                .service
                .set_connection_settings(ConnectionSettings {
                    server_alive_interval: 0,
                    ..ConnectionSettings::default()
                })
                .await,
            Err(AppError::InvalidRequest(_))
        ));
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
