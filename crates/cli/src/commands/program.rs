//! Deployed program inspection commands.

use std::io::IsTerminal;

use anyhow::Result;
use clap::{Args, Subcommand};
use dialoguer::console::style;
use serde_json::json;
use sift_core::{ProgramInfo, Registry, RpcClient, inspect_program, inspect_programs};

use crate::{
    ContextOptions,
    tui::{ListedProgram, program_list_document, run, should_run},
};

/// Program inspection command group.
#[derive(Args, Debug)]
pub(crate) struct ProgramCommand {
    #[command(subcommand)]
    command: ProgramSubcommand,
}

#[derive(Subcommand, Debug)]
enum ProgramSubcommand {
    /// Browse every registered program's on-chain state, otherwise print JSON.
    List {
        /// Print JSON instead of opening the interactive browser.
        #[arg(long)]
        no_tui: bool,
    },
    /// Show a deployed program's loader state, on-chain IDL, and verified build.
    Show {
        /// Program alias or base58 address.
        program: String,
        /// Print JSON even when standard output is a terminal.
        #[arg(long)]
        json: bool,
    },
}

impl ProgramCommand {
    /// Executes the selected program operation.
    pub(crate) async fn execute(self, registry: &Registry, context: &ContextOptions) -> Result<()> {
        match self.command {
            ProgramSubcommand::List { no_tui } => {
                let rpc = RpcClient::new(context.url.clone(), context.commitment.clone())?;
                let addresses = registry.config.programs.keys().cloned().collect::<Vec<_>>();
                let programs = inspect_programs(&rpc, &addresses)
                    .await
                    .into_iter()
                    .zip(&registry.config.programs)
                    .map(|(info, (address, entry))| ListedProgram {
                        address: address.clone(),
                        aliases: entry.aliases.iter().cloned().collect(),
                        info: info.map_err(|error| format!("{error:#}")),
                    })
                    .collect::<Vec<_>>();
                if should_run(no_tui, !programs.is_empty()) {
                    run(program_list_document(&programs, &rpc.url))?;
                } else {
                    let listed = programs
                        .iter()
                        .map(ListedProgram::as_json)
                        .collect::<Vec<_>>();
                    println!("{}", serde_json::to_string_pretty(&listed)?);
                }
            }
            ProgramSubcommand::Show { program, json } => {
                let address = registry.program_address(&program)?;
                let rpc = RpcClient::new(context.url.clone(), context.commitment.clone())?;
                let info = inspect_program(&rpc, &address).await?;
                let aliases = registry
                    .config
                    .programs
                    .get(&address)
                    .map(|entry| entry.aliases.iter().cloned().collect::<Vec<_>>())
                    .unwrap_or_default();
                if json || !std::io::stdout().is_terminal() {
                    let mut value = serde_json::to_value(&info)?;
                    value["aliases"] = json!(aliases);
                    println!("{}", serde_json::to_string_pretty(&value)?);
                } else {
                    print_info(&info, &aliases);
                }
            }
        }
        Ok(())
    }
}

fn print_info(info: &ProgramInfo, aliases: &[String]) {
    let none = || style("none").dim().to_string();
    let mut rows = vec![("Address", info.address.clone())];
    if !aliases.is_empty() {
        rows.push(("Aliases", aliases.join(", ")));
    }
    rows.push(("Loader", info.loader.clone()));
    rows.push((
        "ProgramData",
        info.programdata_address.clone().unwrap_or_else(none),
    ));
    rows.push((
        "Authority",
        info.authority
            .clone()
            .unwrap_or_else(|| style("none (immutable)").dim().to_string()),
    ));
    if let Some(slot) = info.last_deployed_slot {
        rows.push(("Deployed slot", slot.to_string()));
    }
    rows.push(("IDL", info.idl_address.clone().unwrap_or_else(none)));
    match &info.verified_build {
        Some(build) => {
            rows.push(("Verified", style("yes").green().to_string()));
            rows.push(("Repository", build.repo_url.clone()));
            rows.push(("Commit", build.commit.clone()));
            rows.push(("Hash", build.executable_hash.clone()));
            if let Some(at) = &build.last_verified_at {
                rows.push(("Verified at", at.clone()));
            }
        }
        None => rows.push(("Verified", none())),
    }
    let width = rows.iter().map(|(label, _)| label.len()).max().unwrap_or(0);
    for (label, value) in rows {
        println!("{}  {value}", style(format!("{label:<width$}")).bold());
    }
}
