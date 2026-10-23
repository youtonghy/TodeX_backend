use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use chrono::{DateTime, Utc};
use futures_util::StreamExt;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio::sync::{Mutex, OwnedRwLockReadGuard, OwnedRwLockWriteGuard, RwLock};
use uuid::Uuid;

use crate::config::Config;
use crate::conversation::ProviderKind;
use crate::error::AppError;

use super::process::{executable_available, resolve_executable, run_bounded_command, CommandSpec};

const VERSION_TIMEOUT: Duration = Duration::from_secs(12);
const UPGRADE_TIMEOUT: Duration = Duration::from_secs(10 * 60);
const INSTALL_TIMEOUT: Duration = Duration::from_secs(10 * 60);
const MAX_OUTPUT_BYTES: usize = 64 * 1024;
/// A CLI's whole row, including the version its binary reports.
const ROW_CACHE_TTL: Duration = Duration::from_secs(30);
/// Its latest release, read from the network; a refresh skips it.
const LATEST_CACHE_TTL: Duration = Duration::from_secs(15 * 60);
const LATEST_FAILURE_TTL: Duration = Duration::from_secs(2 * 60);
const MAX_VERSION_RESPONSE_BYTES: usize = 64 * 1024;

#[derive(Clone, Copy, Debug, Deserialize, Serialize, Eq, PartialEq, Hash)]
#[serde(rename_all = "kebab-case")]
pub enum ManagedCli {
    Codex,
    Pi,
    ClaudeCode,
    GrokBuild,
    Devin,
    Opencode,
    Antigravity,
    DeepseekHarness,
}

impl ManagedCli {
    /// In declaration order: `cli as usize` indexes it.
    pub const ALL: [ManagedCli; 8] = [
        Self::Codex,
        Self::Pi,
        Self::ClaudeCode,
        Self::GrokBuild,
        Self::Devin,
        Self::Opencode,
        Self::Antigravity,
        Self::DeepseekHarness,
    ];

    pub fn id(self) -> &'static str {
        match self {
            Self::Codex => "codex",
            Self::Pi => "pi",
            Self::ClaudeCode => "claude-code",
            Self::GrokBuild => "grok-build",
            Self::Devin => "devin",
            Self::Opencode => "opencode",
            Self::Antigravity => "antigravity",
            Self::DeepseekHarness => "deepseek-harness",
        }
    }

    pub(crate) fn name(self) -> &'static str {
        match self {
            Self::Codex => "Codex",
            Self::Pi => "Pi",
            Self::ClaudeCode => "Claude Code",
            Self::GrokBuild => "Grok Build",
            Self::Devin => "Devin",
            Self::Opencode => "OpenCode",
            Self::Antigravity => "Antigravity CLI",
            Self::DeepseekHarness => "DeepSeek Harness",
        }
    }

    pub(crate) fn provider_kind(self) -> ProviderKind {
        match self {
            Self::Codex => ProviderKind::Codex,
            Self::Pi => ProviderKind::Pi,
            Self::ClaudeCode => ProviderKind::ClaudeCode,
            Self::GrokBuild => ProviderKind::GrokBuild,
            Self::Devin => ProviderKind::Devin,
            Self::Opencode => ProviderKind::Opencode,
            Self::Antigravity => ProviderKind::Antigravity,
            Self::DeepseekHarness => ProviderKind::DeepseekHarness,
        }
    }

    /// Whether the binary this CLI would launch ships with the container
    /// image (`agent.image_cli_dirs`); the image, not TodeX, updates it.
    pub(crate) fn managed_by_image(self, config: &Config) -> bool {
        binary_in_dirs(self.binary(config), &config.deployment.image_cli_dirs)
    }

    pub(crate) fn binary(self, config: &Config) -> &str {
        match self {
            Self::Codex => &config.agent.codex_bin,
            Self::Pi => &config.agent.pi_bin,
            Self::ClaudeCode => &config.agent.claude_bin,
            Self::GrokBuild => &config.agent.grok_bin,
            Self::Devin => &config.agent.devin_bin,
            Self::Opencode => &config.agent.opencode_bin,
            Self::Antigravity => &config.agent.antigravity_bin,
            Self::DeepseekHarness => &config.agent.dsh_bin,
        }
    }

    fn upgrade_args(self) -> &'static [&'static str] {
        match self {
            Self::Codex => &["update"],
            Self::Pi => &["update", "--self", "--no-approve"],
            Self::ClaudeCode => &["update"],
            Self::GrokBuild => &["update"],
            Self::Devin => &["update"],
            Self::Opencode => &["upgrade"],
            Self::Antigravity => &["update"],
            // No self-update command; `npm_package` reinstalls instead.
            Self::DeepseekHarness => &[],
        }
    }

    /// The npm package for CLIs distributed only through npm. Both install
    /// and upgrade run `npm install -g <package>@latest`.
    fn npm_package(self) -> Option<&'static str> {
        match self {
            Self::DeepseekHarness => Some("@deepseek-ai/dsh"),
            _ => None,
        }
    }

    /// Whether TodeX can install this CLI here: a vendor script on Unix, or
    /// npm on PATH for npm-distributed CLIs.
    fn install_available(self) -> bool {
        if self.npm_package().is_some() {
            return cfg!(unix) && executable_available("npm");
        }
        self.install_script().is_some()
    }

    /// The vendor's documented `curl | bash` installer. Each script installs
    /// a native binary into a per-user directory (see
    /// `process::user_install_dirs`); Pi's also needs Node.js >= 22 on PATH.
    /// Windows installers are PowerShell-only and are not offered yet.
    fn install_script(self) -> Option<InstallScript> {
        if !cfg!(unix) {
            return None;
        }
        Some(match self {
            Self::Codex => InstallScript {
                url: "https://chatgpt.com/codex/install.sh",
                shell: "sh",
                // Skips the "start Codex now?" and npm-cleanup prompts.
                env: &[("CODEX_NON_INTERACTIVE", "1")],
            },
            Self::Pi => InstallScript {
                url: "https://pi.dev/install.sh",
                shell: "sh",
                env: &[],
            },
            Self::ClaudeCode => InstallScript {
                url: "https://claude.ai/install.sh",
                shell: "bash",
                env: &[],
            },
            Self::GrokBuild => InstallScript {
                url: "https://x.ai/cli/install.sh",
                shell: "bash",
                env: &[],
            },
            Self::Devin => InstallScript {
                url: "https://cli.devin.ai/install.sh",
                shell: "bash",
                env: &[],
            },
            Self::Opencode => InstallScript {
                url: "https://opencode.ai/install",
                shell: "bash",
                env: &[],
            },
            Self::Antigravity => InstallScript {
                url: "https://antigravity.google/cli/install.sh",
                shell: "bash",
                env: &[],
            },
            Self::DeepseekHarness => return None,
        })
    }
}

