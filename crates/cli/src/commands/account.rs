//! Account inspection and offline byte-patching commands.

use anyhow::{Result, bail};
use clap::{Args, Subcommand};
use serde_json::Value;
use sift_types::{AccountSchema, ProgramSchema};
use tokio::sync::mpsc;

use sift_core::{
    Registry, RpcClient, decode_account_with_info, discriminator_filter, encode_account,
    find_account, patch_value,
};
use tracing::debug;

use crate::{
    ContextOptions,
    tui::{
        AccountPageUpdate, AccountPaging, DecodedAccount, RequestMetadata, account_document,
        account_list_json, run_paged_accounts, should_run,
    },
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
                let filters = discriminator_filter(layout);
                if should_run(no_tui, true) {
                    return browse_accounts(rpc, schema, account_type, metadata, filters).await;
                }
                let accounts = rpc.program_accounts(&schema.address, filters).await?;
                let result = decode_accounts(accounts, layout, &schema);
                println!(
                    "{}",
                    serde_json::to_string_pretty(&account_list_json(&result))?
                );
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

fn decode_accounts(
    accounts: impl IntoIterator<Item = (String, Vec<u8>)>,
    layout: &AccountSchema,
    schema: &ProgramSchema,
) -> Vec<DecodedAccount> {
    sift_core::decode_accounts(accounts, layout, schema)
        .accounts
        .into_iter()
        .map(|account| DecodedAccount {
            address: account.address,
            account_type: account.account_type,
            data: account.data,
            seeds: None,
            note: account.note,
        })
        .collect()
}

async fn browse_accounts(
    rpc: RpcClient,
    schema: ProgramSchema,
    account_type: String,
    metadata: RequestMetadata,
    filters: Value,
) -> Result<()> {
    let layout = find_account(&schema, &account_type)?.clone();
    let initial = account_document(&[], &layout.ty, &schema.types, metadata.clone());
    let (updates_tx, updates_rx) = mpsc::unbounded_channel();
    let (requests_tx, mut requests_rx) = mpsc::unbounded_channel();
    requests_tx.send(())?;
    let worker = tokio::spawn(async move {
        let mut addresses = None;
        let mut offset = 0;
        while requests_rx.recv().await.is_some() {
            if addresses.is_none() {
                match rpc
                    .program_account_addresses(&schema.address, filters.clone())
                    .await
                {
                    Ok(discovered) => {
                        if updates_tx
                            .send(AccountPageUpdate::Discovered {
                                addresses: discovered.clone(),
                                account_type: layout.name.clone(),
                            })
                            .is_err()
                        {
                            break;
                        }
                        addresses = Some(discovered);
                    }
                    Err(error) => {
                        if updates_tx
                            .send(AccountPageUpdate::Failed(format!("{error:#}")))
                            .is_err()
                        {
                            break;
                        }
                    }
                }
                continue;
            }
            let page: Result<AccountPageUpdate> = async {
                let addresses = addresses.as_ref().expect("addresses were loaded");
                let end = (offset + 100).min(addresses.len());
                let keys = &addresses[offset..end];
                let accounts = rpc.multiple_accounts(keys).await?;
                // Accounts may close or change owner between discovery and the page request.
                let accounts =
                    keys.iter()
                        .cloned()
                        .zip(accounts)
                        .filter_map(|(address, account)| {
                            account
                                .filter(|account| account.owner == schema.address)
                                .map(|account| (address, account.data))
                        });
                let decoded = decode_accounts(accounts, &layout, &schema);
                let start = offset;
                offset = end;
                Ok(AccountPageUpdate::Loaded {
                    start,
                    document: account_document(
                        &decoded,
                        &layout.ty,
                        &schema.types,
                        metadata.clone(),
                    ),
                    scanned: offset,
                    total: addresses.len(),
                })
            }
            .await;
            let update =
                page.unwrap_or_else(|error| AccountPageUpdate::Failed(format!("{error:#}")));
            if updates_tx.send(update).is_err() {
                break;
            }
        }
    });
    let result = run_paged_accounts(initial, AccountPaging::new(updates_rx, requests_tx));
    worker.abort();
    let _ = worker.await;
    result
}
