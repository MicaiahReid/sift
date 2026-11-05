//! Conversion of Anchor IDLs (legacy and 0.30+) into Codama-shaped schemas.
//!
//! The conversion follows the same rules as Codama's `nodes-from-anchor`:
//! account and instruction discriminators become leading `discriminator`
//! fields with omitted default values, Anchor type expressions become Codama
//! type nodes, and PDA recipes become `pdaValueNode` defaults.

use anyhow::{Context, Result, bail};
use serde_json::{Map, Value, json};
use sha2::{Digest, Sha256};
use sift_types::{
    AccountSchema, Discriminator, ErrorSchema, FieldSchema, InstructionAccountSchema,
    InstructionSchema, OptionalAccountStrategy, ProgramSchema,
};
use tracing::debug;

use crate::nodes::{self, kind};

/// Converts a parsed Anchor IDL document into a Codama-shaped schema.
pub fn parse_anchor(root: &Value, program_address: &str) -> Result<ProgramSchema> {
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
        let type_name = required_string(definition, "name", "type definition")?;
        let ty = convert_type(
            definition
                .get("type")
                .with_context(|| format!("type {type_name} has no type expression"))?,
        )
        .with_context(|| format!("type {type_name}"))?;
        types.insert(type_name, ty);
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
    let errors = root
        .get("errors")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|error| {
            Some(ErrorSchema {
                code: u32::try_from(error.get("code")?.as_u64()?).ok()?,
                name: error.get("name")?.as_str()?.to_owned(),
                message: error
                    .get("msg")
                    .or_else(|| error.get("message"))
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_owned(),
            })
        })
        .collect();
    if accounts.is_empty() && instructions.is_empty() {
        bail!("unsupported IDL: expected an Anchor accounts or instructions array")
    }
    debug!(
        program_address,
        name,
        account_count = accounts.len(),
        instruction_count = instructions.len(),
        type_count = types.len(),
        "converted Anchor IDL"
    );
    Ok(ProgramSchema {
        name,
        address: program_address.to_owned(),
        accounts,
        instructions,
        types,
        pdas: Vec::new(),
        errors,
    })
}

fn parse_account(
    value: &Value,
    definitions: &std::collections::BTreeMap<String, Value>,
) -> Result<AccountSchema> {
    let name = required_string(value, "name", "account")?;
    let body = match value.get("type") {
        Some(ty) => convert_type(ty).with_context(|| format!("account {name}"))?,
        None => definitions
            .get(&name)
            .cloned()
            .ok_or_else(|| anyhow::anyhow!("account {name} has no type definition"))?,
    };
    let discriminator = discriminator_bytes(value, "account", &name)?;
    Ok(AccountSchema {
        discriminators: vec![Discriminator::Constant {
            offset: 0,
            bytes: discriminator.clone(),
            inferred: false,
        }],
        ty: with_leading_discriminator(body, &discriminator),
        name,
    })
}

/// Prepends an omitted `discriminator` field holding `bytes` to a layout.
fn with_leading_discriminator(body: Value, bytes: &[u8]) -> Value {
    let field = discriminator_field(bytes);
    if kind(&body) == Some("structTypeNode") {
        let mut fields = vec![field];
        fields.extend(
            body.get("fields")
                .and_then(Value::as_array)
                .cloned()
                .unwrap_or_default(),
        );
        return nodes::struct_node(fields);
    }
    nodes::struct_node(vec![field, nodes::struct_field("value", body)])
}

fn discriminator_field(bytes: &[u8]) -> Value {
    let mut field = nodes::struct_field(
        "discriminator",
        nodes::fixed_size(nodes::bytes(), bytes.len()),
    );
    field["defaultValue"] = nodes::bytes_value(bytes);
    field["defaultValueStrategy"] = json!("omitted");
    field
}

