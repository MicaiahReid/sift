//! Unsigned transaction construction commands.

use anyhow::Result;
use base64::Engine;
use clap::{Args, Subcommand, ValueEnum};
use serde_json::{Value, json};
use tracing::debug;
use wip_core::{BuildOptions, Registry, RpcClient, TransactionVersion, build_transaction};

use crate::{
    ContextOptions,
    utils::{
        account_map, parse_assignment, read_account_json, read_value_json, resolve_schema,
        value_map,
    },
};

/// Transaction command group.
#[derive(Args, Debug)]
pub(crate) struct TransactionCommand {
    #[command(subcommand)]
    command: TransactionSubcommand,
}

#[derive(Subcommand, Debug)]
enum TransactionSubcommand {
    /// Build, but never sign or send, one IDL instruction transaction.
    Build {
        /// IDL instruction name.
        instruction: String,
        /// Program alias or base58 address.
        #[arg(long)]
        program: String,
        /// Required account in `name=base58-address` format.
        #[arg(long, value_parser = parse_assignment)]
        account: Vec<(String, Value)>,
        /// Instruction argument in `name=value` format.
        #[arg(long, value_parser = parse_assignment)]
        arg: Vec<(String, Value)>,
        /// JSON object (literal, @file, or -) of account values.
        #[arg(long)]
        accounts_json: Option<String>,
        /// JSON object (literal, @file, or -) of argument values.
        #[arg(long)]
        args_json: Option<String>,
        /// Explicit fee payer; otherwise infer the sole signer.
        #[arg(long)]
        payer: Option<String>,
        /// Wire transaction version; v1 is the default.
        #[arg(long, value_enum, default_value_t = CliTransactionVersion::V1)]
        transaction_version: CliTransactionVersion,
        /// v1 maximum compute units.
        #[arg(long)]
        compute_unit_limit: Option<u32>,
        /// v1 account-data load cap in bytes.
        #[arg(long)]
        loaded_accounts_data_size_limit: Option<u32>,
        /// v1 heap request in bytes.
        #[arg(long)]
        heap_size: Option<u32>,
        /// v1 total priority fee in lamports.
        #[arg(long)]
        priority_fee: Option<u64>,
        /// Wire transaction presentation.
        #[arg(long, value_enum, default_value_t = TransactionOutput::Base64)]
        output: TransactionOutput,
    },
}

/// User-selectable message envelope.
#[derive(Clone, Copy, Debug, ValueEnum)]
enum CliTransactionVersion {
    V1,
    V0,
    Legacy,
}

/// User-selectable transaction output encoding.
#[derive(Clone, Copy, Debug, ValueEnum)]
enum TransactionOutput {
    Base64,
    Base58,
    Breakdown,
}

impl TransactionCommand {
    /// Executes an unsigned transaction build.
    pub(crate) async fn execute(
        self,
        registry: &mut Registry,
        context: &ContextOptions,
    ) -> Result<()> {
        let TransactionSubcommand::Build {
            instruction,
            program,
            account,
            arg,
            accounts_json,
            args_json,
            payer,
            transaction_version,
            compute_unit_limit,
            loaded_accounts_data_size_limit,
            heap_size,
            priority_fee,
            output,
        } = self.command;
        debug!(program, instruction, account_count = account.len(), argument_count = arg.len(), transaction_version = ?transaction_version, "building transaction from CLI arguments");
        let schema = resolve_schema(registry, &program, context).await?;
        let rpc = RpcClient::new(context.url.clone(), context.commitment.clone())?;
        let mut accounts = account_map(account)?;
        accounts.extend(read_account_json(accounts_json).await?);
        let mut args = value_map(arg);
        args.extend(read_value_json(args_json, "--args-json").await?);
        let mut options = BuildOptions {
            payer,
            accounts,
            args,
            recent_blockhash: rpc.latest_blockhash().await?,
            ..Default::default()
        };
        options.version = match transaction_version {
            CliTransactionVersion::V1 => TransactionVersion::V1,
            CliTransactionVersion::V0 => TransactionVersion::V0,
            CliTransactionVersion::Legacy => TransactionVersion::Legacy,
        };
        if let Some(value) = compute_unit_limit {
            options.compute_unit_limit = value;
        }
        if let Some(value) = loaded_accounts_data_size_limit {
            options.loaded_accounts_data_size_limit = value;
        }
        if let Some(value) = heap_size {
            options.heap_size = value;
        }
        if let Some(value) = priority_fee {
            options.priority_fee = value;
        }
        let built = build_transaction(&schema, &instruction, &options)?;
        debug!(version = ?built.version, signer_count = built.signers.len(), wire_length = built.bytes.len(), "rendering unsigned transaction output");
        match output {
            TransactionOutput::Base64 => println!(
                "{}",
                base64::engine::general_purpose::STANDARD.encode(built.bytes)
            ),
            TransactionOutput::Base58 => println!("{}", bs58::encode(built.bytes).into_string()),
            TransactionOutput::Breakdown => println!(
                "{}",
                serde_json::to_string_pretty(
                    &json!({"version":format!("{:?}",built.version).to_lowercase(),"feePayer":built.payer,"signers":built.signers,"wireLength":built.bytes.len(),"wireBase64":base64::engine::general_purpose::STANDARD.encode(built.bytes)})
                )?
            ),
        }
        Ok(())
    }
}
