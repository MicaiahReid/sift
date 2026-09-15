//! Account inspection and offline byte-patching commands.

use anyhow::{Result, bail};
use clap::{Args, Subcommand};
use serde_json::{Value, json};
use tracing::debug;
use wip_core::{
    Registry, RpcClient, decode_account_with_info, encode_account, find_account, patch_value,
};

use crate::{
    ContextOptions,
    tui::{DecodedAccount, RequestMetadata, account_document, account_list_json, run, should_run},
    utils::{
        BytesFormat, parse_assignment, print_bytes, read_value_json, resolve_schema,
        resolve_schema_with_source,
    },
};

/// Account command group.
#[derive(Args, Debug)]
pub(crate) struct AccountCommand {
    #[command(subcommand)]
    command: AccountSubcommand,
}

#[derive(Subcommand, Debug)]
enum AccountSubcommand {
    /// List program-owned accounts that decode as TYPE.
    List {
        #[arg(value_name = "TYPE")]
        account_type: String,
        #[arg(long)]
        program: String,
        /// Print the decoded JSON instead of opening the interactive account browser.
        #[arg(long)]
        no_tui: bool,
    },
    /// Fetch, decode, and patch account bytes locally; this never writes on-chain.
    Patch {
        /// Address of the account to fetch.
        address: String,
        /// Program alias or base58 address that owns the IDL.
        #[arg(long)]
        program: String,
        /// Override discriminator-based account type selection.
        #[arg(long = "type")]
        account_type: Option<String>,
        /// Dynamic field replacement in `path=value` format.
        #[arg(long = "set", value_parser = parse_assignment)]
        patches: Vec<(String, Value)>,
        /// JSON object (literal, @file, or - for stdin) mapping paths to replacements.
        #[arg(long)]
        set_json: Option<String>,
        /// Encoding used for the patched bytes.
        #[arg(long, value_enum, default_value_t = BytesFormat::Base64)]
        output: BytesFormat,
    },
}

impl AccountCommand {
    /// Executes the selected account command.
    pub(crate) async fn execute(
        self,
        registry: &mut Registry,
        context: &ContextOptions,
    ) -> Result<()> {
        match self.command {
            AccountSubcommand::List {
                account_type,
                program,
                no_tui,
            } => {
                debug!(program, account_type, "listing program accounts");
                let resolved = resolve_schema_with_source(registry, &program, context).await?;
                let schema = resolved.schema;
                let layout = find_account(&schema, &account_type)?;
                let rpc = RpcClient::new(context.url.clone(), context.commitment.clone())?;
                let metadata = RequestMetadata {
                    account_type: layout.name.clone(),
                    program: schema.address.clone(),
                    idl_location: resolved.idl_location,
                    rpc_url: rpc.url.clone(),
                    commitment: rpc.commitment.clone(),
                };
                let filters = json!([{ "memcmp": { "offset": 0, "bytes": bs58::encode(&layout.discriminator).into_string() }}]);
                let accounts = rpc.program_accounts(&schema.address, filters).await?;
                let mut result = Vec::new();
                for (address, bytes) in accounts {
                    match decode_account_with_info(layout, &schema.types, &bytes) {
                        Ok((data, decode_info)) => {
                            let note = decode_info.has_unmodelled_data().then(|| {
                                format!(
                                    "Parsed with {} unmodelled trailing byte(s) ({} non-zero); the IDL does not describe this data.",
                                    decode_info.trailing_byte_count,
                                    decode_info.non_zero_trailing_byte_count,
                                )
                            });
                            result.push(DecodedAccount {
                                address,
                                account_type: layout.name.clone(),
                                data,
                                seeds: None,
                                note,
                            });
                        }
                        Err(e) => {
                            debug!(account = %layout.name, byte_length = bytes.len(), error = %e, "failed to decode account data");
                        }
                    }
                }
                debug!(account_type = %layout.name, matched_count = result.len(), "decoded matching program accounts");
                let use_tui = should_run(no_tui, !result.is_empty());
                debug!(
                    no_tui,
                    has_accounts = !result.is_empty(),
                    use_tui,
                    "selected account-list presentation"
                );
                if use_tui {
                    run(account_document(
                        &result,
                        &layout.ty,
                        &schema.types,
                        metadata,
                    ))?;
                } else {
                    println!(
                        "{}",
                        serde_json::to_string_pretty(&account_list_json(&result))?
                    );
                }
            }
            AccountSubcommand::Patch {
                address,
                program,
                account_type,
                mut patches,
                set_json,
                output,
            } => {
                debug!(program, address, requested_type = ?account_type, patch_count = patches.len(), "patching account bytes locally");
                let schema = resolve_schema(registry, &program, context).await?;
                let rpc = RpcClient::new(context.url.clone(), context.commitment.clone())?;
                let bytes = rpc
                    .account_data(&address)
                    .await?
                    .ok_or_else(|| anyhow::anyhow!("account {address} does not exist"))?;
                let layout = if let Some(account_type) = account_type {
                    find_account(&schema, &account_type)?
                } else {
                    schema
                        .accounts
                        .iter()
                        .find(|layout| bytes.starts_with(&layout.discriminator))
                        .ok_or_else(|| {
                            anyhow::anyhow!(
                                "no registered account discriminator matches; use --type"
                            )
                        })?
                };
                let (mut value, decode_info) =
                    decode_account_with_info(layout, &schema.types, &bytes)?;
                if decode_info.has_unmodelled_data() {
                    bail!(
                        "cannot patch {}: the IDL does not describe {} trailing byte(s), {} of which are non-zero; patching would discard them",
                        layout.name,
                        decode_info.trailing_byte_count,
                        decode_info.non_zero_trailing_byte_count,
                    );
                }
                patches.extend(read_value_json(set_json, "--set-json").await?);
                for (path, replacement) in patches {
                    patch_value(&mut value, &path, replacement)?;
                }
                let encoded = encode_account(layout, &schema.types, &value)?;
                debug!(account_type = %layout.name, encoded_length = encoded.len(), "encoded patched account bytes");
                print_bytes(&encoded, output, &value)?;
            }
        }
        Ok(())
    }
}
