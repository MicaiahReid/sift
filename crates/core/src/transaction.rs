//! Unsigned Solana transaction construction from normalized IDL instructions.

use std::collections::BTreeMap;
use std::str::FromStr;

use anyhow::{Context, Result, bail};
use serde_json::Value;
use sift_types::{
    Discriminator, FieldSchema, InstructionSchema, OptionalAccountStrategy, ProgramSchema,
};
use solana_address::Address;
use solana_hash::Hash;
use solana_instruction::{AccountMeta, Instruction};
use solana_message::{Message, VersionedMessage, v0, v1};
use solana_signature::Signature;
use solana_transaction::versioned::VersionedTransaction;
use tracing::debug;

use crate::codec::encode_type;
use crate::idl::find_instruction;
use crate::nodes::{kind, value_node_to_json};

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
    let metas = resolve_account_metas(definition, &schema.address, options)?;
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

/// Resolves each instruction account to an address, applying Codama defaults.
///
/// Caller-supplied addresses win. Otherwise a `publicKeyValueNode` default or
/// a `programIdValueNode` default is used; optional accounts without a value
/// follow the instruction's optional-account strategy; anything else is an error.
fn resolve_account_metas(
    instruction: &InstructionSchema,
    program_address: &str,
    options: &BuildOptions,
) -> Result<Vec<AccountMeta>> {
    let mut metas = Vec::new();
    for account in &instruction.accounts {
        let supplied = options
            .accounts
            .get(&account.name)
            .or_else(|| options.accounts.get(&to_kebab(&account.name)));
        let defaulted;
        let address = match supplied {
            Some(address) => address.as_str(),
            None => match default_address(account.default_value.as_ref(), program_address) {
                Some(address) => {
                    defaulted = address;
                    defaulted.as_str()
                }
                None if account.optional => match instruction.optional_account_strategy {
                    OptionalAccountStrategy::Omitted => continue,
                    OptionalAccountStrategy::ProgramId => {
                        metas.push(AccountMeta::new_readonly(
                            parse_address(program_address)?,
                            false,
                        ));
                        continue;
                    }
                },
                None => bail!("missing required account {}", account.name),
            },
        };
        let address =
            parse_address(address).with_context(|| format!("account {}", account.name))?;
        metas.push(if account.writable {
            AccountMeta::new(address, account.signer)
        } else {
            AccountMeta::new_readonly(address, account.signer)
        });
    }
    Ok(metas)
}

fn default_address(default_value: Option<&Value>, program_address: &str) -> Option<String> {
    let node = default_value?;
    match kind(node)? {
        "publicKeyValueNode" => node.get("publicKey")?.as_str().map(ToOwned::to_owned),
        "programIdValueNode" => Some(program_address.to_owned()),
        _ => None,
    }
}

/// Encodes instruction data from caller arguments and Codama defaults.
///
/// Arguments the caller leaves out fall back to their default value node.
/// A constant discriminator that an argument's default would produce must
/// come from that argument, so overriding it with a conflicting value is an
/// error. A constant discriminator that no argument describes is inserted at
/// its declared offset, which must fall on an argument boundary.
fn instruction_data(
    instruction: &InstructionSchema,
    args: &BTreeMap<String, Value>,
    definitions: &BTreeMap<String, Value>,
) -> Result<Vec<u8>> {
    let mut data = Vec::new();
    let mut boundaries = vec![0];
    let mut defaults = Vec::new();
    for field in &instruction.args {
        let default;
        let value = match (args.get(&field.name), &field.default_value) {
            (Some(value), _) if !field.omitted => value,
            (_, Some(node)) => {
                default = value_node_to_json(node)?;
                &default
            }
            (Some(value), None) => value,
            (None, None) => bail!("missing argument {}", field.name),
        };
        let start = data.len();
        encode_type(&field.ty, value, definitions, &mut data)
            .with_context(|| format!("argument {}", field.name))?;
        boundaries.push(data.len());
        defaults.push((start, field, encoded_default(field, definitions)));
    }
    for discriminator in &instruction.discriminators {
        let Discriminator::Constant { offset, bytes, .. } = discriminator else {
            continue;
        };
        if data.get(*offset..offset + bytes.len()) == Some(bytes.as_slice()) {
            continue;
        }
        if let Some((_, field, _)) = defaults.iter().find(|(start, _, default)| {
            start == offset && default.as_deref().is_some_and(|d| d.starts_with(bytes))
        }) {
            bail!(
                "argument {} must encode the instruction discriminator {bytes:?}; omit it to use its default",
                field.name
            )
        }
        if !boundaries.contains(offset) {
            bail!(
                "instruction discriminator at offset {offset} falls inside an encoded argument and no argument describes it"
            )
        }
        data.splice(offset..offset, bytes.iter().copied());
    }
    Ok(data)
}

/// Encodes an argument's default value, or `None` when it has none or fails to encode.
fn encoded_default(field: &FieldSchema, definitions: &BTreeMap<String, Value>) -> Option<Vec<u8>> {
    let value = value_node_to_json(field.default_value.as_ref()?).ok()?;
    let mut bytes = Vec::new();
    encode_type(&field.ty, &value, definitions, &mut bytes).ok()?;
    Some(bytes)
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::nodes::number;
    use serde_json::json;

    fn tagged_instruction(omitted: bool, declare_argument: bool) -> InstructionSchema {
        let mut args = vec![FieldSchema {
            name: "amount".into(),
            ty: number("u8"),
            default_value: None,
            omitted: false,
        }];
        if declare_argument {
            args.insert(
                0,
                FieldSchema {
                    name: "tag".into(),
                    ty: number("u8"),
                    default_value: Some(json!({"kind": "numberValueNode", "number": 3})),
                    omitted,
                },
            );
        }
        InstructionSchema {
            name: "tagged".into(),
            discriminators: vec![Discriminator::Constant {
                offset: 0,
                bytes: vec![3],
                inferred: false,
            }],
            args,
            accounts: vec![],
            optional_account_strategy: OptionalAccountStrategy::ProgramId,
        }
    }

    /// Overriding a non-omitted discriminator argument used to splice the
    /// expected tag in front of the caller's value, shifting every field.
    #[test]
    fn rejects_arguments_that_conflict_with_the_discriminator() {
        let definitions = BTreeMap::new();
        let instruction = tagged_instruction(false, true);
        let mut args = BTreeMap::new();
        args.insert("amount".to_owned(), json!(9));
        assert_eq!(
            instruction_data(&instruction, &args, &definitions).unwrap(),
            vec![3, 9]
        );
        args.insert("tag".to_owned(), json!(4));
        let error = instruction_data(&instruction, &args, &definitions).unwrap_err();
        assert!(error.to_string().contains("argument tag"), "{error}");

        let omitted = tagged_instruction(true, true);
        assert_eq!(
            instruction_data(&omitted, &args, &definitions).unwrap(),
            vec![3, 9]
        );
    }

    #[test]
    fn inserts_discriminators_no_argument_describes() {
        let definitions = BTreeMap::new();
        let instruction = tagged_instruction(false, false);
        let mut args = BTreeMap::new();
        args.insert("amount".to_owned(), json!(9));
        assert_eq!(
            instruction_data(&instruction, &args, &definitions).unwrap(),
            vec![3, 9]
        );
    }
}
