mod agent_browser;
mod agent_desktop;
mod agent_mcp;
mod agent_providers;
mod app_state;
mod autostart;
mod catalog;
mod codex_gateway;
mod computer;
mod config;
mod conversation;
mod daemon;
mod device_auth;
mod device_pairing;
mod devices;
mod error;
mod event;
mod external_command;
mod history_crypto;
mod history_keys;
mod kanban_scheduler;
mod kanban_store;
mod listen_addrs;
mod local_terminal;
mod mcp;
mod provider;
mod quota_store;
mod remote_fs;
mod secure_fs;
mod server;
mod server_runner;
mod ssh;
mod transport_crypto;
mod tui;
mod update;
mod version;
mod workspace_paths;
mod workspace_store;
mod workspace_trust;

use anyhow::Context;
use clap::{Args, Parser, Subcommand};

use crate::config::{Config, ServeArgs};
use crate::server_runner::{ManagedServer, ProviderProcessTracking};

#[derive(Debug, Parser)]
#[command(
    name = "todex-agentd",
    version = crate::version::APP_VERSION,
    about = "TodeX agent daemon backend"
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    #[command(about = "Run the backend server without an interactive UI")]
    Serve(ServeArgs),
    #[command(about = "Open the interactive terminal UI for starting and stopping the server")]
    Tui(ServeArgs),
    #[command(about = "Control the persistent backend daemon")]
    Daemon {
        #[command(subcommand)]
        command: DaemonCommand,
    },
    #[command(about = "Run read-only backend diagnostics")]
    Doctor {
        #[command(subcommand)]
        command: DoctorCommand,
    },
    #[command(about = "Check or install the latest packaged backend release")]
    Update {
        #[arg(long)]
        check: bool,
        #[command(flatten)]
        args: ServeArgs,
    },
    #[command(about = "Show the backend repository, version, and update status")]
    About,
    #[command(about = "Manage the agent browser's Chromium")]
    Browser {
        #[command(subcommand)]
        command: BrowserCommand,
    },
    #[command(name = "daemon-run", hide = true)]
    DaemonRun(ServeArgs),
    /// Stdio MCP bridge TodeX injects into agents for its tools.
    #[command(
        name = agent_mcp::BRIDGE_SUBCOMMAND,
        alias = agent_mcp::LEGACY_BRIDGE_SUBCOMMAND,
        hide = true
    )]
    AgentMcpBridge,
    /// `SSH_ASKPASS` helper for password-authenticated SFTP sessions.
    #[command(name = "ssh-askpass", hide = true)]
    SshAskpass {
        #[arg(allow_hyphen_values = true)]
        prompt: Option<String>,
    },
}

#[derive(Debug, Subcommand)]
enum DaemonCommand {
    #[command(about = "Start the backend as a detached daemon")]
    Start(ServeArgs),
    #[command(about = "Stop the running backend daemon")]
    Stop(ServeArgs),
    #[command(about = "Restart the backend daemon")]
    Restart(ServeArgs),
    #[command(about = "Show backend daemon status")]
    Status(ServeArgs),
    #[command(about = "Manage launching the daemon automatically at login")]
    Autostart {
        #[command(subcommand)]
        command: AutostartCommand,
    },
}

#[derive(Debug, Subcommand)]
enum AutostartCommand {
    #[command(about = "Start the daemon automatically when you log in")]
    Enable(ServeArgs),
    #[command(about = "Stop the daemon from launching at login")]
    Disable,
    #[command(about = "Show whether the daemon launches at login")]
    Status,
}

#[derive(Debug, Subcommand)]
enum BrowserCommand {
    #[command(about = "Download and verify the Chromium this release pins")]
    Install {
        #[arg(long)]
        data_dir: Option<std::path::PathBuf>,
    },
}

#[derive(Debug, Subcommand)]
enum DoctorCommand {
    #[command(about = "Check Codex, Pi and OpenCode installation, login, and RPC discovery")]
    Providers(ProviderDoctorArgs),
}

