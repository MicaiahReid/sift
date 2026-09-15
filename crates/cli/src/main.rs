//! Command-line frontend for the `wip` IDL-aware Solana tooling.

use std::{io::IsTerminal, path::PathBuf};

use anyhow::Result;
use clap::{ArgAction, Parser, Subcommand};
use dialoguer::console::style;
use tracing::{debug, error};
use tracing_subscriber::{EnvFilter, fmt};
use wip_core::Registry;

mod commands;
mod utils;

/// Solana account introspection and unsigned transaction construction.
#[derive(Parser, Debug)]
#[command(name = "wip", version, about)]
struct Cli {
    /// Explicit registry TOML path.
    #[arg(long, global = true)]
    config: Option<PathBuf>,
    /// Solana RPC endpoint or localhost/devnet/testnet/mainnet shorthand.
    #[arg(long, global = true)]
    url: Option<String>,
    /// RPC commitment used for read-only account queries.
    #[arg(long, global = true, default_value = "confirmed")]
    commitment: String,
    /// Use this IDL file for this invocation without modifying the registry.
    #[arg(long, global = true)]
    idl: Option<PathBuf>,
    /// Disable prompts; a single discovered local IDL is used transiently and never registered.
    #[arg(long, global = true)]
    no_input: bool,
    /// Emit debug logs; repeat for trace-level logs. `RUST_LOG` takes precedence.
    #[arg(short, long, action = ArgAction::Count, global = true)]
    verbose: u8,
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand, Debug)]
enum Command {
    /// Inspect or rewrite program-owned account data.
    Account(commands::account::AccountCommand),
    /// Build an unsigned transaction.
    Transaction(commands::transaction::TransactionCommand),
    /// Manage persistent IDL sources.
    Idl(commands::idl::IdlCommand),
}

/// Globally configured values shared by individual command implementations.
#[derive(Clone, Debug)]
pub(crate) struct ContextOptions {
    /// Optional RPC endpoint override.
    pub(crate) url: Option<String>,
    /// Requested RPC commitment.
    pub(crate) commitment: String,
    /// Optional transient IDL path.
    pub(crate) idl: Option<PathBuf>,
    /// Whether local IDL discovery must not prompt or persist its result.
    pub(crate) no_input: bool,
}

#[tokio::main]
async fn main() {
    let cli = Cli::parse();
    init_logging(cli.verbose);
    debug!(?cli, "parsed CLI arguments");
    if let Err(error) = run(cli).await {
        error!(error = ?error, "command failed");
        eprintln!("{} {error:#}", style("error:").red().bold());
        std::process::exit(1);
    }
}

fn init_logging(verbose: u8) {
    let fallback = match verbose {
        0 => "warn",
        1 => "wip=debug,wip_core=debug,warn",
        _ => "wip=trace,wip_core=trace,warn",
    };
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new(fallback));
    fmt()
        .with_env_filter(filter)
        .with_target(true)
        .with_ansi(std::io::stderr().is_terminal())
        .init();
}

async fn run(cli: Cli) -> Result<()> {
    let mut registry = Registry::load(cli.config).await?;
    let context = ContextOptions {
        url: cli.url,
        commitment: cli.commitment,
        idl: cli.idl,
        no_input: cli.no_input,
    };
    match cli.command {
        Command::Account(command) => command.execute(&mut registry, &context).await,
        Command::Transaction(command) => command.execute(&mut registry, &context).await,
        Command::Idl(command) => command.execute(&mut registry, &context).await,
    }
}