fn parse_instruction(value: &Value) -> Result<InstructionSchema> {
    let name = required_string(value, "name", "instruction")?;
    let discriminator = discriminator_bytes(value, "global", &to_snake_case(&name))?;
    let mut args = vec![FieldSchema {
        name: "discriminator".to_owned(),
        ty: nodes::fixed_size(nodes::bytes(), discriminator.len()),
        default_value: Some(nodes::bytes_value(&discriminator)),
        omitted: true,
    }];
    for field in value
        .get("args")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        let field_name = required_string(field, "name", "argument")?;
        args.push(FieldSchema {
            ty: convert_type(
                field
                    .get("type")
                    .with_context(|| format!("argument {field_name} has no type"))?,
            )
            .with_context(|| format!("instruction {name} argument {field_name}"))?,
            name: field_name,
            default_value: None,
            omitted: false,
        });
    }
    let mut accounts = Vec::new();
    flatten_accounts(
        value
            .get("accounts")
            .and_then(Value::as_array)
            .map(Vec::as_slice)
            .unwrap_or(&[]),
        &args,
        &mut accounts,
    )?;
    Ok(InstructionSchema {
        discriminators: vec![Discriminator::Constant {
            offset: 0,
            bytes: discriminator,
            inferred: false,
        }],
        name,
        args,
        accounts,
        optional_account_strategy: OptionalAccountStrategy::ProgramId,
    })
}

fn flatten_accounts(
    values: &[Value],
    args: &[FieldSchema],
    output: &mut Vec<InstructionAccountSchema>,
) -> Result<()> {
    for account in values {
        if let Some(nested) = account.get("accounts").and_then(Value::as_array) {
            flatten_accounts(nested, args, output)?;
            continue;
        }
        let name = required_string(account, "name", "instruction account")?;
        let default_value = if let Some(address) = account.get("address").and_then(Value::as_str) {
            Some(json!({"kind": "publicKeyValueNode", "publicKey": address}))
        } else if let Some(pda) = account.get("pda") {
            Some(convert_pda(&name, pda, args)?)
        } else {
            None
        };
        output.push(InstructionAccountSchema {
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
            optional: account
                .get("optional")
                .or_else(|| account.get("isOptional"))
                .and_then(Value::as_bool)
                .unwrap_or(false),
            default_value,
            name,
        });
    }
    Ok(())
}

/// Converts an Anchor PDA recipe into a `pdaValueNode` with an inline `pdaNode`.
fn convert_pda(account_name: &str, pda: &Value, args: &[FieldSchema]) -> Result<Value> {
    let mut seed_nodes = Vec::new();
    let mut seed_values = Vec::new();
    for seed in pda
        .get("seeds")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        match seed.get("kind").and_then(Value::as_str) {
            Some("const") => {
                let bytes = seed
                    .get("value")
                    .and_then(Value::as_array)
                    .context("const seed lacks value")?
                    .iter()
                    .map(|byte| {
                        byte.as_u64()
                            .filter(|byte| *byte <= 255)
                            .map(|byte| byte as u8)
                            .context("const seed byte is invalid")
                    })
                    .collect::<Result<Vec<_>>>()?;
                seed_nodes.push(json!({
                    "kind": "constantPdaSeedNode",
                    "type": nodes::fixed_size(nodes::bytes(), bytes.len()),
                    "value": nodes::bytes_value(&bytes),
                }));
            }
            Some(seed_kind @ ("account" | "arg")) => {
                let path = seed
                    .get("path")
                    .and_then(Value::as_str)
                    .context("seed lacks path")?;
                let seed_name = path.rsplit('.').next().unwrap_or(path).to_owned();
                let ty = if seed_kind == "arg" {
                    args.iter()
                        .find(|arg| arg.name == path)
                        .map(|arg| arg.ty.clone())
                        .unwrap_or_else(nodes::bytes)
                } else {
                    nodes::public_key()
                };
                seed_nodes
                    .push(json!({"kind": "variablePdaSeedNode", "name": seed_name, "type": ty}));
                let value_kind = if seed_kind == "arg" {
                    "argumentValueNode"
                } else {
                    "accountValueNode"
                };
                seed_values.push(json!({
                    "kind": "pdaSeedValueNode",
                    "name": seed_name,
                    "value": {"kind": value_kind, "name": path},
                }));
            }
            other => bail!("unsupported PDA seed kind {other:?}"),
        }
    }
    let mut pda_node = Map::new();
    pda_node.insert("kind".to_owned(), json!("pdaNode"));
    pda_node.insert("name".to_owned(), json!(account_name));
    pda_node.insert("seeds".to_owned(), Value::Array(seed_nodes));
    if let Some(program) = pda.get("program") {
        pda_node.insert("programId".to_owned(), program.clone());
    }
    Ok(json!({"kind": "pdaValueNode", "pda": Value::Object(pda_node), "seeds": seed_values}))
}