#[derive(Debug, Args)]
struct ProviderDoctorArgs {
    #[arg(long, value_delimiter = ',', default_value = "codex,pi")]
    provider: Vec<String>,
    #[arg(long)]
    data_dir: Option<std::path::PathBuf>,
    #[arg(long)]
    workspace_root: Vec<std::path::PathBuf>,
    #[arg(long, default_value = "json")]
    format: String,
}

fn main() -> anyhow::Result<()> {
    let cli = Cli::parse_from(remote_fs::askpass::rewrite_args(
        std::env::args_os().collect(),
    ));
    // Processes that serve agents host Computer Use: they take their own
    // macOS permission identity (before any thread exists) and leave the
    // main thread to the host UI.
    let hosts_computer_use = matches!(
        cli.command,
        Command::Serve(_) | Command::Tui(_) | Command::DaemonRun(_)
    );
    if hosts_computer_use {
        computer::platform::adopt_own_permission_identity();
    }
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?;
    if hosts_computer_use {
        computer::host_ui::run_with_main_loop(move || runtime.block_on(run(cli)));
    }
    runtime.block_on(run(cli))
}

async fn run(cli: Cli) -> anyhow::Result<()> {
    match &cli.command {
        Command::Serve(args) | Command::Tui(args) => {
            let config = Config::load_read_only(args.clone())?;
            if daemon::status(&config)?.is_none() {
                update::before_start().await?;
            }
        }
        _ => {}
    }

    match cli.command {
        Command::Serve(args) => {
            init_serve_logging();
            serve(args).await
        }
        Command::Tui(args) => {
            init_tui_logging();
            tui::run(args).await
        }
        Command::Daemon { command } => {
            init_serve_logging();
            daemon_command(command).await
        }
        Command::Doctor { command } => doctor_command(command).await,
        Command::Browser {
            command: BrowserCommand::Install { data_dir },
        } => {
            let config = Config::load_read_only(ServeArgs {
                host: None,
                port: None,
                data_dir,
                workspace_root: Vec::new(),
                history_retention_days: None,
            })?;
            agent_browser::install_cli(&config.data_dir).await
        }
        Command::Update { check, args } => {
            if !check && update::enabled() {
                let config = Config::load_read_only(args)?;
                if daemon::status(&config)?.is_some() {
                    anyhow::bail!(
                        "stop the daemon before installing an update (or use daemon restart)"
                    );
                }
            }
            println!(
                "{}",
                serde_json::to_string_pretty(&update::run(check).await?)?
            );
            Ok(())
        }
        Command::About => {
            println!("{}", serde_json::to_string_pretty(&update::about().await)?);
            Ok(())
        }
        Command::DaemonRun(args) => {
            init_serve_logging();
            daemon_run(args).await
        }
        // No logging: stdout carries the MCP protocol.
        Command::AgentMcpBridge => agent_mcp::run_bridge().await,
        Command::SshAskpass { prompt } => {
            std::process::exit(remote_fs::askpass::run(prompt.as_deref()))
        }
    }
}

async fn doctor_command(command: DoctorCommand) -> anyhow::Result<()> {
    match command {
        DoctorCommand::Providers(args) => {
            if args.format != "json" {
                anyhow::bail!("doctor providers currently supports only --format json");
            }
            let config = Config::load_read_only(ServeArgs {
                host: None,
                port: None,
                data_dir: args.data_dir,
                workspace_root: args.workspace_root,
                history_retention_days: None,
            })
            .context("failed to read configuration")?;
            let report = provider::inspect_providers(&config, &args.provider).await?;
            println!("{}", serde_json::to_string_pretty(&report)?);
            if !report.success {
                anyhow::bail!("one or more provider checks failed");
            }
        }
    }
    Ok(())
}

fn init_serve_logging() {
    tracing_subscriber::fmt()
        .with_env_filter(default_env_filter())
        .init();
}

