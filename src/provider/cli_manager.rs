use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use chrono::Utc;
use futures_util::StreamExt;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio::sync::{Mutex, RwLock};
use uuid::Uuid;

use crate::config::Config;
use crate::error::AppError;

use super::process::{run_bounded_command, CommandSpec};

const VERSION_TIMEOUT: Duration = Duration::from_secs(12);
const UPGRADE_TIMEOUT: Duration = Duration::from_secs(10 * 60);
const MAX_OUTPUT_BYTES: usize = 64 * 1024;
const VERSION_CACHE_TTL: Duration = Duration::from_secs(30);
const MAX_VERSION_RESPONSE_BYTES: usize = 64 * 1024;

#[derive(Clone, Copy, Debug, Deserialize, Serialize, Eq, PartialEq)]
#[serde(rename_all = "kebab-case")]
pub enum ManagedCli {
    Codex,
    Pi,
    ClaudeCode,
    GrokBuild,
}

impl ManagedCli {
    pub fn id(self) -> &'static str {
        match self {
            Self::Codex => "codex",
            Self::Pi => "pi",
            Self::ClaudeCode => "claude-code",
            Self::GrokBuild => "grok-build",
        }
    }

    fn name(self) -> &'static str {
        match self {
            Self::Codex => "Codex",
            Self::Pi => "Pi",
            Self::ClaudeCode => "Claude Code",
            Self::GrokBuild => "Grok Build",
        }
    }

    fn binary(self, config: &Config) -> &str {
        match self {
            Self::Codex => &config.agent.codex_bin,
            Self::Pi => &config.agent.pi_bin,
            Self::ClaudeCode => &config.agent.claude_bin,
            Self::GrokBuild => &config.agent.grok_bin,
        }
    }

    fn upgrade_args(self) -> &'static [&'static str] {
        match self {
            Self::Codex => &["update"],
            Self::Pi => &["update", "--self", "--no-approve"],
            Self::ClaudeCode => &["update"],
            Self::GrokBuild => &["update"],
        }
    }
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
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CliVersionsResponse {
    pub clis: Vec<CliVersionInfo>,
    pub checked_at: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub active_operation: Option<CliUpgradeOperation>,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CliUpgradeOperation {
    pub id: String,
    pub provider: ManagedCli,
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

#[derive(Default)]
struct UpgradeState {
    active_id: Option<String>,
    operations: HashMap<String, CliUpgradeOperation>,
}

#[derive(Clone)]
pub struct CliManager {
    upgrades: Arc<Mutex<UpgradeState>>,
    version_check: Arc<Mutex<()>>,
    version_cache: Arc<RwLock<Option<(Instant, CliVersionsResponse)>>>,
}

impl Default for CliManager {
    fn default() -> Self {
        Self {
            upgrades: Arc::new(Mutex::new(UpgradeState::default())),
            version_check: Arc::new(Mutex::new(())),
            version_cache: Arc::new(RwLock::new(None)),
        }
    }
}

impl CliManager {
    pub async fn versions(&self, config: &Config) -> CliVersionsResponse {
        if let Some(cached) = self.cached_versions().await {
            return self.with_active_operation(cached).await;
        }
        let _check = self.version_check.lock().await;
        if let Some(cached) = self.cached_versions().await {
            return self.with_active_operation(cached).await;
        }
        let (codex, pi, claude, grok) = tokio::join!(
            inspect_cli(config, ManagedCli::Codex),
            inspect_cli(config, ManagedCli::Pi),
            inspect_cli(config, ManagedCli::ClaudeCode),
            inspect_cli(config, ManagedCli::GrokBuild),
        );
        let mut clis = vec![codex, pi, claude, grok];
        clis.extend(config.agent.acp_profiles.keys().map(|name| CliVersionInfo {
            id: format!("acp:{name}"),
            name: name.clone(),
            kind: "external".to_owned(),
            installed: true,
            current_version: None,
            latest_version: None,
            status: "external".to_owned(),
            upgrade_supported: false,
            error: None,
        }));
        let response = CliVersionsResponse {
            clis,
            checked_at: Utc::now().to_rfc3339(),
            active_operation: None,
        };
        *self.version_cache.write().await = Some((Instant::now(), response.clone()));
        self.with_active_operation(response).await
    }

    async fn cached_versions(&self) -> Option<CliVersionsResponse> {
        self.version_cache
            .read()
            .await
            .as_ref()
            .filter(|(checked, _)| checked.elapsed() < VERSION_CACHE_TTL)
            .map(|(_, response)| response.clone())
    }

    pub async fn cached_versions_while_busy(&self) -> Option<CliVersionsResponse> {
        let cached = self
            .version_cache
            .read()
            .await
            .as_ref()
            .map(|(_, response)| response.clone())?;
        Some(self.with_active_operation(cached).await)
    }

    async fn with_active_operation(
        &self,
        mut response: CliVersionsResponse,
    ) -> CliVersionsResponse {
        let state = self.upgrades.lock().await;
        response.active_operation = state
            .active_id
            .as_ref()
            .and_then(|id| state.operations.get(id))
            .cloned();
        response
    }

    pub async fn begin_upgrade(
        &self,
        provider: ManagedCli,
        previous_version: Option<String>,
    ) -> Result<CliUpgradeOperation, AppError> {
        let mut state = self.upgrades.lock().await;
        if let Some(id) = &state.active_id {
            let active = state
                .operations
                .get(id)
                .expect("active CLI operation must exist");
            return Err(AppError::Conflict(format!(
                "CLI upgrade {} is already running",
                active.provider.id()
            )));
        }
        let operation = CliUpgradeOperation {
            id: format!("cliup_{}", Uuid::new_v4().simple()),
            provider,
            status: "running".to_owned(),
            started_at: Utc::now().to_rfc3339(),
            finished_at: None,
            previous_version,
            current_version: None,
            error: None,
        };
        state.active_id = Some(operation.id.clone());
        state
            .operations
            .insert(operation.id.clone(), operation.clone());
        if state.operations.len() > 50 {
            let active_id = state.active_id.clone();
            let mut completed = state
                .operations
                .values()
                .filter(|item| Some(&item.id) != active_id.as_ref())
                .map(|item| (item.started_at.clone(), item.id.clone()))
                .collect::<Vec<_>>();
            completed.sort_unstable();
            for (_, id) in completed.into_iter().take(state.operations.len() - 50) {
                state.operations.remove(&id);
            }
        }
        Ok(operation)
    }

    pub async fn complete_upgrade(
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
        if state.active_id.as_deref() == Some(operation_id) {
            state.active_id = None;
        }
        let completed = state.operations.get(operation_id).cloned();
        drop(state);
        *self.version_cache.write().await = None;
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

pub async fn read_current_version(
    config: &Config,
    provider: ManagedCli,
) -> Result<Option<String>, AppError> {
    current_version(config, provider).await
}

async fn inspect_cli(config: &Config, provider: ManagedCli) -> CliVersionInfo {
    let current = current_version(config, provider).await;
    let latest = latest_version(config, provider).await;
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
        .or_else(|| latest.err())
        .map(|error| user_facing_error(&error));
    CliVersionInfo {
        id: provider.id().to_owned(),
        name: provider.name().to_owned(),
        kind: "managed".to_owned(),
        installed,
        current_version,
        latest_version,
        status: status.to_owned(),
        upgrade_supported: installed && config.security.auth_token.is_some(),
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
    let (url, field) = match provider {
        ManagedCli::Codex => (
            "https://api.github.com/repos/openai/codex/releases/latest",
            "tag_name",
        ),
        ManagedCli::Pi => ("https://pi.dev/api/latest-version", "version"),
        ManagedCli::ClaudeCode => (
            "https://registry.npmjs.org/@anthropic-ai%2fclaude-code/latest",
            "version",
        ),
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

fn compare_versions(left: &str, right: &str) -> std::cmp::Ordering {
    let parts = |value: &str| {
        value
            .split(['.', '-', '+'])
            .map(|part| part.parse::<u64>().unwrap_or(0))
            .collect::<Vec<_>>()
    };
    let left = parts(left);
    let right = parts(right);
    for index in 0..left.len().max(right.len()) {
        match left
            .get(index)
            .copied()
            .unwrap_or(0)
            .cmp(&right.get(index).copied().unwrap_or(0))
        {
            std::cmp::Ordering::Equal => {}
            ordering => return ordering,
        }
    }
    std::cmp::Ordering::Equal
}

fn user_facing_error(error: &AppError) -> String {
    match error {
        AppError::ProviderUnavailable(message) | AppError::Unsupported(message) => message.clone(),
        _ => "CLI version operation failed".to_owned(),
    }
}

#[cfg(test)]
mod tests {
    use super::{compare_versions, extract_version, CliManager, ManagedCli};

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
    fn compares_numeric_versions() {
        assert!(compare_versions("0.84.4", "0.85.0").is_lt());
        assert!(compare_versions("2.1.259", "2.1.259").is_eq());
        assert!(compare_versions("1.2.0", "1.1.9").is_gt());
    }

    #[tokio::test]
    async fn upgrades_are_single_flight() {
        let manager = CliManager::default();
        let first = manager
            .begin_upgrade(ManagedCli::Codex, Some("1.0.0".to_owned()))
            .await
            .unwrap();
        assert!(manager
            .begin_upgrade(ManagedCli::Pi, Some("1.0.0".to_owned()))
            .await
            .is_err());
        manager
            .complete_upgrade(&first.id, Ok(Some("1.1.0".to_owned())))
            .await;
        assert!(manager
            .begin_upgrade(ManagedCli::Pi, Some("1.0.0".to_owned()))
            .await
            .is_ok());
    }

    #[test]
    fn managed_cli_deserialization_rejects_arbitrary_commands() {
        assert!(serde_json::from_str::<ManagedCli>("\"codex\"").is_ok());
        assert!(serde_json::from_str::<ManagedCli>("\"sh -c whoami\"").is_err());
    }
}