/// Whether `program` resolves (as it would be launched) to a file under one
/// of `dirs`. Both the resolved path and its canonical target count, and the
/// directories are compared as given and canonicalized: an image may expose
/// its CLIs as symlinks (`node_modules/.bin/codex`) into the package tree.
fn binary_in_dirs(program: &str, dirs: &[std::path::PathBuf]) -> bool {
    if dirs.is_empty() {
        return false;
    }
    let Some(resolved) = resolve_executable(program) else {
        return false;
    };
    let canonical = std::fs::canonicalize(&resolved).ok();
    dirs.iter().any(|dir| {
        let canonical_dir = std::fs::canonicalize(dir).ok();
        [Some(&resolved), canonical.as_ref()]
            .into_iter()
            .flatten()
            .any(|path| {
                path.starts_with(dir)
                    || canonical_dir
                        .as_ref()
                        .is_some_and(|canonical_dir| path.starts_with(canonical_dir))
            })
    })
}

/// The `managedBy` value of CLIs shipped with the container image.
pub const MANAGED_BY_IMAGE: &str = "image";

/// A fixed installer invocation: `curl -fsSL <url> | <shell> -s`, fetched
/// over HTTPS only (redirects included). Nothing in it comes from the
/// request, so the shell never sees user input.
struct InstallScript {
    url: &'static str,
    shell: &'static str,
    env: &'static [(&'static str, &'static str)],
}

