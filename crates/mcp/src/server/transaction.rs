//! Unsigned transaction construction tool.

use std::collections::BTreeMap;

use anyhow::{Context, Result};
use base64::Engine;
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::{Value, json};
use sift_core::{BuildOptions, TransactionVersion, build_transaction, find_instruction};

use super::SiftServer;

/// Wire envelope for `transaction_build`.
#[derive(Clone, Copy, Debug, Default, Deserialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum WireVersion {
    /// Current 4KB v1 envelope (default).
    #[default]
    V1,
    /// Versioned v0 envelope.
    V0,
    /// Original legacy envelope.
    Legacy,
}

/// Input for `transaction_build`.
#[derive(Debug, Deserialize, JsonSchema)]
pub struct TransactionBuildParams {
    /// Program alias or base58 address.
    pub program: String,
    /// IDL instruction name. Case-insensitive.
    pub instruction: String,
    /// Map of instruction account name to base58 address. Every account without a derivable PDA must be present.
    #[serde(default)]
    pub accounts: BTreeMap<String, String>,
    /// Map of instruction argument name to JSON value. Integers wider than 32 bits should be strings.
    #[serde(default)]
    pub args: BTreeMap<String, Value>,
    /// Fee payer address. Defaults to the sole signer when exactly one account signs.
    #[serde(default)]
    pub payer: Option<String>,
    /// `v1` (default), `v0`, or `legacy`.
    #[serde(default)]
    pub version: WireVersion,
    /// v1 compute unit limit. Defaults to 1,400,000.
    #[serde(default)]
    pub compute_unit_limit: Option<u32>,
    /// v1 loaded account data cap in bytes. Defaults to 64 MiB.
    #[serde(default)]
    pub loaded_accounts_data_size_limit: Option<u32>,
    /// v1 heap size in bytes. Defaults to 32 KiB.
    #[serde(default)]
    pub heap_size: Option<u32>,
    /// v1 total priority fee in lamports. Defaults to 0.
    #[serde(default)]
    pub priority_fee: Option<u64>,
    /// RPC URL or cluster shorthand overriding the server default; used for the recent blockhash.
    #[serde(default)]
    pub rpc_url: Option<String>,
    /// RPC commitment overriding the server default.
    #[serde(default)]
    pub commitment: Option<String>,
}

pub(super) async fn build(server: &SiftServer, params: TransactionBuildParams) -> Result<Value> {
    let rpc = server.rpc(params.rpc_url.as_deref(), params.commitment.as_deref())?;
    let resolved = server.schema(&params.program, &rpc).await?;
    let schema = &resolved.resolved.schema;
    let instruction = find_instruction(schema, &params.instruction).with_context(|| {
        format!(
            "known instructions for {}: {:?}",
            schema.name,
            schema
                .instructions
                .iter()
                .map(|instruction| instruction.name.as_str())
                .collect::<Vec<_>>()
        )
    })?;
    let mut options = BuildOptions {
        payer: params.payer,
        accounts: params.accounts,
        args: params.args,
        recent_blockhash: rpc.latest_blockhash().await?,
        version: match params.version {
            WireVersion::V1 => TransactionVersion::V1,
            WireVersion::V0 => TransactionVersion::V0,
            WireVersion::Legacy => TransactionVersion::Legacy,
        },
        ..Default::default()
    };
    if let Some(value) = params.compute_unit_limit {
        options.compute_unit_limit = value;
    }
    if let Some(value) = params.loaded_accounts_data_size_limit {
        options.loaded_accounts_data_size_limit = value;
    }
    if let Some(value) = params.heap_size {
        options.heap_size = value;
    }
    if let Some(value) = params.priority_fee {
        options.priority_fee = value;
    }
    let built = build_transaction(schema, &instruction.name, &options).with_context(|| {
        format!(
            "instruction {} expects accounts {:?} and args {:?}",
            instruction.name,
            instruction
                .accounts
                .iter()
                .map(|account| account.name.as_str())
                .collect::<Vec<_>>(),
            instruction
                .args
                .iter()
                .map(|arg| arg.name.as_str())
                .collect::<Vec<_>>()
        )
    })?;
    Ok(resolved.annotate(json!({
        "program": schema.address,
        "instruction": instruction.name,
        "version": format!("{:?}", built.version).to_lowercase(),
        "fee_payer": built.payer,
        "signers": built.signers,
        "recent_blockhash": options.recent_blockhash,
        "wire_length": built.bytes.len(),
        "wire_base64": base64::engine::general_purpose::STANDARD.encode(&built.bytes),
    })))
}
