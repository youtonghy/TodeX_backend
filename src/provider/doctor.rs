use std::{path::Path, time::Instant};

use serde::Serialize;
use tokio::time::{timeout, Duration};
use uuid::Uuid;

use crate::{config::Config, conversation::ProviderKind, error::AppError};

use super::{
    codex::CodexDriver,
    pi::PiDriver,
    process::{redact_sensitive_text, run_bounded_command, CommandSpec},
    types::ProviderDriver,
};

const PROBE_TIMEOUT: Duration = Duration::from_secs(15);
const MAX_DIAGNOSTIC_BYTES: usize = 32 * 1024;

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ProviderDoctorReport {
    pub schema_version: u32,
    pub generated_at: String,
    pub backend_version: &'static str,
    pub billable: bool,
    pub workspace_isolated: bool,
    pub success: bool,
    pub providers: Vec<ProviderProbeReport>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ProviderProbeReport {
    pub provider: ProviderKind,
    pub binary: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub version: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub selected_model: Option<String>,
    pub model_count: usize,
    pub command_count: usize,
    pub success: bool,
    pub stages: Vec<ProbeStage>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ProbeStage {
    pub name: &'static str,
    pub status: &'static str,
    pub code: &'static str,
    pub message: String,
    pub duration_ms: u64,
}

pub async fn inspect_providers(
    config: &Config,
    requested: &[String],
) -> Result<ProviderDoctorReport, AppError> {
    if requested.is_empty() {
        return Err(AppError::InvalidRequest(
            "at least one provider is required".to_owned(),
        ));
    }
    let root = std::env::temp_dir().join(format!("todex-provider-doctor-{}", Uuid::new_v4()));
    let workspace = root.join("workspace");
    tokio::fs::create_dir_all(&workspace).await?;
    set_owner_only_directory(&root).await?;
    set_owner_only_directory(&workspace).await?;

    let mut providers = Vec::new();
    for requested_provider in requested {
        let provider = requested_provider
            .parse::<ProviderKind>()
            .map_err(AppError::InvalidRequest)?;
        if !matches!(provider, ProviderKind::Codex | ProviderKind::Pi) {
            return Err(AppError::Unsupported(format!(
                "provider doctor currently supports codex and pi, got {}",
                provider.as_str()
            )));
        }
        providers.push(inspect_provider(config, provider, &workspace).await);
    }
    let _ = tokio::fs::remove_dir_all(&root).await;
    let success = providers.iter().all(|provider| provider.success);
    Ok(ProviderDoctorReport {
        schema_version: 1,
        generated_at: chrono::Utc::now().to_rfc3339(),
        backend_version: env!("CARGO_PKG_VERSION"),
        billable: false,
        workspace_isolated: true,
        success,
        providers,
    })
}

async fn inspect_provider(
    config: &Config,
    provider: ProviderKind,
    workspace: &Path,
) -> ProviderProbeReport {
    let binary = match provider {
        ProviderKind::Codex => config.agent.codex_bin.clone(),
        ProviderKind::Pi => config.agent.pi_bin.clone(),
        _ => unreachable!("provider was validated by inspect_providers"),
    };
    let driver: Box<dyn ProviderDriver> = match provider {
        ProviderKind::Codex => Box::new(CodexDriver::new(&config.agent)),
        ProviderKind::Pi => Box::new(PiDriver::new(&config.agent)),
        _ => unreachable!("provider was validated by inspect_providers"),
    };
    let mut report = ProviderProbeReport {
        provider,
        binary: binary.clone(),
        version: None,
        selected_model: None,
        model_count: 0,
        command_count: 0,
        success: true,
        stages: Vec::new(),
    };

    let started = Instant::now();
    match probe_command(&binary, &["--version"], workspace).await {
        Ok(version) => {
            report.version = Some(version.clone());
            report.stages.push(pass_stage("binary", version, started));
        }
        Err(message) => {
            report.success = false;
            report
                .stages
                .push(fail_stage("binary", "BINARY_UNAVAILABLE", message, started));
            return report;
        }
    }

    if provider == ProviderKind::Codex {
        let started = Instant::now();
        match probe_command(&binary, &["login", "status"], workspace).await {
            Ok(_) => report.stages.push(pass_stage(
                "auth",
                "Codex login is available".to_owned(),
                started,
            )),
            Err(message) => {
                report.success = false;
                report
                    .stages
                    .push(fail_stage("auth", "AUTH_UNAVAILABLE", message, started));
            }
        }
    }

    let started = Instant::now();
    match timeout(PROBE_TIMEOUT, driver.discover_models(workspace)).await {
        Ok(Ok(models)) if !models.is_empty() => {
            report.model_count = models.len();
            report.selected_model = models
                .iter()
                .find(|model| model.is_default)
                .or_else(|| models.first())
                .map(|model| model.id.clone());
            report.stages.push(pass_stage(
                "models",
                format!("discovered {} models", report.model_count),
                started,
            ));
        }
        Ok(Ok(_)) => {
            report.success = false;
            report.stages.push(fail_stage(
                "models",
                "MODEL_UNAVAILABLE",
                "provider returned no models".to_owned(),
                started,
            ));
        }
        Ok(Err(error)) => {
            report.success = false;
            report.stages.push(fail_stage(
                "models",
                "PROTOCOL_HANDSHAKE_FAILED",
                safe_message(&error.to_string()),
                started,
            ));
        }
        Err(_) => {
            report.success = false;
            report.stages.push(fail_stage(
                "models",
                "PROTOCOL_TIMEOUT",
                "provider model discovery timed out".to_owned(),
                started,
            ));
        }
    }

    if provider == ProviderKind::Pi {
        let started = Instant::now();
        if let Some(model) = report.selected_model.as_deref() {
            match probe_command(
                &binary,
                &["auth", "check", "--model", model, "--json", "--no-refresh"],
                workspace,
            )
            .await
            {
                Ok(_) => report.stages.push(pass_stage(
                    "auth",
                    "Pi credentials are available for the selected model".to_owned(),
                    started,
                )),
                Err(message) => {
                    report.success = false;
                    report
                        .stages
                        .push(fail_stage("auth", "AUTH_UNAVAILABLE", message, started));
                }
            }
        } else {
            report.success = false;
            report.stages.push(fail_stage(
                "auth",
                "MODEL_UNAVAILABLE",
                "Pi authentication could not be checked without a model".to_owned(),
                started,
            ));
        }
    }

    let started = Instant::now();
    match timeout(PROBE_TIMEOUT, driver.discover_commands(workspace)).await {
        Ok(Ok(commands)) => {
            report.command_count = commands.len();
            report.stages.push(pass_stage(
                "commands",
                format!("discovered {} commands", report.command_count),
                started,
            ));
        }
        Ok(Err(error)) => {
            report.success = false;
            report.stages.push(fail_stage(
                "commands",
                "PROTOCOL_HANDSHAKE_FAILED",
                safe_message(&error.to_string()),
                started,
            ));
        }
        Err(_) => {
            report.success = false;
            report.stages.push(fail_stage(
                "commands",
                "PROTOCOL_TIMEOUT",
                "provider command discovery timed out".to_owned(),
                started,
            ));
        }
    }
    report
}

async fn probe_command(binary: &str, args: &[&str], workspace: &Path) -> Result<String, String> {
    let mut spec = CommandSpec::new(binary, workspace);
    spec.args = args.iter().map(|arg| (*arg).to_owned()).collect();
    let output = run_bounded_command(&spec, MAX_DIAGNOSTIC_BYTES, PROBE_TIMEOUT)
        .await
        .map_err(|error| safe_message(&error.to_string()))?;
    if !output.success {
        let detail = if output.stderr.is_empty() {
            String::from_utf8_lossy(&output.stdout).into_owned()
        } else {
            String::from_utf8_lossy(&output.stderr).into_owned()
        };
        return Err(safe_message(&detail));
    }
    Ok(safe_message(&String::from_utf8_lossy(&output.stdout)))
}

fn pass_stage(name: &'static str, message: String, started: Instant) -> ProbeStage {
    ProbeStage {
        name,
        status: "pass",
        code: "OK",
        message,
        duration_ms: elapsed_millis(started),
    }
}

fn fail_stage(
    name: &'static str,
    code: &'static str,
    message: String,
    started: Instant,
) -> ProbeStage {
    ProbeStage {
        name,
        status: "fail",
        code,
        message,
        duration_ms: elapsed_millis(started),
    }
}

fn elapsed_millis(started: Instant) -> u64 {
    started.elapsed().as_millis().try_into().unwrap_or(u64::MAX)
}

fn safe_message(message: &str) -> String {
    redact_sensitive_text(message.trim())
        .chars()
        .take(500)
        .collect()
}

async fn set_owner_only_directory(path: &Path) -> Result<(), AppError> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        tokio::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700)).await?;
    }
    #[cfg(not(unix))]
    let _ = path;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn safe_message_redacts_and_bounds_diagnostics() {
        let value = format!("Bearer secret {}", "x".repeat(700));
        let safe = safe_message(&value);
        assert!(!safe.contains("secret"));
        assert!(safe.chars().count() <= 500);
    }
}
