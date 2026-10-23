//! Local administration commands for headless daemons (a container, a
//! server without a terminal UI): `pair`, `devices` and `providers install`.
//!
//! They add no network entry point. Like the TUI, they act on the data
//! directory's control files, which only their owner (the daemon user) may
//! write, and they reuse the TUI's and the HTTP API's functions.
use std::io::{BufRead, IsTerminal, Write};
use std::path::{Path, PathBuf};

use clap::{Args, Subcommand, ValueEnum};

use crate::config::{Config, ServeArgs};
use crate::device_pairing::{self, DevicePairingRequestSummary, PairingLookup, PairingTarget};
use crate::error::AppError;
use crate::provider::{read_current_version, run_install, ManagedCli};

/// Not found, expired or ambiguous.
pub const EXIT_NOT_FOUND: i32 = 2;
/// The caller is not the user owning the daemon's control files.
pub const EXIT_WRONG_USER: i32 = 3;

#[derive(Debug, Clone, Copy, Default, Eq, PartialEq, ValueEnum)]
pub enum OutputFormat {
    #[default]
    Text,
    Json,
}

#[derive(Debug, Subcommand)]
pub enum PairCommand {
    #[command(about = "List device pairing requests waiting for a decision")]
    List {
        #[arg(long, value_enum, default_value_t)]
        format: OutputFormat,
        #[arg(long)]
        data_dir: Option<PathBuf>,
    },
    #[command(about = "Approve a device pairing request")]
    Approve(DecideArgs),
    #[command(about = "Reject a device pairing request")]
    Reject(DecideArgs),
}

#[derive(Debug, Args)]
pub struct DecideArgs {
    /// The verification code the device shows (case-insensitive, hyphen
    /// optional) or the request id from `pair list`.
    #[arg(value_name = "CODE|REQUEST_ID")]
    target: String,
    /// Skip the confirmation prompt a request id needs.
    #[arg(long)]
    yes: bool,
    #[arg(long)]
    data_dir: Option<PathBuf>,
}

#[derive(Debug, Subcommand)]
pub enum DevicesCommand {
    #[command(about = "List paired devices")]
    List {
        #[arg(long, value_enum, default_value_t)]
        format: OutputFormat,
        #[arg(long)]
        data_dir: Option<PathBuf>,
    },
    #[command(about = "Revoke a paired device")]
    Revoke {
        device_id: String,
        #[arg(long)]
        data_dir: Option<PathBuf>,
    },
}

#[derive(Debug, Subcommand)]
pub enum ProvidersCommand {
    #[command(about = "Install Agent CLIs with the same installers as the app")]
    Install {
        /// CLI ids (`codex`, `pi`, `claude-code`, `grok-build`, `devin`,
        /// `opencode`, `antigravity`, `deepseek-harness`), separated by
        /// spaces or commas.
        #[arg(value_delimiter = ',')]
        ids: Vec<String>,
        /// Skip CLIs that are already installed instead of failing.
        #[arg(long)]
        if_missing: bool,
        #[arg(long)]
        data_dir: Option<PathBuf>,
    },
}

/// A failed command: the message for stderr and the process exit code.
#[derive(Debug)]
pub struct CommandFailure {
    pub code: i32,
    pub message: String,
}

impl CommandFailure {
    fn new(code: i32, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
        }
    }
}

impl From<anyhow::Error> for CommandFailure {
    fn from(error: anyhow::Error) -> Self {
        Self::new(1, format!("{error:#}"))
    }
}

impl From<AppError> for CommandFailure {
    fn from(error: AppError) -> Self {
        Self::new(1, error.to_string())
    }
}

impl From<std::io::Error> for CommandFailure {
    fn from(error: std::io::Error) -> Self {
        Self::new(1, error.to_string())
    }
}

type CommandResult<T = ()> = Result<T, CommandFailure>;

/// Loads the configuration the way the daemon does, for its data directory.
fn load_config(data_dir: Option<PathBuf>) -> CommandResult<Config> {
    Config::load_read_only(ServeArgs {
        data_dir,
        ..Default::default()
    })
    .map_err(|error| CommandFailure::new(1, format!("failed to load configuration: {error:#}")))
}

/// Exit code 3 when this process cannot act on the control files: the daemon
/// accepts decisions only from files its own user owns.
fn ensure_control_owner(data_dir: &Path, command: &str) -> CommandResult {
    #[cfg(unix)]
    {
        let euid = unsafe { libc::geteuid() };
        if let Some(owner) = device_pairing::control_owner_mismatch(data_dir, euid) {
            return Err(CommandFailure::new(
                EXIT_WRONG_USER,
                format!(
                    "{} belongs to uid {owner}, but this command runs as uid {euid}; run it as \
                     the daemon's user (in a container: docker exec -u todex <container> \
                     todex-agentd {command} ...)",
                    data_dir.display()
                ),
            ));
        }
    }
    #[cfg(not(unix))]
    let _ = (data_dir, command);
    Ok(())
}

