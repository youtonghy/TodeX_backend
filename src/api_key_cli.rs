//! `todex-agentd api-key …`: manages `api-keys.json` directly. A running
//! daemon picks the changes up from the file; turns of a revoked key are
//! cancelled within seconds.
use std::path::PathBuf;

use anyhow::Context;
use clap::{Args, Subcommand};

use crate::api_keys::{ApiKeyScopes, ApiKeyStore, ApiKeyUpdate, ApprovalPolicy, NewApiKey};
use crate::config::{Config, ServeArgs};
use crate::conversation::ProviderKind;

#[derive(Debug, Subcommand)]
pub(crate) enum ApiKeyCommand {
    #[command(about = "Create a key; the key is printed once")]
    Create(CreateArgs),
    #[command(about = "List keys (never their secrets)")]
    List(DataDirArgs),
    #[command(about = "Change a key's name, scopes, approval policy or expiry")]
    Update(UpdateArgs),
    #[command(about = "Revoke a key; its running turns are cancelled")]
    Revoke {
        id: String,
        #[command(flatten)]
        data: DataDirArgs,
    },
}

#[derive(Debug, Args)]
pub(crate) struct DataDirArgs {
    #[arg(long)]
    data_dir: Option<PathBuf>,
}

#[derive(Debug, Args)]
pub(crate) struct CreateArgs {
    #[arg(long)]
    name: String,
    /// Agents the key may use (repeatable); all when omitted.
    #[arg(long = "agent")]
    agents: Vec<String>,
    /// Workspaces the key may use (repeatable); listing one also trusts it
    /// for the key. All workspace roots when omitted.
    #[arg(long = "workspace")]
    workspaces: Vec<PathBuf>,
    /// ask (default), auto-approve or reject.
    #[arg(long, default_value = "ask")]
    approval: String,
    #[arg(long)]
    expires_days: Option<u64>,
    #[command(flatten)]
    data: DataDirArgs,
}

#[derive(Debug, Args)]
pub(crate) struct UpdateArgs {
    id: String,
    #[arg(long)]
    name: Option<String>,
    #[arg(long = "agent")]
    agents: Vec<String>,
    /// Allow every agent again.
    #[arg(long, conflicts_with = "agents")]
    all_agents: bool,
    #[arg(long = "workspace")]
    workspaces: Vec<PathBuf>,
    /// Allow every workspace root again.
    #[arg(long, conflicts_with = "workspaces")]
    all_workspaces: bool,
    #[arg(long)]
    approval: Option<String>,
    #[arg(long, conflicts_with = "no_expiry")]
    expires_days: Option<u64>,
    #[arg(long)]
    no_expiry: bool,
    #[command(flatten)]
    data: DataDirArgs,
}

fn store(data: DataDirArgs) -> anyhow::Result<ApiKeyStore> {
    let config = Config::load_read_only(ServeArgs {
        data_dir: data.data_dir,
        ..ServeArgs::default()
    })?;
    std::fs::create_dir_all(&config.data_dir)
        .with_context(|| format!("failed to create {}", config.data_dir.display()))?;
    Ok(ApiKeyStore::load(&config.data_dir)?)
}

fn agents(values: Vec<String>) -> anyhow::Result<Option<Vec<ProviderKind>>> {
    if values.is_empty() {
        return Ok(None);
    }
    values
        .iter()
        .map(|value| value.parse().map_err(anyhow::Error::msg))
        .collect::<anyhow::Result<Vec<_>>>()
        .map(Some)
}

fn workspaces(values: Vec<PathBuf>) -> anyhow::Result<Option<Vec<PathBuf>>> {
    if values.is_empty() {
        return Ok(None);
    }
    values
        .into_iter()
        .map(|path| {
            std::fs::canonicalize(&path)
                .with_context(|| format!("workspace {} does not exist", path.display()))
        })
        .collect::<anyhow::Result<Vec<_>>>()
        .map(Some)
}

fn approval(value: &str) -> anyhow::Result<ApprovalPolicy> {
    ApprovalPolicy::parse(value)
        .with_context(|| format!("approval must be ask, auto-approve or reject, not {value}"))
}

fn expiry(days: u64) -> u64 {
    crate::api_keys::unix_ms().saturating_add(days.saturating_mul(24 * 60 * 60 * 1000))
}

pub(crate) fn run(command: ApiKeyCommand) -> anyhow::Result<()> {
    match command {
        ApiKeyCommand::Create(args) => {
            let store = store(args.data)?;
            let (record, key) = store.create(NewApiKey {
                name: args.name,
                scopes: ApiKeyScopes {
                    agents: agents(args.agents)?,
                    workspaces: workspaces(args.workspaces)?,
                },
                approval: approval(&args.approval)?,
                expires_at: args.expires_days.map(expiry),
            })?;
            println!("Created API key {} ({})", record.id, record.name);
            println!("Store it now; it is not shown again:");
            println!("{}", key.as_str());
        }
        ApiKeyCommand::List(data) => {
            let keys = store(data)?.list()?;
            if keys.is_empty() {
                println!("No API keys.");
            }
            let now = crate::api_keys::unix_ms();
            for key in keys {
                let agents = key.scopes.agents.as_ref().map_or_else(
                    || "*".to_owned(),
                    |agents| {
                        agents
                            .iter()
                            .map(|agent| agent.as_str())
                            .collect::<Vec<_>>()
                            .join(",")
                    },
                );
                let workspaces = key.scopes.workspaces.as_ref().map_or_else(
                    || "*".to_owned(),
                    |paths| {
                        paths
                            .iter()
                            .map(|path| path.display().to_string())
                            .collect::<Vec<_>>()
                            .join(",")
                    },
                );
                println!(
                    "{}  {:<8} {:<24} approval={} agents={} workspaces={}",
                    key.prefix(),
                    key.status(now),
                    key.name,
                    key.approval.as_str(),
                    agents,
                    workspaces
                );
            }
        }
        ApiKeyCommand::Update(args) => {
            let store = store(args.data)?;
            let current = store
                .get(&args.id)?
                .with_context(|| format!("no API key {}", args.id))?;
            let mut scopes = current.scopes.clone();
            if args.all_agents {
                scopes.agents = None;
            } else if let Some(agents) = agents(args.agents)? {
                scopes.agents = Some(agents);
            }
            if args.all_workspaces {
                scopes.workspaces = None;
            } else if let Some(workspaces) = workspaces(args.workspaces)? {
                scopes.workspaces = Some(workspaces);
            }
            let record = store.update(
                &args.id,
                ApiKeyUpdate {
                    name: args.name,
                    scopes: Some(scopes),
                    approval: args.approval.as_deref().map(approval).transpose()?,
                    expires_at: if args.no_expiry {
                        Some(None)
                    } else {
                        args.expires_days.map(|days| Some(expiry(days)))
                    },
                },
            )?;
            println!("Updated API key {} ({})", record.id, record.name);
        }
        ApiKeyCommand::Revoke { id, data } => {
            if store(data)?.revoke(&id)? {
                println!("Revoked API key {id}");
            } else {
                anyhow::bail!("no active API key {id}");
            }
        }
    }
    Ok(())
}