#[derive(Clone, Copy, Debug, Serialize, Eq, PartialEq)]
#[serde(rename_all = "camelCase")]
pub enum CliOperationAction {
    Install,
    Upgrade,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CliVersionInfo {
    pub id: String,
    pub name: String,
    pub kind: String,
    pub installed: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub current_version: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub latest_version: Option<String>,
    pub status: String,
    pub upgrade_supported: bool,
    pub install_supported: bool,
    /// `"image"` when the binary ships with the container image; such a CLI
    /// is neither installed nor upgraded by TodeX.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub managed_by: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CliVersionsResponse {
    pub clis: Vec<CliVersionInfo>,
    /// When the oldest latest-release check among `clis` ran.
    pub checked_at: String,
    /// The earliest-started running operation, for clients that track one.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub active_operation: Option<CliUpgradeOperation>,
    /// Every running operation, at most one per CLI.
    pub active_operations: Vec<CliUpgradeOperation>,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CliUpgradeOperation {
    pub id: String,
    pub provider: ManagedCli,
    pub action: CliOperationAction,
    pub status: String,
    pub started_at: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub finished_at: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub previous_version: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub current_version: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

/// One lock per managed CLI. An install or upgrade holds its CLI's lock
/// exclusively for its whole run; anything that starts that CLI's binary
/// (a turn, discovery, a version probe) holds it shared while it starts, so
/// CLIs upgrade independently of each other and of every other Agent.
#[derive(Clone, Default)]
pub struct CliLocks([Arc<RwLock<()>>; ManagedCli::ALL.len()]);

/// Shared permits on one or more CLIs; dropping it releases them.
#[must_use]
pub struct CliPermits(#[allow(dead_code)] Vec<OwnedRwLockReadGuard<()>>);

impl CliLocks {
    fn lock(&self, cli: ManagedCli) -> Arc<RwLock<()>> {
        self.0[cli as usize].clone()
    }

    /// In the fixed `ManagedCli::ALL` order, so two callers taking several
    /// locks never wait on each other in a cycle.
    fn ordered(clis: &[ManagedCli]) -> impl Iterator<Item = ManagedCli> + '_ {
        ManagedCli::ALL
            .into_iter()
            .filter(move |cli| clis.contains(cli))
    }

    /// Waits while any of `clis` is being installed or upgraded.
    pub async fn read(&self, clis: &[ManagedCli]) -> CliPermits {
        let mut guards = Vec::new();
        for cli in Self::ordered(clis) {
            guards.push(self.lock(cli).read_owned().await);
        }
        CliPermits(guards)
    }

    /// `None` while any of `clis` is being installed or upgraded.
    pub fn try_read(&self, clis: &[ManagedCli]) -> Option<CliPermits> {
        let mut guards = Vec::new();
        for cli in Self::ordered(clis) {
            guards.push(self.lock(cli).try_read_owned().ok()?);
        }
        Some(CliPermits(guards))
    }

    pub fn try_write(&self, cli: ManagedCli) -> Option<OwnedRwLockWriteGuard<()>> {
        self.lock(cli).try_write_owned().ok()
    }

    /// Waits up to `wait` for starts and probes of `cli` already holding it
    /// shared to finish, so an upgrade does not fail just because the
    /// version list was being read.
    pub async fn write_within(
        &self,
        cli: ManagedCli,
        wait: Duration,
    ) -> Option<OwnedRwLockWriteGuard<()>> {
        tokio::time::timeout(wait, self.lock(cli).write_owned())
            .await
            .ok()
    }

    /// No CLI is held, shared or exclusively.
    pub fn is_idle(&self) -> bool {
        self.0.iter().all(|lock| lock.try_write().is_ok())
    }
}

#[derive(Default)]
struct UpgradeState {
    /// The running operation of each CLI; at most one per CLI.
    running: HashMap<ManagedCli, String>,
    operations: HashMap<String, CliUpgradeOperation>,
}

struct LatestRelease {
    at: Instant,
    checked_at: DateTime<Utc>,
    /// A user-facing error when the release source could not be read.
    version: Result<Option<String>, String>,
}

impl LatestRelease {
    fn fresh(&self) -> bool {
        let ttl = if self.version.is_ok() {
            LATEST_CACHE_TTL
        } else {
            LATEST_FAILURE_TTL
        };
        self.at.elapsed() < ttl
    }
}

/// Probe results by CLI: a whole row for a short while, and the latest
/// release (a network request) for longer.
#[derive(Default)]
struct VersionCache {
    rows: HashMap<ManagedCli, (Instant, CliVersionInfo)>,
    latest: HashMap<ManagedCli, LatestRelease>,
}

#[derive(Clone, Default)]
pub struct CliManager {
    upgrades: Arc<Mutex<UpgradeState>>,
    locks: CliLocks,
    /// One probe of each CLI at a time; concurrent version requests share it.
    probes: Arc<[Mutex<()>; ManagedCli::ALL.len()]>,
    cache: Arc<std::sync::Mutex<VersionCache>>,
}

impl CliManager {
    pub fn locks(&self) -> CliLocks {
        self.locks.clone()
    }

    fn cache(&self) -> std::sync::MutexGuard<'_, VersionCache> {
        self.cache
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// `refresh` re-reads every CLI's latest release instead of the cache.
    pub async fn versions(&self, config: &Config, refresh: bool) -> CliVersionsResponse {
        let rows = futures_util::future::join_all(
            ManagedCli::ALL
                .into_iter()
                .map(|cli| self.version_row(config, cli, refresh)),
        )
        .await;
        let checked_at = {
            let cache = self.cache();
            ManagedCli::ALL
                .iter()
                .filter_map(|cli| cache.latest.get(cli).map(|latest| latest.checked_at))
                .min()
                .unwrap_or_else(Utc::now)
        };
        let mut clis = rows;
        clis.extend(config.agent.acp_profiles.keys().map(|name| CliVersionInfo {
            id: format!("acp:{name}"),
            name: name.clone(),
            kind: "external".to_owned(),
            installed: true,
            current_version: None,
            latest_version: None,
            status: "external".to_owned(),
            upgrade_supported: false,
            install_supported: false,
            managed_by: None,
            error: None,
        }));
        let (active_operation, active_operations) = self.running_operations().await;
        CliVersionsResponse {
            clis,
            checked_at: checked_at.to_rfc3339(),
            active_operation,
            active_operations,
        }
    }

    async fn version_row(&self, config: &Config, cli: ManagedCli, refresh: bool) -> CliVersionInfo {
        self.cached_row(cli, refresh, || {
            inspect_cli(config, cli, self.latest_version(config, cli, refresh))
        })
        .await
    }

    /// `probe` runs only when no usable row is cached and `cli` is not
    /// being installed or upgraded.
    async fn cached_row<F>(
        &self,
        cli: ManagedCli,
        refresh: bool,
        probe: impl FnOnce() -> F,
    ) -> CliVersionInfo
    where
        F: std::future::Future<Output = CliVersionInfo>,
    {
        let requested = Instant::now();
        let _probe = self.probes[cli as usize].lock().await;
        if let Some((at, row)) = self.cache().rows.get(&cli) {
            // A probe that finished while this request waited is as new as
            // a refresh could make it.
            if *at >= requested || (!refresh && at.elapsed() < ROW_CACHE_TTL) {
                return row.clone();
            }
        }
        // Mid-install or upgrade: report the last result without running it.
        let Some(_permit) = self.locks.try_read(&[cli]) else {
            if let Some((_, row)) = self.cache().rows.get(&cli) {
                return row.clone();
            }
            return self.busy_row(cli).await;
        };
        let row = probe().await;
        self.cache().rows.insert(cli, (Instant::now(), row.clone()));
        row
    }

    async fn latest_version(
        &self,
        config: &Config,
        cli: ManagedCli,
        refresh: bool,
    ) -> Result<Option<String>, String> {
        if !refresh {
            if let Some(latest) = self
                .cache()
                .latest
                .get(&cli)
                .filter(|latest| latest.fresh())
            {
                return latest.version.clone();
            }
        }
        let version = latest_version(config, cli)
            .await
            .map_err(|error| user_facing_error(&error));
        self.cache().latest.insert(
            cli,
            LatestRelease {
                at: Instant::now(),
                checked_at: Utc::now(),
                version: version.clone(),
            },
        );
        version
    }

    /// A row for a CLI first seen mid-operation, from what the operation knows.
    async fn busy_row(&self, cli: ManagedCli) -> CliVersionInfo {
        let operation = {
            let state = self.upgrades.lock().await;
            state
                .running
                .get(&cli)
                .and_then(|id| state.operations.get(id))
                .cloned()
        };
        let upgrading = operation
            .as_ref()
            .is_some_and(|operation| operation.action == CliOperationAction::Upgrade);
        let latest_version = self
            .cache()
            .latest
            .get(&cli)
            .and_then(|latest| latest.version.clone().ok().flatten());
        CliVersionInfo {
            id: cli.id().to_owned(),
            name: cli.name().to_owned(),
            kind: "managed".to_owned(),
            installed: upgrading,
            current_version: operation.and_then(|operation| operation.previous_version),
            latest_version,
            status: if upgrading { "unknown" } else { "notInstalled" }.to_owned(),
            upgrade_supported: false,
            install_supported: false,
            managed_by: None,
            error: None,
        }
    }

    /// The earliest-started running operation, and all of them in start order.
    async fn running_operations(&self) -> (Option<CliUpgradeOperation>, Vec<CliUpgradeOperation>) {
        let state = self.upgrades.lock().await;
        let mut running = state
            .running
            .values()
            .filter_map(|id| state.operations.get(id))
            .cloned()
            .collect::<Vec<_>>();
        running.sort_by(|left, right| left.started_at.cmp(&right.started_at));
        (running.first().cloned(), running)
    }

    /// Each CLI runs one install or upgrade at a time; different CLIs run
    /// theirs concurrently.
    pub async fn begin_operation(
        &self,
        provider: ManagedCli,
        action: CliOperationAction,
        previous_version: Option<String>,
    ) -> Result<CliUpgradeOperation, AppError> {
        let mut state = self.upgrades.lock().await;
        if let Some(id) = state.running.get(&provider) {
            let active = state
                .operations
                .get(id)
                .expect("running CLI operation must exist");
            let verb = match active.action {
                CliOperationAction::Install => "install",
                CliOperationAction::Upgrade => "upgrade",
            };
            return Err(AppError::Conflict(format!(
                "CLI {verb} of {} is already running",
                provider.id()
            )));
        }
        let operation = CliUpgradeOperation {
            id: format!("cliup_{}", Uuid::new_v4().simple()),
            provider,
            action,
            status: "running".to_owned(),
            started_at: Utc::now().to_rfc3339(),
            finished_at: None,
            previous_version,
            current_version: None,
            error: None,
        };
        state.running.insert(provider, operation.id.clone());
        state
            .operations
            .insert(operation.id.clone(), operation.clone());
        if state.operations.len() > 50 {
            let mut completed = state
                .operations
                .values()
                .filter(|item| item.status != "running")
                .map(|item| (item.started_at.clone(), item.id.clone()))
                .collect::<Vec<_>>();
            completed.sort_unstable();
            let excess = state.operations.len() - 50;
            for (_, id) in completed.into_iter().take(excess) {
                state.operations.remove(&id);
            }
        }
        Ok(operation)
    }

    pub async fn complete_operation(
        &self,
        operation_id: &str,
        result: Result<Option<String>, AppError>,
    ) -> Option<CliUpgradeOperation> {
        let mut state = self.upgrades.lock().await;
        let operation = state.operations.get_mut(operation_id)?;
        operation.finished_at = Some(Utc::now().to_rfc3339());
        match result {
            Ok(version) => {
                operation.status = "succeeded".to_owned();
                operation.current_version = version;
            }
            Err(error) => {
                operation.status = "failed".to_owned();
                operation.error = Some(user_facing_error(&error));
            }
        }
        let provider = operation.provider;
        if state.running.get(&provider).map(String::as_str) == Some(operation_id) {
            state.running.remove(&provider);
        }
        let completed = state.operations.get(operation_id).cloned();
        drop(state);
        // Only this CLI changed; the next list probes it again, and its
        // release too, since a self-update may have gone past the cached one.
        let mut cache = self.cache();
        cache.rows.remove(&provider);
        cache.latest.remove(&provider);
        completed
    }

    pub async fn operation(&self, operation_id: &str) -> Option<CliUpgradeOperation> {
        self.upgrades
            .lock()
            .await
            .operations
            .get(operation_id)
            .cloned()
    }
}

pub async fn run_upgrade(
    config: &Config,
    provider: ManagedCli,
) -> Result<Option<String>, AppError> {
    if provider.npm_package().is_some() {
        run_npm_install(config, provider).await?;
        return current_version(config, provider).await;
    }
    let binary = provider.binary(config);
    let cwd = &config.data_dir;
    let help = run_command(binary, &["--help"], cwd, VERSION_TIMEOUT).await?;
    if provider == ManagedCli::Codex && !help.contains("update") {
        return Err(AppError::Unsupported(
            "this Codex installation does not expose a safe self-update command".to_owned(),
        ));
    }
    run_command(binary, provider.upgrade_args(), cwd, UPGRADE_TIMEOUT).await?;
    current_version(config, provider).await
}

/// Run the vendor installer, then confirm through the configured binary.
///
/// Success is judged by the result, not the script's exit status: Devin's
/// installer ends with the interactive `devin setup`, which fails without a
/// terminal after the binary is already in place.
pub async fn run_install(
    config: &Config,
    provider: ManagedCli,
) -> Result<Option<String>, AppError> {
    if provider.npm_package().is_some() {
        run_npm_install(config, provider).await?;
        return current_version(config, provider).await.and_then(|version| {
            version
                .map(Some)
                .ok_or_else(|| not_found_after_install(config, provider))
        });
    }
    let script = provider.install_script().ok_or_else(|| {
        AppError::Unsupported(format!(
            "{} cannot be installed from TodeX on this platform",
            provider.name()
        ))
    })?;
    // `-L` follows redirects; keep every hop on HTTPS so a redirect cannot
    // downgrade the script the shell runs to plain HTTP.
    let command = format!(
        "curl --proto '=https' --proto-redir '=https' -fsSL '{}' | {} -s",
        script.url, script.shell
    );
    let mut spec = CommandSpec::new("bash", &config.data_dir);
    // pipefail: a failed download must not look like an empty, successful script.
    spec.args = vec![
        "-o".to_owned(),
        "pipefail".to_owned(),
        "-c".to_owned(),
        command,
    ];
    spec.env = script
        .env
        .iter()
        .map(|(key, value)| ((*key).to_owned(), (*value).to_owned()))
        .collect();
    spec.detach_terminal = true;
    let script_error = match run_bounded_command(&spec, MAX_OUTPUT_BYTES, INSTALL_TIMEOUT).await {
        Ok(output) if output.success => None,
        Ok(output) => Some(install_failure(provider, &output.stderr, &output.stdout)),
        Err(error) => Some(error),
    };
    match (current_version(config, provider).await, script_error) {
        (Ok(version), None) => Ok(version),
        (Ok(version), Some(error)) => {
            tracing::warn!(provider = provider.id(), error = %error, "CLI installer exited with an error after installing");
            Ok(version)
        }
        (Err(_), Some(error)) => Err(error),
        (Err(_), None) => Err(not_found_after_install(config, provider)),
    }
}

fn not_found_after_install(config: &Config, provider: ManagedCli) -> AppError {
    AppError::ProviderUnavailable(format!(
        "{} was installed but '{}' is still not found; set its path in the TodeX config",
        provider.name(),
        provider.binary(config)
    ))
}

/// `npm install -g <package>@latest`. The package name is a constant, so no
/// request input reaches npm.
async fn run_npm_install(config: &Config, provider: ManagedCli) -> Result<(), AppError> {
    let package = provider.npm_package().ok_or_else(|| {
        AppError::Unsupported(format!(
            "{} is not distributed through npm",
            provider.name()
        ))
    })?;
    if !provider.install_available() {
        return Err(AppError::Unsupported(format!(
            "{} needs npm on PATH to install or upgrade from TodeX",
            provider.name()
        )));
    }
    let mut spec = CommandSpec::new("npm", &config.data_dir);
    spec.args = npm_install_args(package);
    spec.detach_terminal = true;
    let output = run_bounded_command(&spec, MAX_OUTPUT_BYTES, INSTALL_TIMEOUT).await?;
    if output.success {
        Ok(())
    } else {
        Err(install_failure(provider, &output.stderr, &output.stdout))
    }
}

fn npm_install_args(package: &str) -> Vec<String> {
    vec![
        "install".to_owned(),
        "--global".to_owned(),
        "--no-fund".to_owned(),
        "--no-audit".to_owned(),
        format!("{package}@latest"),
    ]
}

/// The installer's last non-empty output line, which is where these scripts
/// print their error (for example Pi's missing Node.js).
fn install_failure(provider: ManagedCli, stderr: &[u8], stdout: &[u8]) -> AppError {
    let last_line = |bytes: &[u8]| {
        String::from_utf8_lossy(bytes)
            .lines()
            .rev()
            .map(|line| strip_ansi(line).trim().to_owned())
            .find(|line| !line.is_empty())
    };
    let detail = last_line(stderr).or_else(|| last_line(stdout));
    let mut message = format!("{} installer failed", provider.name());
    if let Some(detail) = detail {
        message.push_str(": ");
        message.extend(detail.chars().take(300));
    }
    AppError::ProviderUnavailable(message)
}

fn strip_ansi(line: &str) -> String {
    let mut out = String::with_capacity(line.len());
    let mut chars = line.chars().peekable();
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

pub async fn read_current_version(
    config: &Config,
    provider: ManagedCli,
) -> Result<Option<String>, AppError> {
    current_version(config, provider).await
}

async fn inspect_cli(
    config: &Config,
    provider: ManagedCli,
    latest: impl std::future::Future<Output = Result<Option<String>, String>>,
) -> CliVersionInfo {
    // A missing binary is a normal state with an Install action, not an error.
    if !executable_available(provider.binary(config)) {
        let latest_version = latest.await.ok().flatten();
        return CliVersionInfo {
            id: provider.id().to_owned(),
            name: provider.name().to_owned(),
            kind: "managed".to_owned(),
            installed: false,
            current_version: None,
            latest_version,
            status: "notInstalled".to_owned(),
            upgrade_supported: false,
            install_supported: config.security.enable_auth && provider.install_available(),
            managed_by: None,
            error: None,
        };
    }
    let (current, latest) = tokio::join!(current_version(config, provider), latest);
    let installed = current.is_ok();
    let current_version = current.as_ref().ok().cloned().flatten();
    let latest_version = latest.as_ref().ok().cloned().flatten();
    let status = match (&current_version, &latest_version) {
        (None, _) => "notInstalled",
        (Some(_), None) => "unknown",
        (Some(current), Some(latest)) => match compare_versions(current, latest) {
            std::cmp::Ordering::Less => "updateAvailable",
            std::cmp::Ordering::Equal => "upToDate",
            std::cmp::Ordering::Greater => "ahead",
        },
    };
    let error = current
        .err()
        .map(|error| user_facing_error(&error))
        .or_else(|| latest.err());
    let managed_by_image = provider.managed_by_image(config);
    CliVersionInfo {
        id: provider.id().to_owned(),
        name: provider.name().to_owned(),
        kind: "managed".to_owned(),
        installed,
        current_version,
        latest_version,
        status: status.to_owned(),
        upgrade_supported: installed
            && !managed_by_image
            && config.security.enable_auth
            && (provider.npm_package().is_none() || provider.install_available()),
        install_supported: false,
        managed_by: managed_by_image.then(|| MANAGED_BY_IMAGE.to_owned()),
        error,
    }
}

async fn current_version(
    config: &Config,
    provider: ManagedCli,
) -> Result<Option<String>, AppError> {
    let output = run_command(
        provider.binary(config),
        &["--version"],
        &config.data_dir,
        VERSION_TIMEOUT,
    )
    .await?;
    extract_version(&output).map(Some).ok_or_else(|| {
        AppError::ProviderUnavailable(format!(
            "{} version output was not recognized",
            provider.name()
        ))
    })
}

async fn latest_version(config: &Config, provider: ManagedCli) -> Result<Option<String>, AppError> {
    if provider == ManagedCli::GrokBuild {
        let output = run_command(
            provider.binary(config),
            &["update", "--check", "--json"],
            &config.data_dir,
            VERSION_TIMEOUT,
        )
        .await?;
        let value: Value = serde_json::from_str(&output).map_err(|_| {
            AppError::ProviderUnavailable("Grok returned invalid update metadata".to_owned())
        })?;
        return find_version_value(&value).map(Some).ok_or_else(|| {
            AppError::ProviderUnavailable("Grok latest version is unavailable".to_owned())
        });
    }

    let client = reqwest::Client::builder()
        .timeout(VERSION_TIMEOUT)
        .user_agent(format!("todex-agentd/{}", crate::version::APP_VERSION))
        .build()
        .map_err(|error| AppError::Anyhow(error.into()))?;
    let antigravity_manifest;
    let (url, field) = match provider {
        // GitHub's unauthenticated API is rate-limited per IP; the npm registry
        // mirrors the published CLI version without that cap.
        ManagedCli::Codex => (
            "https://registry.npmjs.org/@openai%2fcodex/latest",
            "version",
        ),
        ManagedCli::Pi => ("https://pi.dev/api/latest-version", "version"),
        ManagedCli::ClaudeCode => (
            "https://registry.npmjs.org/@anthropic-ai%2fclaude-code/latest",
            "version",
        ),
        // The self-update manifest is the same source `devin update` reads.
        ManagedCli::Devin => (
            "https://static.devin.ai/cli/current/manifest.json",
            "version",
        ),
        ManagedCli::Opencode => ("https://registry.npmjs.org/opencode-ai/latest", "version"),
        ManagedCli::DeepseekHarness => (
            "https://registry.npmjs.org/@deepseek-ai%2fdsh/latest",
            "version",
        ),
        // The per-platform manifest the official installer and `agy update` read.
        ManagedCli::Antigravity => {
            antigravity_manifest = antigravity_manifest_url();
            (antigravity_manifest.as_str(), "version")
        }
        ManagedCli::GrokBuild => unreachable!(),
    };
    let response = client
        .get(url)
        .send()
        .await
        .map_err(|_| {
            AppError::ProviderUnavailable(format!(
                "{} latest version is unavailable",
                provider.name()
            ))
        })?
        .error_for_status()
        .map_err(|_| {
            AppError::ProviderUnavailable(format!(
                "{} latest version is unavailable",
                provider.name()
            ))
        })?;
    let body = bounded_response_text(response).await.map_err(|_| {
        AppError::ProviderUnavailable(format!("{} latest version is unavailable", provider.name()))
    })?;
    if let Ok(value) = serde_json::from_str::<Value>(&body) {
        return value
            .get(field)
            .and_then(Value::as_str)
            .and_then(extract_version)
            .or_else(|| find_version_value(&value))
            .map(Some)
            .ok_or_else(|| {
                AppError::ProviderUnavailable(format!(
                    "{} latest version is unavailable",
                    provider.name()
                ))
            });
    }
    extract_version(&body).map(Some).ok_or_else(|| {
        AppError::ProviderUnavailable(format!("{} latest version is unavailable", provider.name()))
    })
}

fn antigravity_manifest_url() -> String {
    let os = match std::env::consts::OS {
        "macos" => "darwin",
        other => other,
    };
    let arch = match std::env::consts::ARCH {
        "aarch64" => "arm64",
        "x86_64" => "amd64",
        other => other,
    };
    format!(
        "https://antigravity-cli-auto-updater-974169037036.us-central1.run.app/manifests/{os}_{arch}.json"
    )
}

async fn bounded_response_text(response: reqwest::Response) -> Result<String, AppError> {
    if response
        .content_length()
        .is_some_and(|length| length > MAX_VERSION_RESPONSE_BYTES as u64)
    {
        return Err(AppError::ProviderUnavailable(
            "CLI release metadata response is too large".to_owned(),
        ));
    }
    let mut stream = response.bytes_stream();
    let mut bytes = Vec::new();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(|error| AppError::Anyhow(error.into()))?;
        if bytes.len().saturating_add(chunk.len()) > MAX_VERSION_RESPONSE_BYTES {
            return Err(AppError::ProviderUnavailable(
                "CLI release metadata response is too large".to_owned(),
            ));
        }
        bytes.extend_from_slice(&chunk);
    }
    String::from_utf8(bytes)
        .map_err(|_| AppError::ProviderUnavailable("CLI release metadata is not UTF-8".to_owned()))
}

async fn run_command(
    binary: &str,
    args: &[&str],
    cwd: &std::path::Path,
    timeout: Duration,
) -> Result<String, AppError> {
    let mut spec = CommandSpec::new(binary, cwd);
    spec.args = args.iter().map(|arg| (*arg).to_owned()).collect();
    let output = run_bounded_command(&spec, MAX_OUTPUT_BYTES, timeout).await?;
    if !output.success {
        return Err(AppError::ProviderUnavailable(
            "CLI command did not complete successfully".to_owned(),
        ));
    }
    let mut text = String::from_utf8_lossy(&output.stdout).trim().to_owned();
    if text.is_empty() {
        text = String::from_utf8_lossy(&output.stderr).trim().to_owned();
    }
    Ok(text)
}

fn extract_version(input: &str) -> Option<String> {
    let start = input.char_indices().find(|(_, ch)| ch.is_ascii_digit())?.0;
    let value = input[start..]
        .chars()
        .take_while(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '.' | '-' | '+'))
        .collect::<String>();
    (!value.is_empty()).then_some(value)
}

fn find_version_value(value: &Value) -> Option<String> {
    for key in [
        "latestVersion",
        "latest_version",
        "latest",
        "version",
        "tag_name",
    ] {
        if let Some(version) = value
            .get(key)
            .and_then(Value::as_str)
            .and_then(extract_version)
        {
            return Some(version);
        }
    }
    value.as_str().and_then(extract_version)
}

/// Numeric release parts first; on a tie a pre-release (`1.2.0-beta.1`)
/// precedes its release, and `+build` metadata is ignored, as in semver.
fn compare_versions(left: &str, right: &str) -> std::cmp::Ordering {
    use std::cmp::Ordering;
    let split = |value: &str| {
        let value = value.split('+').next().unwrap_or(value);
        match value.split_once('-') {
            Some((core, pre)) => (core.to_owned(), Some(pre.to_owned())),
            None => (value.to_owned(), None),
        }
    };
    let numbers = |core: &str| {
        core.split('.')
            .map(|part| part.parse::<u64>().unwrap_or(0))
            .collect::<Vec<_>>()
    };
    let ((left_core, left_pre), (right_core, right_pre)) = (split(left), split(right));
    let (left_core, right_core) = (numbers(&left_core), numbers(&right_core));
    for index in 0..left_core.len().max(right_core.len()) {
        match left_core
            .get(index)
            .copied()
            .unwrap_or(0)
            .cmp(&right_core.get(index).copied().unwrap_or(0))
        {
            Ordering::Equal => {}
            ordering => return ordering,
        }
    }
    match (left_pre, right_pre) {
        (None, None) => Ordering::Equal,
        (Some(_), None) => Ordering::Less,
        (None, Some(_)) => Ordering::Greater,
        (Some(left), Some(right)) => {
            let mut left = left.split('.');
            let mut right = right.split('.');
            loop {
                match (left.next(), right.next()) {
                    (None, None) => return Ordering::Equal,
                    (None, Some(_)) => return Ordering::Less,
                    (Some(_), None) => return Ordering::Greater,
                    (Some(left), Some(right)) => {
                        let ordering = match (left.parse::<u64>(), right.parse::<u64>()) {
                            (Ok(left), Ok(right)) => left.cmp(&right),
                            // Numeric identifiers sort before alphanumeric ones.
                            (Ok(_), Err(_)) => Ordering::Less,
                            (Err(_), Ok(_)) => Ordering::Greater,
                            (Err(_), Err(_)) => left.cmp(right),
                        };
                        if ordering != Ordering::Equal {
                            return ordering;
                        }
                    }
                }
            }
        }
    }
}

fn user_facing_error(error: &AppError) -> String {
    match error {
        AppError::ProviderUnavailable(message) | AppError::Unsupported(message) => message.clone(),
        _ => "CLI version operation failed".to_owned(),
    }
}

#[cfg(test)]
mod tests {
    use std::time::{Duration, Instant};

    use chrono::Utc;

    use super::{
        compare_versions, extract_version, npm_install_args, strip_ansi, CliLocks, CliManager,
        CliOperationAction, LatestRelease, ManagedCli, ProviderKind, ROW_CACHE_TTL,
    };

    #[test]
    fn extracts_versions_from_cli_output() {
        assert_eq!(
            extract_version("codex-cli 0.145.0"),
            Some("0.145.0".to_owned())
        );
        assert_eq!(
            extract_version("v2.1.259 (Claude Code)"),
            Some("2.1.259".to_owned())
        );
    }

    #[test]
    fn deepseek_harness_installs_and_upgrades_through_fixed_npm_command() {
        let cli = ManagedCli::DeepseekHarness;
        assert_eq!(cli.provider_kind(), ProviderKind::DeepseekHarness);
        assert!(cli.install_script().is_none());
        assert!(cli.upgrade_args().is_empty());
        assert_eq!(
            npm_install_args(cli.npm_package().unwrap()),
            [
                "install",
                "--global",
                "--no-fund",
                "--no-audit",
                "@deepseek-ai/dsh@latest"
            ]
        );
        assert!(ManagedCli::Opencode.npm_package().is_none());
    }

    #[test]
    fn compares_numeric_versions() {
        assert!(compare_versions("0.84.4", "0.85.0").is_lt());
        assert!(compare_versions("2.1.259", "2.1.259").is_eq());
        assert!(compare_versions("1.2.0", "1.1.9").is_gt());
    }

    #[test]
    fn pre_releases_precede_their_release() {
        assert!(compare_versions("1.2.0-beta.1", "1.2.0").is_lt());
        assert!(compare_versions("1.2.0", "1.2.0-rc.2").is_gt());
        assert!(compare_versions("1.2.0-beta.2", "1.2.0-beta.10").is_lt());
        assert!(compare_versions("1.2.0-1", "1.2.0-alpha").is_lt());
        assert!(compare_versions("1.2.0+build.5", "1.2.0").is_eq());
        assert!(compare_versions("1.2.1-beta.1", "1.2.0").is_gt());
    }

    #[tokio::test]
    async fn operations_are_single_flight_per_cli() {
        let manager = CliManager::default();
        let codex = manager
            .begin_operation(
                ManagedCli::Codex,
                CliOperationAction::Upgrade,
                Some("1.0.0".to_owned()),
            )
            .await
            .unwrap();
        // Another CLI runs alongside; the same CLI does not.
        let pi = manager
            .begin_operation(ManagedCli::Pi, CliOperationAction::Install, None)
            .await
            .unwrap();
        assert!(manager
            .begin_operation(ManagedCli::Codex, CliOperationAction::Upgrade, None)
            .await
            .is_err());
        let (first, running) = manager.running_operations().await;
        assert_eq!(first.unwrap().id, codex.id);
        assert_eq!(
            running
                .iter()
                .map(|item| item.id.as_str())
                .collect::<Vec<_>>(),
            [codex.id.as_str(), pi.id.as_str()]
        );

        manager
            .complete_operation(&codex.id, Ok(Some("1.1.0".to_owned())))
            .await;
        assert_eq!(manager.running_operations().await.1.len(), 1);
        assert!(manager
            .begin_operation(ManagedCli::Codex, CliOperationAction::Upgrade, None)
            .await
            .is_ok());
    }

    #[tokio::test]
    async fn completing_an_operation_forgets_only_that_cli() {
        let manager = CliManager::default();
        for cli in [ManagedCli::Codex, ManagedCli::Pi] {
            let row = manager.busy_row(cli).await;
            let mut cache = manager.cache();
            cache.rows.insert(cli, (Instant::now(), row));
            cache.latest.insert(
                cli,
                LatestRelease {
                    at: Instant::now(),
                    checked_at: Utc::now(),
                    version: Ok(Some("2.0.0".to_owned())),
                },
            );
        }
        let operation = manager
            .begin_operation(ManagedCli::Codex, CliOperationAction::Upgrade, None)
            .await
            .unwrap();
        manager.complete_operation(&operation.id, Ok(None)).await;
        let cache = manager.cache();
        assert!(!cache.rows.contains_key(&ManagedCli::Codex));
        assert!(!cache.latest.contains_key(&ManagedCli::Codex));
        assert!(cache.rows.contains_key(&ManagedCli::Pi));
        assert!(cache.latest.contains_key(&ManagedCli::Pi));
    }

    #[tokio::test]
    async fn a_cli_mid_upgrade_reports_its_last_row() {
        let manager = CliManager::default();
        let mut cached = manager.busy_row(ManagedCli::ClaudeCode).await;
        cached.current_version = Some("2.1.0".to_owned());
        cached.status = "updateAvailable".to_owned();
        manager.cache().rows.insert(
            ManagedCli::ClaudeCode,
            (Instant::now() - ROW_CACHE_TTL, cached),
        );
        let _upgrade = manager.locks().try_write(ManagedCli::ClaudeCode).unwrap();
        // Stale, and a refresh, yet the binary being replaced is not run.
        let row = manager
            .cached_row(ManagedCli::ClaudeCode, true, || async {
                unreachable!("a CLI mid-upgrade is not probed")
            })
            .await;
        assert_eq!(row.current_version.as_deref(), Some("2.1.0"));
        assert_eq!(row.status, "updateAvailable");
    }

    #[tokio::test]
    async fn cli_locks_scope_to_one_cli() {
        let locks = CliLocks::default();
        let upgrade = locks.try_write(ManagedCli::Codex).unwrap();
        assert!(!locks.is_idle());
        assert!(locks.try_read(&[ManagedCli::Codex]).is_none());
        assert!(locks
            .try_read(&[ManagedCli::ClaudeCode, ManagedCli::Codex])
            .is_none());
        let other = locks.try_read(&[ManagedCli::ClaudeCode]).unwrap();
        assert!(locks.try_write(ManagedCli::Pi).is_some());
        // A start of Codex waits for its upgrade, then proceeds.
        let waiting = tokio::spawn({
            let locks = locks.clone();
            async move { locks.read(&[ManagedCli::Codex]).await }
        });
        tokio::task::yield_now().await;
        assert!(!waiting.is_finished());
        drop(upgrade);
        drop(waiting.await.unwrap());
        // An upgrade waits a bounded time for starts holding the lock shared.
        assert!(locks
            .write_within(ManagedCli::ClaudeCode, Duration::from_millis(20))
            .await
            .is_none());
        drop(other);
        assert!(locks
            .write_within(ManagedCli::ClaudeCode, Duration::from_millis(20))
            .await
            .is_some());
        assert!(locks.is_idle());
    }

    #[test]
    fn strips_ansi_colors_from_installer_output() {
        assert_eq!(
            strip_ansi("\u{1b}[0;31mError:\u{1b}[0m Node.js 22 is required"),
            "Error: Node.js 22 is required"
        );
    }

    #[test]
    fn managed_cli_deserialization_rejects_arbitrary_commands() {
        assert!(serde_json::from_str::<ManagedCli>("\"codex\"").is_ok());
        assert!(serde_json::from_str::<ManagedCli>("\"sh -c whoami\"").is_err());
    }

    /// The image links its CLIs from `node_modules/.bin` into the package
    /// tree; either the link or its target under an image directory counts.
    #[cfg(unix)]
    #[test]
    fn image_managed_binaries_are_detected_through_symlinks() {
        use super::binary_in_dirs;
        use std::os::unix::fs::{symlink, PermissionsExt};

        let root = std::env::temp_dir().join(format!("todex-image-cli-{}", uuid::Uuid::new_v4()));
        let image = root.join("opt/todex/agents");
        let package = image.join("node_modules/@openai/codex/bin");
        std::fs::create_dir_all(&package).unwrap();
        std::fs::create_dir_all(image.join("node_modules/.bin")).unwrap();
        let target = package.join("codex.js");
        std::fs::write(&target, "#!/bin/sh\n").unwrap();
        std::fs::set_permissions(&target, std::fs::Permissions::from_mode(0o755)).unwrap();
        let link = image.join("node_modules/.bin/codex");
        symlink(&target, &link).unwrap();
        // A user-installed CLI linking into the image is the image's too.
        let user_bin = root.join("home/.local/bin");
        std::fs::create_dir_all(&user_bin).unwrap();
        symlink(&target, user_bin.join("codex")).unwrap();
        let own = user_bin.join("claude");
        std::fs::write(&own, "#!/bin/sh\n").unwrap();
        std::fs::set_permissions(&own, std::fs::Permissions::from_mode(0o755)).unwrap();
        // The configured directory itself may be a symlink.
        let alias = root.join("agents-alias");
        symlink(&image, &alias).unwrap();

        let link = link.to_str().unwrap();
        assert!(binary_in_dirs(link, std::slice::from_ref(&image)));
        assert!(binary_in_dirs(link, std::slice::from_ref(&alias)));
        assert!(binary_in_dirs(
            user_bin.join("codex").to_str().unwrap(),
            std::slice::from_ref(&image)
        ));
        assert!(!binary_in_dirs(
            own.to_str().unwrap(),
            &[image.clone(), alias]
        ));
        assert!(!binary_in_dirs(link, &[]));
        assert!(!binary_in_dirs(
            link,
            &[root.join("opt/todex/agents-other")]
        ));
        assert!(!binary_in_dirs(
            root.join("missing").to_str().unwrap(),
            std::slice::from_ref(&image)
        ));

        let mut config = crate::config::Config::default();
        config.agent.codex_bin = link.to_owned();
        assert!(!ManagedCli::Codex.managed_by_image(&config));
        config.deployment.image_cli_dirs = vec![image];
        assert!(ManagedCli::Codex.managed_by_image(&config));
        // `pi` resolves from PATH, outside the temporary image directory.
        assert!(!ManagedCli::Pi.managed_by_image(&config));
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn managed_by_is_omitted_unless_set() {
        let mut row = super::CliVersionInfo {
            id: "codex".to_owned(),
            name: "Codex".to_owned(),
            kind: "managed".to_owned(),
            installed: true,
            current_version: Some("1.0.0".to_owned()),
            latest_version: None,
            status: "unknown".to_owned(),
            upgrade_supported: true,
            install_supported: false,
            managed_by: None,
            error: None,
        };
        assert!(serde_json::to_value(&row)
            .unwrap()
            .get("managedBy")
            .is_none());
        row.managed_by = Some(super::MANAGED_BY_IMAGE.to_owned());
        assert_eq!(serde_json::to_value(&row).unwrap()["managedBy"], "image");
    }
}
