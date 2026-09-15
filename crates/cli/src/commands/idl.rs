//! Persistent IDL registry management commands.

use std::path::PathBuf;

use anyhow::{Result, bail};
use clap::{Args, Subcommand};
use tracing::debug;
use wip_core::Registry;
use wip_types::IdlSource;

use crate::{
    ContextOptions,
    utils::{print_status, resolve_schema},
};

/// IDL registry command group.
#[derive(Args, Debug)]
pub(crate) struct IdlCommand {
    #[command(subcommand)]
    command: IdlSubcommand,
}

#[derive(Subcommand, Debug)]
enum IdlSubcommand {
    /// Add an IDL source at highest precedence.
    Add {
        /// Program base58 address.
        program: String,
        /// Local JSON IDL.
        #[arg(long, conflicts_with_all = ["url", "onchain"])]
        file: Option<PathBuf>,
        /// HTTP(S) JSON IDL.
        #[arg(long, conflicts_with_all = ["file", "onchain"])]
        url: Option<url::Url>,
        /// Register the canonical on-chain source.
        #[arg(long, conflicts_with_all = ["file", "url"])]
        onchain: bool,
        /// Friendly alias for the program.
        #[arg(long)]
        alias: Option<String>,
    },
    /// Print the registry as JSON.
    List,
    /// Show the resolved normalized program schema.
    Show {
        /// Program alias or base58 address.
        program: String,
    },
    /// Remove a program and all of its registered sources.
    Remove {
        /// Program alias or base58 address.
        program: String,
    },
}

impl IdlCommand {
    /// Executes the selected registry operation.
    pub(crate) async fn execute(
        self,
        registry: &mut Registry,
        context: &ContextOptions,
    ) -> Result<()> {
        match self.command {
            IdlSubcommand::Add {
                program,
                file,
                url,
                onchain,
                alias,
            } => {
                let source = match (file, url, onchain) {
                    (Some(path), None, false) => IdlSource::File {
                        name: path.display().to_string(),
                        path,
                    },
                    (None, Some(url), false) => IdlSource::Url {
                        name: url.to_string(),
                        url,
                    },
                    (None, None, true) | (None, None, false) => IdlSource::OnChain {
                        name: "canonical-onchain".to_owned(),
                        address: None,
                    },
                    _ => unreachable!(),
                };
                debug!(program, source = ?source, alias = ?alias, "adding IDL registry source");
                registry.add_source(&program, source, alias)?;
                registry.save().await?;
                print_status(&format!("Registered IDL for {program}"));
            }
            IdlSubcommand::List => println!("{}", serde_json::to_string_pretty(&registry.json())?),
            IdlSubcommand::Show { program } => println!(
                "{}",
                serde_json::to_string_pretty(&resolve_schema(registry, &program, context).await?)?
            ),
            IdlSubcommand::Remove { program } => {
                debug!(program, "removing IDL registry entry");
                let address = registry.program_address(&program)?;
                if registry.config.programs.remove(&address).is_none() {
                    bail!("program {program} is not registered")
                }
                registry.save().await?;
                print_status(&format!("Removed IDL registry entry for {address}"));
            }
        }
        Ok(())
    }
}
