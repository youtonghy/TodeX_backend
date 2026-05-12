mod app_state;
mod codex_gateway;
mod config;
mod error;
mod event;
mod server;
mod server_runner;
mod transport_crypto;
mod tui;

use anyhow::Context;
use clap::{Parser, Subcommand};

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
    }
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
