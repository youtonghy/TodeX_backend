mod app_state;
mod catalog;
mod codex_gateway;
mod config;
mod conversation;
mod daemon;
mod error;
mod event;
mod local_terminal;
mod mcp;
mod provider;
mod server;
mod server_runner;
mod transport_crypto;
mod tui;
mod workspace_paths;
mod workspace_store;
mod workspace_trust;

use anyhow::Context;
use clap::{Args, Parser, Subcommand};

use crate::config::{Config, ServeArgs};
use crate::server_runner::ManagedServer;

#[derive(Debug, Parser)]
#[command(name = "todex-agentd", version, about = "TodeX agent daemon backend")]
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
    #[command(name = "daemon-run", hide = true)]
    DaemonRun(ServeArgs),
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
}

#[derive(Debug, Subcommand)]
enum DoctorCommand {
    #[command(about = "Check Codex and Pi installation, login, and RPC discovery")]
    Providers(ProviderDoctorArgs),
}

#[derive(Debug, Args)]
struct ProviderDoctorArgs {
    #[arg(long, value_delimiter = ',', default_value = "codex,pi")]
    provider: Vec<String>,
    #[arg(long)]
    data_dir: Option<std::path::PathBuf>,
    #[arg(long)]
    workspace_root: Option<std::path::PathBuf>,
    #[arg(long, default_value = "json")]
    format: String,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();

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
        Command::DaemonRun(args) => {
            init_serve_logging();
            daemon_run(args).await
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
    ManagedServer::start(config).await?.wait().await
}

async fn daemon_run(args: ServeArgs) -> anyhow::Result<()> {
    let config = Config::load(args).context("failed to load configuration")?;
    daemon::run(config).await
}

async fn daemon_command(command: DaemonCommand) -> anyhow::Result<()> {
    match command {
        DaemonCommand::Start(args) => {
            let config = Config::load(args).context("failed to load configuration")?;
            let process = daemon::start(config).await?;
            println!(
                "Daemon running: pid={} listen={}",
                process.pid,
                process.listen_addr()
            );
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
            let process = daemon::restart(config).await?;
            println!(
                "Daemon restarted: pid={} listen={}",
                process.pid,
                process.listen_addr()
            );
        }
        DaemonCommand::Status(args) => {
            let config = Config::load(args).context("failed to load configuration")?;
            match daemon::status(&config)? {
                Some(process) => println!(
                    "Daemon running: pid={} listen={} started_at={}",
                    process.pid,
                    process.listen_addr(),
                    process.started_at.to_rfc3339()
                ),
                None => println!("Daemon stopped."),
            }
        }
    }

    Ok(())
}