/// Reads an explicit discriminator, or derives Anchor's `sha256("{namespace}:{name}")[..8]`.
fn discriminator_bytes(value: &Value, namespace: &str, name: &str) -> Result<Vec<u8>> {
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

/// Converts an Anchor type expression into a Codama type node.
pub fn convert_type(ty: &Value) -> Result<Value> {
    if let Some(name) = ty.as_str() {
        return Ok(match name {
            "bool" => nodes::boolean(),
            "u8" | "u16" | "u32" | "u64" | "u128" | "u256" | "i8" | "i16" | "i32" | "i64"
            | "i128" | "i256" | "f32" | "f64" | "shortU16" => nodes::number(name),
            "string" => nodes::size_prefix(nodes::string("utf8"), nodes::number("u32")),
            "bytes" => nodes::size_prefix(nodes::bytes(), nodes::number("u32")),
            "publicKey" | "pubkey" => nodes::public_key(),
            other => nodes::defined_link(other),
        });
    }
    let object = ty
        .as_object()
        .ok_or_else(|| anyhow::anyhow!("unsupported type expression {ty}"))?;
    if let Some(inner) = object.get("vec") {
        return Ok(nodes::array(
            convert_type(inner)?,
            nodes::prefixed_count(nodes::number("u32")),
        ));
    }
    if let Some(inner) = object.get("option") {
        return Ok(nodes::option(convert_type(inner)?));
    }
    if let Some(inner) = object.get("coption") {
        return Ok(json!({
            "kind": "optionTypeNode",
            "item": convert_type(inner)?,
            "prefix": nodes::number("u32"),
            "fixed": true,
        }));
    }
    if let Some(array) = object.get("array").and_then(Value::as_array) {
        let [inner, length] = array.as_slice() else {
            bail!("array type must have [type, length]")
        };
        let length = length.as_u64().ok_or_else(|| {
            anyhow::anyhow!("array length must be an integer; generics are not supported")
        })?;
        return Ok(nodes::array(
            convert_type(inner)?,
            nodes::fixed_count(length),
        ));
    }
    if let Some(defined) = object.get("defined") {
        let name = defined
            .as_str()
            .or_else(|| defined.get("name").and_then(Value::as_str))
            .ok_or_else(|| anyhow::anyhow!("defined type lacks a name"))?;
        return Ok(nodes::defined_link(name));
    }
    if let Some(map) = object.get("hashMap").or_else(|| object.get("bTreeMap")) {
        let [key, value] = map
            .as_array()
            .map(Vec::as_slice)
            .ok_or_else(|| anyhow::anyhow!("map type must have [key, value]"))?
        else {
            bail!("map type must have [key, value]")
        };
        return Ok(json!({
            "kind": "mapTypeNode",
            "key": convert_type(key)?,
            "value": convert_type(value)?,
            "count": nodes::prefixed_count(nodes::number("u32")),
        }));
    }
    if let Some(inner) = object.get("hashSet").or_else(|| object.get("bTreeSet")) {
        return Ok(json!({
            "kind": "setTypeNode",
            "item": convert_type(inner)?,
            "count": nodes::prefixed_count(nodes::number("u32")),
        }));
    }
    if object.contains_key("generic") {
        bail!("generic type parameters are not supported")
    }
    match object.get("kind").and_then(Value::as_str) {
        Some("struct") => convert_struct(object),
        Some("enum") => convert_enum(object),
        Some("type") => convert_type(
            object
                .get("alias")
                .ok_or_else(|| anyhow::anyhow!("type alias lacks alias"))?,
        ),
        _ => bail!("unsupported type expression {ty}"),
    }
}

/// Converts Anchor struct fields, producing a tuple node for unnamed fields.
fn convert_struct(object: &Map<String, Value>) -> Result<Value> {
    let fields = object
        .get("fields")
        .and_then(Value::as_array)
        .map(Vec::as_slice)
        .unwrap_or(&[]);
    let named = fields.iter().all(|field| field.get("name").is_some());
    if !named && !fields.is_empty() {
        return Ok(nodes::tuple(
            fields
                .iter()
                .map(convert_type)
                .collect::<Result<Vec<_>>>()?,
        ));
    }
    Ok(nodes::struct_node(
        fields
            .iter()
            .map(|field| {
                let name = required_string(field, "name", "struct field")?;
                let ty = convert_type(
                    field
                        .get("type")
                        .with_context(|| format!("field {name} lacks type"))?,
                )
                .with_context(|| format!("field {name}"))?;
                let mut node = nodes::struct_field(&name, ty);
                if let Some(docs) = field
                    .get("docs")
                    .filter(|docs| docs.as_array().is_some_and(|docs| !docs.is_empty()))
                {
                    node["docs"] = docs.clone();
                }
                Ok(node)
            })
            .collect::<Result<Vec<_>>>()?,
    ))
}

fn convert_enum(object: &Map<String, Value>) -> Result<Value> {
    let variants = object
        .get("variants")
        .and_then(Value::as_array)
        .ok_or_else(|| anyhow::anyhow!("enum lacks variants"))?
        .iter()
        .map(|variant| {
            let name = required_string(variant, "name", "enum variant")?;
            let Some(fields) = variant.get("fields").and_then(Value::as_array) else {
                return Ok(json!({"kind": "enumEmptyVariantTypeNode", "name": name}));
            };
            if fields.is_empty() {
                return Ok(json!({"kind": "enumEmptyVariantTypeNode", "name": name}));
            }
            let mut spec = Map::new();
            spec.insert("fields".to_owned(), Value::Array(fields.clone()));
            let body = convert_struct(&spec).with_context(|| format!("variant {name}"))?;
            Ok(if kind(&body) == Some("tupleTypeNode") {
                json!({"kind": "enumTupleVariantTypeNode", "name": name, "tuple": body})
            } else {
                json!({"kind": "enumStructVariantTypeNode", "name": name, "struct": body})
            })
        })
        .collect::<Result<Vec<_>>>()?;
    Ok(nodes::enum_node(variants, nodes::number("u8")))
}

fn required_string(value: &Value, field: &str, context: &str) -> Result<String> {
    value
        .get(field)
        .and_then(Value::as_str)
        .map(ToOwned::to_owned)
        .ok_or_else(|| anyhow::anyhow!("{context} is missing a string {field}"))
}

/// Converts `camelCase` or `PascalCase` to `snake_case`; snake case passes through.
pub fn to_snake_case(name: &str) -> String {
    let mut out = String::with_capacity(name.len() + 4);
    let chars = name.chars().collect::<Vec<_>>();
    for (index, character) in chars.iter().enumerate() {
        if character.is_ascii_uppercase() {
            let previous_lower = index > 0 && chars[index - 1].is_ascii_lowercase();
            let previous_digit = index > 0 && chars[index - 1].is_ascii_digit();
            let next_lower = chars
                .get(index + 1)
                .is_some_and(|next| next.is_ascii_lowercase());
            let previous_upper = index > 0 && chars[index - 1].is_ascii_uppercase();
            if index > 0 && (previous_lower || previous_digit || (previous_upper && next_lower)) {
                out.push('_');
            }
            out.push(character.to_ascii_lowercase());
        } else {
            out.push(*character);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::codec::{decode_account, encode_account};

    #[test]
    fn derives_legacy_instruction_sighash_from_snake_case() {
        assert_eq!(to_snake_case("initializeMint"), "initialize_mint");
        assert_eq!(to_snake_case("initialize_mint"), "initialize_mint");
        assert_eq!(to_snake_case("setV2Config"), "set_v2_config");
        let idl = json!({"name": "demo", "accounts": [], "instructions": [{"name": "initializeMint", "accounts": [], "args": []}]});
        let schema = parse_anchor(&idl, "11111111111111111111111111111111").unwrap();
        let expected = Sha256::digest("global:initialize_mint")[..8].to_vec();
        assert_eq!(
            schema.instructions[0].discriminators,
            vec![Discriminator::Constant {
                offset: 0,
                bytes: expected.clone(),
                inferred: false
            }]
        );
        assert_eq!(
            schema.instructions[0].args[0].default_value,
            Some(nodes::bytes_value(&expected))
        );
        assert!(schema.instructions[0].args[0].omitted);
    }

    #[test]
    fn converts_anchor_types_and_round_trips_values() {
        let idl = json!({
            "name": "demo",
            "accounts": [{"name": "State", "discriminator": [1,2,3,4,5,6,7,8]}],
            "types": [
                {"name": "State", "type": {"kind": "struct", "fields": [
                    {"name": "authority", "type": "pubkey"},
                    {"name": "names", "type": {"vec": "string"}},
                    {"name": "window", "type": {"option": {"defined": {"name": "Window"}}}},
                    {"name": "status", "type": {"defined": "Status"}},
                    {"name": "raw", "type": {"array": ["u8", 4]}}
                ]}},
                {"name": "Window", "type": {"kind": "struct", "fields": [{"name": "start", "type": "i64"}]}},
                {"name": "Status", "type": {"kind": "enum", "variants": [
                    {"name": "Open"},
                    {"name": "Closed", "fields": [{"name": "at", "type": "i64"}]},
                    {"name": "Pair", "fields": ["u8", "u8"]}
                ]}}
            ],
            "instructions": []
        });
        let schema = parse_anchor(&idl, "11111111111111111111111111111111").unwrap();
        let account = &schema.accounts[0];
        let value = json!({
            "authority": "11111111111111111111111111111111",
            "names": ["a", "bc"],
            "window": {"start": "-3"},
            "status": {"kind": "Pair", "fields": [1, 2]},
            "raw": [9, 8, 7, 6],
        });
        let bytes = encode_account(account, &schema.types, &value).unwrap();
        assert_eq!(&bytes[..8], &[1, 2, 3, 4, 5, 6, 7, 8]);
        assert_eq!(
            decode_account(account, &schema.types, &bytes).unwrap(),
            value
        );
    }

    #[test]
    fn converts_pda_recipes_and_fixed_addresses() {
        let idl = json!({"name": "demo", "accounts": [], "instructions": [{
            "name": "ping",
            "discriminator": [1,1,1,1,1,1,1,1],
            "accounts": [
                {"name": "vault", "writable": true, "pda": {"seeds": [
                    {"kind": "const", "value": [118, 97, 117, 108, 116]},
                    {"kind": "account", "path": "owner"},
                    {"kind": "arg", "path": "id"}
                ]}},
                {"name": "owner", "signer": true},
                {"name": "systemProgram", "address": "11111111111111111111111111111111"}
            ],
            "args": [{"name": "id", "type": "u64"}]
        }]});
        let schema = parse_anchor(&idl, "11111111111111111111111111111111").unwrap();
        let accounts = &schema.instructions[0].accounts;
        assert!(accounts[0].is_pda());
        assert_eq!(
            accounts[0].default_value.as_ref().unwrap()["pda"]["seeds"][2]["type"],
            nodes::number("u64")
        );
        assert_eq!(
            accounts[2].default_value.as_ref().unwrap()["kind"],
            "publicKeyValueNode"
        );
    }
}