pub fn pair(command: PairCommand) -> CommandResult {
    match command {
        PairCommand::List { format, data_dir } => {
            let config = load_config(data_dir)?;
            ensure_control_owner(&config.data_dir, "pair list")?;
            let requests = device_pairing::list_device_pairing_requests(&config.data_dir)?;
            match format {
                OutputFormat::Json => {
                    println!(
                        "{}",
                        serde_json::to_string_pretty(&requests).map_err(AppError::from)?
                    )
                }
                OutputFormat::Text if requests.is_empty() => {
                    println!("No device pairing requests are waiting.")
                }
                OutputFormat::Text => {
                    for request in &requests {
                        print_request(request);
                    }
                }
            }
            Ok(())
        }
        PairCommand::Approve(args) => decide_command(args, true),
        PairCommand::Reject(args) => decide_command(args, false),
    }
}

fn decide_command(args: DecideArgs, approved: bool) -> CommandResult {
    let config = load_config(args.data_dir.clone())?;
    let summary = decide(&config.data_dir, &args, approved, confirm_interactively)?;
    println!(
        "{} device {:?} (code {}, transport fingerprint {}).",
        if approved { "Approved" } else { "Rejected" },
        summary.device_name,
        summary.verification_code,
        fingerprint(&summary)
    );
    if approved {
        println!("The device finishes pairing on its next poll.");
    }
    Ok(())
}

/// Records the decision for the request `args` names. A verification code
/// is its own confirmation; a request id is confirmed by `confirm` unless
/// `--yes` was given.
fn decide(
    data_dir: &Path,
    args: &DecideArgs,
    approved: bool,
    confirm: impl FnOnce(&DevicePairingRequestSummary, bool) -> CommandResult<bool>,
) -> CommandResult<DevicePairingRequestSummary> {
    let command = if approved {
        "pair approve"
    } else {
        "pair reject"
    };
    ensure_control_owner(data_dir, command)?;
    let (target, lookup) = device_pairing::find_device_pairing_request(data_dir, &args.target)?;
    let summary = match lookup {
        PairingLookup::Found(summary) => summary,
        PairingLookup::Expired(summary) => {
            return Err(CommandFailure::new(
                EXIT_NOT_FOUND,
                format!(
                    "the pairing request {} from {:?} has expired; start pairing again on the device",
                    summary.verification_code, summary.device_name
                ),
            ))
        }
        PairingLookup::NotFound => {
            return Err(CommandFailure::new(
                EXIT_NOT_FOUND,
                format!(
                    "no pairing request waiting for a decision matches {:?}; see `todex-agentd pair list`",
                    args.target.trim()
                ),
            ))
        }
        PairingLookup::Ambiguous(requests) => {
            let ids = requests
                .iter()
                .map(|request| request.request_id.as_str())
                .collect::<Vec<_>>()
                .join(", ");
            return Err(CommandFailure::new(
                EXIT_NOT_FOUND,
                format!(
                    "{} pairing requests have this code; name one by request id ({ids})",
                    requests.len()
                ),
            ));
        }
    };
    if target == PairingTarget::RequestId && !args.yes && !confirm(&summary, approved)? {
        return Err(CommandFailure::new(1, "cancelled; nothing was recorded"));
    }
    device_pairing::decide_device_pairing(data_dir, &summary.request_id, approved).map_err(
        |error| match error {
            // Expired or decided since the lookup.
            AppError::Conflict(message) | AppError::NotFound(message) => {
                CommandFailure::new(EXIT_NOT_FOUND, message)
            }
            other => other.into(),
        },
    )?;
    Ok(summary)
}

/// A request id does not show which code is approved, so ask on a terminal.
fn confirm_interactively(
    summary: &DevicePairingRequestSummary,
    approved: bool,
) -> CommandResult<bool> {
    if !std::io::stdin().is_terminal() {
        return Err(CommandFailure::new(
            1,
            "a request id needs confirmation: pass --yes, run in a terminal, or use the \
             verification code instead",
        ));
    }
    print_request(summary);
    print!(
        "{} this device? [y/N] ",
        if approved { "Approve" } else { "Reject" }
    );
    std::io::stdout().flush()?;
    let mut answer = String::new();
    std::io::stdin().lock().read_line(&mut answer)?;
    Ok(matches!(
        answer.trim().to_ascii_lowercase().as_str(),
        "y" | "yes"
    ))
}

