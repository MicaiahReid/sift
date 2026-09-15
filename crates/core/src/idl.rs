//! Conversion from Anchor and common Codama JSON documents to an internal schema.

use anyhow::{Context, Result, bail};
use serde_json::Value;
use sha2::{Digest, Sha256};
use sift_types::{
    AccountSchema, FieldSchema, InstructionAccountSchema, InstructionSchema, ProgramSchema,
};
use tracing::{debug, trace};

/// Parses an Anchor (legacy or current) IDL, or a compatible common Codama shape.
pub fn parse_idl(input: &str, program_address: &str) -> Result<ProgramSchema> {
    trace!(
        program_address,
        byte_length = input.len(),
        "parsing IDL document"
    );
    let root: Value = serde_json::from_str(input).context("IDL is not valid JSON")?;
    let name = root
        .pointer("/name")
        .or_else(|| root.pointer("/metadata/name"))
        .and_then(Value::as_str)
        .unwrap_or("unnamed")
        .to_owned();
    let mut types = std::collections::BTreeMap::new();
    for definition in root
        .get("types")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        if let (Some(name), Some(ty)) = (
            definition.get("name").and_then(Value::as_str),
            definition.get("type"),
        ) {
            types.insert(name.to_owned(), ty.clone());
        }
    }
    let accounts = root
        .get("accounts")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .map(|account| parse_account(account, &types))
        .collect::<Result<Vec<_>>>()?;
    let instructions = root
        .get("instructions")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .map(parse_instruction)
        .collect::<Result<Vec<_>>>()?;
    if accounts.is_empty() && instructions.is_empty() {
        bail!("unsupported IDL: expected an Anchor/Codama accounts or instructions array")
    }
    debug!(
        program_address,
        name,
        account_count = accounts.len(),
        instruction_count = instructions.len(),
        type_count = types.len(),
        "parsed IDL document"
    );
    Ok(ProgramSchema {
        name,
        address: program_address.to_owned(),
        accounts,
        instructions,
        types,
    })
}

fn parse_account(
    value: &Value,
    definitions: &std::collections::BTreeMap<String, Value>,
) -> Result<AccountSchema> {
    let name = required_string(value, "name", "account")?;
    let ty = value
        .get("type")
        .cloned()
        .or_else(|| definitions.get(&name).cloned())
        .ok_or_else(|| anyhow::anyhow!("account {name} has no type definition"))?;
    Ok(AccountSchema {
        discriminator: discriminator(value, "account", &name)?,
        name,
        ty,
    })
}

fn parse_instruction(value: &Value) -> Result<InstructionSchema> {
    let name = required_string(value, "name", "instruction")?;
    let args = value
        .get("args")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .map(|field| {
            Ok(FieldSchema {
                name: required_string(field, "name", "argument")?,
                ty: field
                    .get("type")
                    .cloned()
                    .ok_or_else(|| anyhow::anyhow!("argument has no type"))?,
            })
        })
        .collect::<Result<Vec<_>>>()?;
    let mut accounts = Vec::new();
    flatten_accounts(
        value
            .get("accounts")
            .and_then(Value::as_array)
            .map(Vec::as_slice)
            .unwrap_or(&[]),
        &mut accounts,
    )?;
    Ok(InstructionSchema {
        discriminator: discriminator(value, "global", &name)?,
        name,
        args,
        accounts,
    })
}

fn flatten_accounts(values: &[Value], output: &mut Vec<InstructionAccountSchema>) -> Result<()> {
    for account in values {
        if let Some(nested) = account.get("accounts").and_then(Value::as_array) {
            flatten_accounts(nested, output)?;
            continue;
        }
        let name = required_string(account, "name", "instruction account")?;
        output.push(InstructionAccountSchema {
            name,
            signer: account
                .get("signer")
                .or_else(|| account.get("isSigner"))
                .and_then(Value::as_bool)
                .unwrap_or(false),
            writable: account
                .get("writable")
                .or_else(|| account.get("isMut"))
                .and_then(Value::as_bool)
                .unwrap_or(false),
            pda: account.get("pda").cloned(),
        });
    }
    Ok(())
}

fn required_string(value: &Value, field: &str, kind: &str) -> Result<String> {
    value
        .get(field)
        .and_then(Value::as_str)
        .map(ToOwned::to_owned)
        .ok_or_else(|| anyhow::anyhow!("{kind} is missing a string {field}"))
}
fn discriminator(value: &Value, namespace: &str, name: &str) -> Result<Vec<u8>> {
    if let Some(items) = value.get("discriminator").and_then(Value::as_array) {
        return items
            .iter()
            .map(|item| {
                item.as_u64()
                    .filter(|number| *number <= 255)
                    .map(|number| number as u8)
                    .ok_or_else(|| anyhow::anyhow!("invalid discriminator for {name}"))
            })
            .collect();
    }
    Ok(Sha256::digest(format!("{namespace}:{name}"))[..8].to_vec())
}

/// Finds an account schema by case-insensitive IDL name.
pub fn find_account<'a>(schema: &'a ProgramSchema, name: &str) -> Result<&'a AccountSchema> {
    schema
        .accounts
        .iter()
        .find(|account| account.name.eq_ignore_ascii_case(name))
        .ok_or_else(|| anyhow::anyhow!("IDL has no account named {name}"))
}
/// Finds an instruction schema by case-insensitive IDL name.
pub fn find_instruction<'a>(
    schema: &'a ProgramSchema,
    name: &str,
) -> Result<&'a InstructionSchema> {
    schema
        .instructions
        .iter()
        .find(|instruction| instruction.name.eq_ignore_ascii_case(name))
        .ok_or_else(|| anyhow::anyhow!("IDL has no instruction named {name}"))
}
