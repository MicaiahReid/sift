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
    let root = if root.get("kind").and_then(Value::as_str) == Some("rootNode") {
        normalize_codama_root(&root)?
    } else {
        root
    };
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
                .or_else(|| account.get("isWritable"))
                .and_then(Value::as_bool)
                .unwrap_or(false),
            pda: account.get("pda").cloned(),
        });
    }
    Ok(())
}

/// Converts the codec-compatible portion of Codama's root document to the
/// compact Anchor-like shape consumed by the rest of Sift.
fn normalize_codama_root(root: &Value) -> Result<Value> {
    let program = root
        .get("program")
        .ok_or_else(|| anyhow::anyhow!("Codama root lacks program"))?;
    let name = program
        .get("name")
        .and_then(Value::as_str)
        .unwrap_or("unnamed");
    let accounts = program.get("accounts").and_then(Value::as_array).into_iter().flatten().map(|account| {
        Ok(serde_json::json!({
            "name": required_string(account, "name", "Codama account")?,
            "type": codama_type(account.get("data").ok_or_else(|| anyhow::anyhow!("Codama account lacks data"))?)?
        }))
    }).collect::<Result<Vec<_>>>()?;
    let instructions = program.get("instructions").and_then(Value::as_array).into_iter().flatten().map(|instruction| {
        let args = instruction.get("arguments").and_then(Value::as_array).into_iter().flatten().map(|argument| {
            Ok(serde_json::json!({"name": required_string(argument, "name", "Codama argument")?, "type": codama_type(argument.get("type").ok_or_else(|| anyhow::anyhow!("Codama argument lacks type"))?)?}))
        }).collect::<Result<Vec<_>>>()?;
        let accounts = instruction.get("accounts").and_then(Value::as_array).into_iter().flatten().map(|account| {
            Ok(serde_json::json!({"name": required_string(account, "name", "Codama instruction account")?, "isSigner": account.get("isSigner").cloned().unwrap_or(Value::Bool(false)), "isWritable": account.get("isWritable").cloned().unwrap_or(Value::Bool(false))}))
        }).collect::<Result<Vec<_>>>()?;
        Ok(serde_json::json!({"name": required_string(instruction, "name", "Codama instruction")?, "args": args, "accounts": accounts}))
    }).collect::<Result<Vec<_>>>()?;
    Ok(serde_json::json!({"name": name, "accounts": accounts, "instructions": instructions}))
}

fn codama_type(value: &Value) -> Result<Value> {
    match value.get("kind").and_then(Value::as_str) {
        Some("structTypeNode") => Ok(
            serde_json::json!({"kind":"struct","fields": value.get("fields").and_then(Value::as_array).into_iter().flatten().map(|field| Ok(serde_json::json!({"name": required_string(field, "name", "Codama field")?, "type": codama_type(field.get("type").ok_or_else(|| anyhow::anyhow!("Codama field lacks type"))?)?}))).collect::<Result<Vec<_>>>()?}),
        ),
        Some("numberTypeNode") => value
            .get("format")
            .cloned()
            .ok_or_else(|| anyhow::anyhow!("Codama number lacks format")),
        Some("booleanTypeNode") => Ok(Value::String("bool".into())),
        Some("publicKeyTypeNode") => Ok(Value::String("publicKey".into())),
        Some("stringTypeNode") => Ok(Value::String("string".into())),
        Some("bytesTypeNode") => Ok(Value::String("bytes".into())),
        Some("definedTypeLinkNode") => Ok(
            serde_json::json!({"defined": required_string(value, "name", "Codama defined type")?}),
        ),
        Some("arrayTypeNode") => Ok(
            serde_json::json!({"array":[codama_type(value.get("item").ok_or_else(|| anyhow::anyhow!("Codama array lacks item"))?)?, value.get("count").cloned().ok_or_else(|| anyhow::anyhow!("Codama array lacks count"))?]}),
        ),
        Some("remainderOptionTypeNode") | Some("optionTypeNode") => Ok(
            serde_json::json!({"option":codama_type(value.get("item").ok_or_else(|| anyhow::anyhow!("Codama option lacks item"))?)?}),
        ),
        Some(other) => bail!("unsupported Codama type node {other}"),
        None => bail!("Codama type lacks kind"),
    }
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

#[cfg(test)]
mod tests {
    use super::parse_idl;

    #[test]
    fn normalizes_a_codec_compatible_codama_root() {
        let input = r#"{"kind":"rootNode","program":{"name":"demo","accounts":[{"name":"state","data":{"kind":"structTypeNode","fields":[{"name":"count","type":{"kind":"numberTypeNode","format":"u64"}}]}}],"instructions":[{"name":"increment","arguments":[{"name":"amount","type":{"kind":"numberTypeNode","format":"u64"}}],"accounts":[{"name":"state","isSigner":false,"isWritable":true}]}]}}"#;
        let schema = parse_idl(input, "11111111111111111111111111111111").unwrap();
        assert_eq!(schema.name, "demo");
        assert_eq!(schema.accounts.len(), 1);
        assert!(schema.instructions[0].accounts[0].writable);
    }
}