fn fingerprint(summary: &DevicePairingRequestSummary) -> &str {
    summary
        .transport_fingerprint
        .as_deref()
        .unwrap_or("none (unencrypted transport)")
}

fn print_request(request: &DevicePairingRequestSummary) {
    let remaining =
        (request.expires_at / 1000).saturating_sub(chrono::Utc::now().timestamp().max(0) as u64);
    println!(
        "{}  {:?}  from {}  expires in {}m{:02}s",
        request.verification_code,
        request.device_name,
        request.peer_address,
        remaining / 60,
        remaining % 60
    );
    println!("  request id:            {}", request.request_id);
    if let Some(device_id) = &request.device_id {
        println!("  device id:             {device_id}");
    }
    println!("  transport fingerprint: {}", fingerprint(request));
}

pub fn devices(command: DevicesCommand) -> CommandResult {
    match command {
        DevicesCommand::List { format, data_dir } => {
            let config = load_config(data_dir)?;
            let devices = crate::devices::list_devices(&config.data_dir)?;
            match format {
                OutputFormat::Json => println!(
                    "{}",
                    serde_json::to_string_pretty(&devices).map_err(AppError::from)?
                ),
                OutputFormat::Text if devices.is_empty() => println!("No paired devices."),
                OutputFormat::Text => {
                    for device in &devices {
                        println!(
                            "{}  {:?}  paired {}  last seen {}",
                            device.device_id,
                            device.name,
                            format_ms(Some(device.paired_at)),
                            format_ms(device.last_seen_at)
                        );
                    }
                }
            }
            Ok(())
        }
        DevicesCommand::Revoke {
            device_id,
            data_dir,
        } => {
            let config = load_config(data_dir)?;
            ensure_control_owner(&config.data_dir, "devices revoke")?;
            let name = crate::devices::list_devices(&config.data_dir)
                .ok()
                .and_then(|devices| {
                    devices
                        .into_iter()
                        .find(|device| device.device_id == device_id)
                        .map(|device| device.name)
                });
            if crate::devices::revoke_device(&config.data_dir, &device_id)? {
                match name {
                    Some(name) => println!("Revoked device {device_id} ({name:?})."),
                    None => println!("Revoked device {device_id}."),
                }
                Ok(())
            } else {
                Err(CommandFailure::new(
                    EXIT_NOT_FOUND,
                    format!("no paired device has id {device_id}; see `todex-agentd devices list`"),
                ))
            }
        }
    }
}

fn format_ms(value: Option<u64>) -> String {
    value
        .and_then(|ms| i64::try_from(ms).ok())
        .and_then(chrono::DateTime::from_timestamp_millis)
        .map_or_else(
            || "never".to_owned(),
            |time| time.format("%Y-%m-%d %H:%M:%S UTC").to_string(),
        )
}

pub async fn providers(command: ProvidersCommand) -> CommandResult {
    match command {
        ProvidersCommand::Install {
            ids,
            if_missing,
            data_dir,
        } => {
            let config = load_config(data_dir)?;
            let mut clis = Vec::new();
            for id in ids.iter().map(|id| id.trim()).filter(|id| !id.is_empty()) {
                let cli = parse_cli(id)?;
                if !clis.contains(&cli) {
                    clis.push(cli);
                }
            }
            if clis.is_empty() {
                return Ok(());
            }
            // Installers run in the data directory.
            crate::secure_fs::ensure_owner_only_dir(&config.data_dir)?;
            install(&config, &clis, if_missing).await
        }
    }
}

fn parse_cli(id: &str) -> CommandResult<ManagedCli> {
    serde_json::from_value(serde_json::Value::String(id.to_owned())).map_err(|_| {
        CommandFailure::new(
            1,
            format!(
                "unknown CLI {id:?}; expected one of {}",
                ManagedCli::ALL.map(ManagedCli::id).join(", ")
            ),
        )
    })
}