fn init_tui_logging() {
    tracing_subscriber::fmt()
        .with_env_filter(default_env_filter())
        .with_writer(std::io::sink)
        .init();
}

fn default_env_filter() -> tracing_subscriber::EnvFilter {
    tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| "todex_agentd=info,tower_http=info".into())
}

async fn serve(args: ServeArgs) -> anyhow::Result<()> {
    let config = Config::load(args).context("failed to load configuration")?;
    ManagedServer::start(config, ProviderProcessTracking::Enabled)
        .await?
        .wait()
        .await
}

async fn daemon_run(args: ServeArgs) -> anyhow::Result<()> {
    let config = Config::load(args).context("failed to load configuration")?;
    daemon::run(config).await
}

async fn daemon_command(command: DaemonCommand) -> anyhow::Result<()> {
    match command {
        DaemonCommand::Start(args) => {
            let config = Config::load(args).context("failed to load configuration")?;
            if daemon::status(&config)?.is_none() {
                update::before_start().await?;
            }
            let process = daemon::start(config).await?;
            println!(
                "Daemon running: pid={} listen={}",
                process.pid,
                process.listen_addr()
            );
            print_connect_addresses(&process);
        }
        DaemonCommand::Stop(args) => {
            let config = Config::load(args).context("failed to load configuration")?;
            match daemon::stop(&config).await? {
                Some(process) => println!("Daemon stopped: pid={}", process.pid),
                None => println!("Daemon is already stopped."),
            }
        }
        DaemonCommand::Restart(args) => {
            let config = Config::load(args).context("failed to load configuration")?;
            // The user explicitly requested a restart; update only after the old
            // daemon is stopped so its executable identity remains verifiable.
            daemon::stop(&config).await?;
            update::before_start().await?;
            let process = daemon::start(config).await?;
            println!(
                "Daemon restarted: pid={} listen={}",
                process.pid,
                process.listen_addr()
            );
            print_connect_addresses(&process);
        }
        DaemonCommand::Status(args) => {
            let config = Config::load(args).context("failed to load configuration")?;
            match daemon::status(&config)? {
                Some(process) => {
                    println!(
                        "Daemon running: pid={} listen={} started_at={}",
                        process.pid,
                        process.listen_addr(),
                        process.started_at.to_rfc3339()
                    );
                    print_connect_addresses(&process);
                }
                None => println!("Daemon stopped."),
            }
        }
        DaemonCommand::Autostart { command } => autostart_command(command)?,
    }

    Ok(())
}

fn autostart_command(command: AutostartCommand) -> anyhow::Result<()> {
    fn print_note(registration: &autostart::Registration) {
        if let Some(note) = &registration.note {
            println!("note: {note}");
        }
    }

    match command {
        AutostartCommand::Enable(args) => {
            let config = Config::load(args).context("failed to load configuration")?;
            let registration = autostart::enable(&config)?;
            println!("Autostart enabled: {}", registration.location);
            println!("The daemon will launch at your next login.");
            print_note(&registration);
        }
        AutostartCommand::Disable => {
            let registration = autostart::disable()?;
            if registration.enabled {
                println!("Autostart is still enabled: {}", registration.location);
            } else {
                println!("Autostart disabled: {}", registration.location);
            }
            print_note(&registration);
        }
        AutostartCommand::Status => {
            let registration = autostart::status()?;
            if registration.enabled {
                println!("Autostart enabled: {}", registration.location);
            } else {
                println!("Autostart disabled ({}).", registration.location);
            }
        }
    }
    Ok(())
}

fn print_connect_addresses(process: &daemon::DaemonProcess) {
    match listen_addrs::connect_addresses(&process.host) {
        Ok(addresses) => {
            for address in addresses {
                let interface = address
                    .interface
                    .as_deref()
                    .map(|name| format!(" ({name})"))
                    .unwrap_or_default();
                println!("  connect: {}{interface}", address.ws_url(process.port));
            }
        }
        Err(error) => eprintln!("warning: failed to list network interface addresses: {error}"),
    }
}
