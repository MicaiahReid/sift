//! Unsigned Solana transaction construction from normalized IDL instructions.

use std::collections::BTreeMap;
use std::str::FromStr;

use anyhow::{Context, Result, bail};
use serde_json::Value;
use solana_address::Address;
use solana_hash::Hash;
use solana_instruction::{AccountMeta, Instruction};
use solana_message::{Message, VersionedMessage, v0, v1};
use solana_signature::Signature;
use solana_transaction::versioned::VersionedTransaction;
use tracing::debug;
use wip_types::{InstructionSchema, ProgramSchema};

use crate::codec::encode_type;
use crate::idl::find_instruction;

/// Transaction envelope selected by the caller.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum TransactionVersion {
    /// Current 4KB v1 transaction envelope.
    #[default]
    V1,
    /// Versioned v0 envelope.
    V0,
    /// Original legacy envelope.
    Legacy,
}

/// Inputs and resource configuration for an unsigned transaction build.
#[derive(Clone, Debug)]
pub struct BuildOptions {
    /// Desired envelope; v1 is the default.
    pub version: TransactionVersion,
    /// Required fee payer or inferred sole signer.
    pub payer: Option<String>,
    /// Address values keyed by exact or kebab-case instruction account name.
    pub accounts: BTreeMap<String, String>,
    /// Argument values keyed by exact instruction argument name.
    pub args: BTreeMap<String, Value>,
    /// Cluster-supplied recent blockhash/lifetime token.
    pub recent_blockhash: String,
    /// v1 compute-unit limit; defaults to the safe maximum of 1,400,000.
    pub compute_unit_limit: u32,
    /// v1 loaded accounts data cap; defaults to 64 MiB.
    pub loaded_accounts_data_size_limit: u32,
    /// v1 heap request; defaults to 32 KiB.
    pub heap_size: u32,
    /// v1 total priority fee in lamports; defaults to zero.
    pub priority_fee: u64,
}

impl Default for BuildOptions {
    fn default() -> Self {
        Self {
            version: TransactionVersion::V1,
            payer: None,
            accounts: BTreeMap::new(),
            args: BTreeMap::new(),
            recent_blockhash: "11111111111111111111111111111111".to_owned(),
            compute_unit_limit: 1_400_000,
            loaded_accounts_data_size_limit: 64 * 1024 * 1024,
            heap_size: 32 * 1024,
            priority_fee: 0,
        }
    }
}

/// Serialized unsigned transaction plus inspectable metadata.
#[derive(Clone, Debug)]
pub struct BuiltTransaction {
    /// Full wire bytes including a zeroed placeholder for each required signature.
    pub bytes: Vec<u8>,
    /// Resolved fee payer.
    pub payer: String,
    /// Required signer addresses in message order.
    pub signers: Vec<String>,
    /// Version used to encode the envelope.
    pub version: TransactionVersion,
}

/// Builds a real unsigned Solana envelope. It never loads a keypair, signs, or sends.
pub fn build_transaction(
    schema: &ProgramSchema,
    instruction_name: &str,
    options: &BuildOptions,
) -> Result<BuiltTransaction> {
    debug!(program = %schema.address, instruction = instruction_name, version = ?options.version, account_count = options.accounts.len(), argument_count = options.args.len(), "building unsigned transaction");
    if options.heap_size < 32 * 1024
        || options.heap_size > 256 * 1024
        || !options.heap_size.is_multiple_of(1024)
    {
        bail!("heap size must be a 1024-byte multiple between 32768 and 262144")
    }
    let definition = find_instruction(schema, instruction_name)?;
    let payer = resolve_payer(definition, options)?;
    debug!(payer, "resolved transaction fee payer");
    let program_id = parse_address(&schema.address)?;
    let metas = definition
        .accounts
        .iter()
        .map(|account| {
            let address = options
                .accounts
                .get(&account.name)
                .or_else(|| options.accounts.get(&to_kebab(&account.name)))
                .ok_or_else(|| anyhow::anyhow!("missing required account {}", account.name))?;
            let address = parse_address(address)?;
            Ok(if account.writable {
                AccountMeta::new(address, account.signer)
            } else {
                AccountMeta::new_readonly(address, account.signer)
            })
        })
        .collect::<Result<Vec<_>>>()?;
    let data = instruction_data(definition, &options.args, &schema.types)?;
    let instruction = Instruction {
        program_id,
        accounts: metas,
        data,
    };
    let blockhash =
        Hash::from_str(&options.recent_blockhash).context("recent blockhash is invalid")?;
    let payer_address = parse_address(&payer)?;
    let message = match options.version {
        TransactionVersion::V1 => VersionedMessage::V1(v1::Message::try_compile_with_config(
            &payer_address,
            &[instruction],
            blockhash,
            v1::TransactionConfig::empty()
                .with_compute_unit_limit(options.compute_unit_limit)
                .with_loaded_accounts_data_size_limit(options.loaded_accounts_data_size_limit)
                .with_heap_size(options.heap_size)
                .with_priority_fee(options.priority_fee),
        )?),
        TransactionVersion::V0 => VersionedMessage::V0(v0::Message::try_compile(
            &payer_address,
            &[instruction],
            &[],
            blockhash,
        )?),
        TransactionVersion::Legacy => {
            VersionedMessage::Legacy(Message::new(&[instruction], Some(&payer_address)))
        }
    };
    let signers = message.static_account_keys()
        [..usize::from(message.header().num_required_signatures)]
        .iter()
        .map(ToString::to_string)
        .collect::<Vec<_>>();
    let transaction = VersionedTransaction {
        signatures: vec![Signature::default(); signers.len()],
        message,
    };
    let bytes =
        wincode::serialize(&transaction).context("cannot serialize unsigned transaction")?;
    debug!(version = ?options.version, signer_count = signers.len(), wire_length = bytes.len(), "built unsigned transaction");
    Ok(BuiltTransaction {
        bytes,
        payer,
        signers,
        version: options.version,
    })
}

fn resolve_payer(instruction: &InstructionSchema, options: &BuildOptions) -> Result<String> {
    if let Some(payer) = &options.payer {
        return Ok(payer.clone());
    }
    let signers = instruction
        .accounts
        .iter()
        .filter(|account| account.signer)
        .filter_map(|account| {
            options
                .accounts
                .get(&account.name)
                .or_else(|| options.accounts.get(&to_kebab(&account.name)))
        })
        .collect::<Vec<_>>();
    if signers.len() == 1 {
        Ok((*signers[0]).clone())
    } else {
        bail!("fee payer is ambiguous; provide --payer")
    }
}

fn instruction_data(
    instruction: &InstructionSchema,
    args: &BTreeMap<String, Value>,
    definitions: &BTreeMap<String, Value>,
) -> Result<Vec<u8>> {
    let mut data = instruction.discriminator.clone();
    for field in &instruction.args {
        encode_type(
            &field.ty,
            args.get(&field.name)
                .ok_or_else(|| anyhow::anyhow!("missing argument {}", field.name))?,
            definitions,
            &mut data,
        )?;
    }
    Ok(data)
}

fn parse_address(value: &str) -> Result<Address> {
    Address::from_str(value).context("invalid base58 public key")
}
fn to_kebab(value: &str) -> String {
    value
        .chars()
        .enumerate()
        .flat_map(|(index, character)| {
            if character.is_ascii_uppercase() {
                if index == 0 {
                    vec![character.to_ascii_lowercase()]
                } else {
                    vec!['-', character.to_ascii_lowercase()]
                }
            } else {
                vec![character]
            }
        })
        .collect()
}
