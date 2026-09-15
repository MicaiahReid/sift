//! Persistent IDL registry management commands.

use std::path::PathBuf;

use anyhow::{Result, bail};
use clap::{Args, Subcommand};
use sift_core::Registry;
use sift_types::IdlSource;
use tracing::debug;

use crate::{
    ContextOptions,
    tui::{idl_document, run, should_run},
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
    /// Browse the registry interactively when possible, otherwise print JSON.
    List {
        /// Print the registry JSON instead of opening the interactive browser.
        #[arg(long)]
        no_tui: bool,
    },
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
                    (Some(path), None, false) => {
                        let name = file_source_name(&path);
                        IdlSource::File { name, path }
                    }
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
            IdlSubcommand::List { no_tui } => {
                let use_tui = should_run(no_tui, !registry.config.programs.is_empty());
                debug!(
                    no_tui,
                    program_count = registry.config.programs.len(),
                    use_tui,
                    "selected IDL-list presentation"
                );
                if use_tui {
                    run(idl_document(registry))?;
                } else {
                    println!("{}", serde_json::to_string_pretty(&registry.json())?);
                }
            }
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

fn file_source_name(path: &std::path::Path) -> String {
    path.file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| path.display().to_string())
}