/// `POST /v2/providers/{p}/install` without the daemon: the same installer,
/// run in order and waited for.
async fn install(config: &Config, clis: &[ManagedCli], if_missing: bool) -> CommandResult {
    let mut failed = 0;
    for &cli in clis {
        let name = cli.name();
        if cli.managed_by_image(config) {
            println!("{name}: ships with the container image; skipped");
            continue;
        }
        if let Ok(version) = read_current_version(config, cli).await {
            let version = version.unwrap_or_else(|| "unknown version".to_owned());
            if if_missing {
                println!("{name}: already installed ({version}); skipped");
            } else {
                eprintln!("{name}: already installed ({version})");
                failed += 1;
            }
            continue;
        }
        println!("{name}: installing...");
        match run_install(config, cli).await {
            Ok(version) => println!(
                "{name}: installed {}",
                version.as_deref().unwrap_or("(version unknown)")
            ),
            Err(error) => {
                eprintln!("{name}: install failed: {error}");
                failed += 1;
            }
        }
    }
    if failed > 0 {
        return Err(CommandFailure::new(
            1,
            format!("{failed} of {} CLI installs did not succeed", clis.len()),
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::device_pairing::write_test_request;
    use uuid::Uuid;

    struct Root(PathBuf);
    impl Root {
        fn new() -> Self {
            let root = std::env::temp_dir().join(format!("todex-admin-cli-{}", Uuid::new_v4()));
            std::fs::create_dir_all(&root).unwrap();
            Self(root)
        }
    }
    impl Drop for Root {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn args(target: &str, yes: bool) -> DecideArgs {
        DecideArgs {
            target: target.to_owned(),
            yes,
            data_dir: None,
        }
    }

    fn now_ms() -> u64 {
        chrono::Utc::now().timestamp_millis() as u64
    }

    fn never(_: &DevicePairingRequestSummary, _: bool) -> CommandResult<bool> {
        panic!("a verification code needs no confirmation")
    }

    #[test]
    fn approving_by_code_records_the_decision() {
        let root = Root::new();
        let id = write_test_request(&root.0, "ABCDE-FGHJK", now_ms() + 60_000);
        let summary = decide(&root.0, &args("abcdefghjk", false), true, never).unwrap();
        assert_eq!(summary.request_id, id);
        assert!(device_pairing::list_device_pairing_requests(&root.0)
            .unwrap()
            .is_empty());
        // Decided: a second decision finds nothing.
        let failure = decide(&root.0, &args("ABCDE-FGHJK", false), false, never).unwrap_err();
        assert_eq!(failure.code, EXIT_NOT_FOUND);
    }

    #[test]
    fn expired_missing_and_ambiguous_requests_exit_with_code_2() {
        let root = Root::new();
        let failure = decide(&root.0, &args("ABCDE-FGHJK", false), true, never).unwrap_err();
        assert_eq!(failure.code, EXIT_NOT_FOUND);

        write_test_request(&root.0, "QRSTV-WXYZ0", now_ms() - 1);
        let failure = decide(&root.0, &args("QRSTV-WXYZ0", false), true, never).unwrap_err();
        assert_eq!(failure.code, EXIT_NOT_FOUND);
        assert!(failure.message.contains("expired"), "{}", failure.message);

        let live = now_ms() + 60_000;
        let first = write_test_request(&root.0, "ABCDE-FGHJK", live);
        let second = write_test_request(&root.0, "ABCDE-FGHJK", live);
        let failure = decide(&root.0, &args("ABCDE-FGHJK", false), true, never).unwrap_err();
        assert_eq!(failure.code, EXIT_NOT_FOUND);
        assert!(failure.message.contains(&first) && failure.message.contains(&second));
        // Nothing was decided.
        assert_eq!(
            device_pairing::list_device_pairing_requests(&root.0)
                .unwrap()
                .len(),
            2
        );
    }

    #[test]
    fn a_request_id_needs_confirmation_or_yes() {
        let root = Root::new();
        let id = write_test_request(&root.0, "ABCDE-FGHJK", now_ms() + 60_000);
        let failure = decide(&root.0, &args(&id, false), true, |summary, approved| {
            assert_eq!(summary.verification_code, "ABCDE-FGHJK");
            assert!(approved);
            Ok(false)
        })
        .unwrap_err();
        assert_eq!(failure.code, 1);
        assert_eq!(
            device_pairing::list_device_pairing_requests(&root.0)
                .unwrap()
                .len(),
            1
        );
        let summary = decide(&root.0, &args(&id, true), false, never).unwrap();
        assert_eq!(summary.request_id, id);
        assert!(device_pairing::list_device_pairing_requests(&root.0)
            .unwrap()
            .is_empty());
    }

    /// `/` belongs to root: a non-root caller is told to switch users before
    /// anything is read or written.
    #[cfg(unix)]
    #[test]
    fn another_users_data_dir_exits_with_code_3() {
        if unsafe { libc::geteuid() } == 0 {
            return;
        }
        let failure = decide(Path::new("/"), &args("ABCDE-FGHJK", false), true, never).unwrap_err();
        assert_eq!(failure.code, EXIT_WRONG_USER);
        assert!(failure.message.contains("-u todex"), "{}", failure.message);
    }

    #[test]
    fn unknown_cli_ids_are_rejected() {
        assert_eq!(parse_cli("claude-code").unwrap(), ManagedCli::ClaudeCode);
        let failure = parse_cli("sh -c whoami").unwrap_err();
        assert!(failure.message.contains("deepseek-harness"));
    }
}
