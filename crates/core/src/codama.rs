//! Loading of Codama IDL documents (`rootNode` / `programNode`).
//!
//! Codama nodes are kept as-is for types; accounts and instructions have their
//! discriminator nodes resolved into concrete byte patterns so the rest of the
//! crate can build RPC filters and match raw data without re-reading the IDL.

use std::collections::BTreeMap;

use anyhow::{Context, Result, bail};
use serde_json::Value;
use sift_types::{
    AccountSchema, Discriminator, ErrorSchema, FieldSchema, InstructionAccountSchema,
    InstructionSchema, OptionalAccountStrategy, ProgramSchema,
};
use tracing::{debug, warn};

use crate::codec::{encode_constant, encode_type};
use crate::nodes::{self, fixed_size_of, kind, shape, struct_fields, value_node_to_json};

/// Converts a parsed Codama document into a schema.
pub fn parse_codama(root: &Value, program_address: &str) -> Result<ProgramSchema> {
    let program = match kind(root) {
        Some("rootNode") => root.get("program").context("Codama root lacks program")?,
        Some("programNode") => root,
        other => bail!("expected a Codama rootNode or programNode, found {other:?}"),
    };
    let name = program
        .get("name")
        .and_then(Value::as_str)
        .unwrap_or("unnamed")
        .to_owned();
    if let Some(declared) = program.get("publicKey").and_then(Value::as_str)
        && declared != program_address
    {
        warn!(
            declared,
            program_address, "Codama IDL declares a different program address than requested"
        );
    }
    let mut types = BTreeMap::new();
    for definition in program
        .get("definedTypes")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        let type_name = required_string(definition, "name", "definedTypeNode")?;
        let ty = definition
            .get("type")
            .cloned()
            .with_context(|| format!("definedTypeNode {type_name} lacks type"))?;
        types.insert(type_name, ty);
    }
    let accounts = program
        .get("accounts")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .map(|account| parse_account(account, &types))
        .collect::<Result<Vec<_>>>()?;
    let instructions = program
        .get("instructions")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .map(|instruction| parse_instruction(instruction, &types))
        .collect::<Result<Vec<_>>>()?;
    let errors = program
        .get("errors")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|error| {
            Some(ErrorSchema {
                code: u32::try_from(error.get("code")?.as_u64()?).ok()?,
                name: error.get("name")?.as_str()?.to_owned(),
                message: error
                    .get("message")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_owned(),
            })
        })
        .collect();
    let pdas = program
        .get("pdas")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    debug!(
        program_address,
        name,
        account_count = accounts.len(),
        instruction_count = instructions.len(),
        type_count = types.len(),
        pda_count = pdas.len(),
        "loaded Codama IDL"
    );
    Ok(ProgramSchema {
        name,
        address: program_address.to_owned(),
        accounts,
        instructions,
        types,
        pdas,
        errors,
    })
}

fn parse_account(value: &Value, types: &BTreeMap<String, Value>) -> Result<AccountSchema> {
    let name = required_string(value, "name", "accountNode")?;
    let ty = value
        .get("data")
        .cloned()
        .with_context(|| format!("accountNode {name} lacks data"))?;
    let mut discriminators = Vec::new();
    for node in value
        .get("discriminators")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        match resolve_discriminator(
            node,
            |field_name| {
                struct_fields(shape(&ty, types))
                    .into_iter()
                    .flatten()
                    .find(|field| field.get("name").and_then(Value::as_str) == Some(field_name))
                    .map(|field| (field.get("type"), field.get("defaultValue")))
            },
            types,
        )
        .with_context(|| format!("account {name}"))?
        {
            Some(discriminator) => discriminators.push(discriminator),
            None => {
                debug!(account = %name, node = %node, "discriminator node has no concrete value")
            }
        }
    }
    if discriminators.is_empty() {
        match infer_discriminator(&name, &ty, types) {
            Some(discriminator) => {
                debug!(account = %name, ?discriminator, "inferred account discriminator from a discriminator enum");
                discriminators.push(discriminator);
            }
            None => {
                debug!(account = %name, "account declares no discriminator; listings decode every program account")
            }
        }
    }
    Ok(AccountSchema {
        name,
        discriminators,
        ty,
    })
}

fn parse_instruction(value: &Value, types: &BTreeMap<String, Value>) -> Result<InstructionSchema> {
    let name = required_string(value, "name", "instructionNode")?;
    let args = value
        .get("arguments")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .map(|argument| {
            let argument_name = required_string(argument, "name", "instructionArgumentNode")?;
            Ok(FieldSchema {
                ty: argument
                    .get("type")
                    .cloned()
                    .with_context(|| format!("argument {argument_name} lacks type"))?,
                default_value: argument.get("defaultValue").cloned(),
                omitted: argument.get("defaultValueStrategy").and_then(Value::as_str)
                    == Some("omitted"),
                name: argument_name,
            })
        })
        .collect::<Result<Vec<_>>>()?;
    let mut discriminators = Vec::new();
    for node in value
        .get("discriminators")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        let resolved = resolve_discriminator(
            node,
            |field_name| {
                value
                    .get("arguments")
                    .and_then(Value::as_array)
                    .into_iter()
                    .flatten()
                    .find(|argument| {
                        argument.get("name").and_then(Value::as_str) == Some(field_name)
                    })
                    .map(|argument| (argument.get("type"), argument.get("defaultValue")))
            },
            types,
        )
        .with_context(|| format!("instruction {name}"))?;
        if let Some(discriminator) = resolved {
            discriminators.push(discriminator);
        }
    }
    let accounts = value
        .get("accounts")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .map(|account| {
            Ok(InstructionAccountSchema {
                name: required_string(account, "name", "instructionAccountNode")?,
                // "either" means the program accepts the account signed or not; a
                // builder cannot require a signature for it.
                signer: account
                    .get("isSigner")
                    .and_then(Value::as_bool)
                    .unwrap_or(false),
                writable: account
                    .get("isWritable")
                    .and_then(Value::as_bool)
                    .unwrap_or(false),
                optional: account
                    .get("isOptional")
                    .and_then(Value::as_bool)
                    .unwrap_or(false),
                default_value: account.get("defaultValue").cloned(),
            })
        })
        .collect::<Result<Vec<_>>>()?;
    let optional_account_strategy =
        match value.get("optionalAccountStrategy").and_then(Value::as_str) {
            Some("omitted") => OptionalAccountStrategy::Omitted,
            _ => OptionalAccountStrategy::ProgramId,
        };
    Ok(InstructionSchema {
        name,
        discriminators,
        args,
        accounts,
        optional_account_strategy,
    })
}

/// Turns one discriminator node into a byte pattern.
///
/// `field` looks up a named struct field or argument, returning its type and
/// default value. Field discriminators without a default value cannot be
/// resolved and yield `None`.
fn resolve_discriminator<'a>(
    node: &Value,
    field: impl Fn(&str) -> Option<(Option<&'a Value>, Option<&'a Value>)>,
    types: &BTreeMap<String, Value>,
) -> Result<Option<Discriminator>> {
    let offset = node
        .get("offset")
        .and_then(Value::as_u64)
        .map_or(0, |offset| offset as usize);
    Ok(match kind(node) {
        Some("constantDiscriminatorNode") => Some(Discriminator::Constant {
            offset,
            bytes: encode_constant(
                node.get("constant")
                    .context("constantDiscriminatorNode lacks constant")?,
                types,
            )?,
            inferred: false,
        }),
        Some("sizeDiscriminatorNode") => Some(Discriminator::Size {
            size: node
                .get("size")
                .and_then(Value::as_u64)
                .context("sizeDiscriminatorNode lacks size")? as usize,
        }),
        Some("fieldDiscriminatorNode") => {
            let field_name = node
                .get("name")
                .and_then(Value::as_str)
                .context("fieldDiscriminatorNode lacks name")?;
            let Some((Some(ty), Some(default))) = field(field_name) else {
                return Ok(None);
            };
            let mut bytes = Vec::new();
            encode_type(ty, &value_node_to_json(default)?, types, &mut bytes)
                .with_context(|| format!("discriminator field {field_name}"))?;
            Some(Discriminator::Constant {
                offset,
                bytes,
                inferred: false,
            })
        }
        other => bail!("unsupported discriminator node {other:?}"),
    })
}

/// Derives a discriminator for an account whose IDL declares none.
///
/// Many hand-written programs store a one-byte account tag as the first field
/// (often inside a shared header) and describe its values with an enum such as
/// `accountDiscriminator { subscriptionAuthority = 0, plan = 1, ... }`, without
/// linking the two in the IDL. This looks for a leading field named like a
/// discriminator at a fixed offset, then for an enum variant named after the
/// account. It returns `None` when either half is missing.
pub fn infer_discriminator(
    account_name: &str,
    ty: &Value,
    types: &BTreeMap<String, Value>,
) -> Option<Discriminator> {
    let (offset, field_ty) = find_discriminator_field(ty, types, 0, 0)?;
    let shaped = shape(field_ty, types);
    let (value, size_node) = match kind(shaped)? {
        "enumTypeNode" => (
            enum_variant_value(shaped, account_name)?,
            shaped
                .get("size")
                .cloned()
                .unwrap_or_else(|| nodes::number("u8")),
        ),
        "numberTypeNode" => (
            types
                .values()
                .map(|candidate| shape(candidate, types))
                .filter(|candidate| kind(candidate) == Some("enumTypeNode"))
                .find_map(|candidate| enum_variant_value(candidate, account_name))?,
            shaped.clone(),
        ),
        _ => return None,
    };
    let mut bytes = Vec::new();
    encode_type(&size_node, &Value::from(value), types, &mut bytes).ok()?;
    Some(Discriminator::Constant {
        offset,
        bytes,
        inferred: true,
    })
}

/// Finds the first fixed-offset field named like an account discriminator.
fn find_discriminator_field<'a>(
    ty: &'a Value,
    types: &'a BTreeMap<String, Value>,
    offset: usize,
    depth: usize,
) -> Option<(usize, &'a Value)> {
    if depth > 8 {
        return None;
    }
    let mut offset = offset;
    for field in struct_fields(shape(ty, types))? {
        let name = field.get("name").and_then(Value::as_str)?;
        let field_ty = field.get("type")?;
        if is_discriminator_name(name) {
            return Some((offset, field_ty));
        }
        if kind(shape(field_ty, types)) == Some("structTypeNode")
            && let Some(found) = find_discriminator_field(field_ty, types, offset, depth + 1)
        {
            return Some(found);
        }
        offset += fixed_size_of(field_ty, types)?;
    }
    None
}

fn is_discriminator_name(name: &str) -> bool {
    matches!(
        normalize_name(name).as_str(),
        "discriminator" | "accountdiscriminator" | "accounttype" | "tag"
    )
}

/// Returns the discriminator value of the variant named after `account_name`.
fn enum_variant_value(enum_node: &Value, account_name: &str) -> Option<i64> {
    let wanted = normalize_name(account_name);
    let mut next = 0_i64;
    for variant in enum_node.get("variants")?.as_array()? {
        let value = variant
            .get("discriminator")
            .and_then(Value::as_i64)
            .unwrap_or(next);
        next = value + 1;
        if kind(variant) == Some("enumEmptyVariantTypeNode")
            && variant
                .get("name")
                .and_then(Value::as_str)
                .is_some_and(|name| normalize_name(name) == wanted)
        {
            return Some(value);
        }
    }
    None
}

fn normalize_name(name: &str) -> String {
    name.chars()
        .filter(|character| character.is_ascii_alphanumeric())
        .map(|character| character.to_ascii_lowercase())
        .collect()
}

fn required_string(value: &Value, field: &str, context: &str) -> Result<String> {
    value
        .get(field)
        .and_then(Value::as_str)
        .map(ToOwned::to_owned)
        .ok_or_else(|| anyhow::anyhow!("{context} is missing a string {field}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::codec::decode_account;
    use crate::nodes::{
        bytes_value, defined_link, enum_node, number, public_key, struct_field, struct_node,
    };
    use serde_json::json;

    const ADDRESS: &str = "De1egAFMkMWZSN5rYXRj9CAdheBamobVNubTsi9avR44";

    /// A trimmed copy of the subscriptions program's on-chain Codama IDL: the
    /// accounts declare no discriminators, but a shared header starts with a
    /// `u8` whose values are listed in a separate enum.
    fn subscriptions_like_idl() -> Value {
        json!({
            "kind": "rootNode", "standard": "codama", "version": "1.0.0",
            "program": {
                "kind": "programNode", "name": "subscriptions", "publicKey": ADDRESS, "version": "0.5.0",
                "accounts": [
                    {"kind": "accountNode", "name": "plan", "data": struct_node(vec![
                        struct_field("discriminator", number("u8")),
                        struct_field("owner", public_key()),
                    ])},
                    {"kind": "accountNode", "name": "fixedDelegation", "data": struct_node(vec![
                        struct_field("header", defined_link("header")),
                        struct_field("mint", public_key()),
                        struct_field("amount", number("u64")),
                    ])}
                ],
                "instructions": [{
                    "kind": "instructionNode", "name": "createPlan",
                    "accounts": [
                        {"kind": "instructionAccountNode", "name": "payer", "isWritable": true, "isSigner": true, "isOptional": false},
                        {"kind": "instructionAccountNode", "name": "systemProgram", "isWritable": false, "isSigner": false, "isOptional": false,
                         "defaultValue": {"kind": "publicKeyValueNode", "publicKey": "11111111111111111111111111111111"}}
                    ],
                    "arguments": [
                        {"kind": "instructionArgumentNode", "name": "discriminator", "type": number("u8"),
                         "defaultValue": {"kind": "numberValueNode", "number": 3}, "defaultValueStrategy": "omitted"},
                        {"kind": "instructionArgumentNode", "name": "amount", "type": number("u64")}
                    ],
                    "discriminators": [{"kind": "fieldDiscriminatorNode", "name": "discriminator", "offset": 0}]
                }],
                "definedTypes": [
                    {"kind": "definedTypeNode", "name": "accountDiscriminator", "type": enum_node(vec![
                        json!({"kind": "enumEmptyVariantTypeNode", "name": "subscriptionAuthority", "discriminator": 0}),
                        json!({"kind": "enumEmptyVariantTypeNode", "name": "plan", "discriminator": 1}),
                        json!({"kind": "enumEmptyVariantTypeNode", "name": "fixedDelegation", "discriminator": 2}),
                    ], number("u8"))},
                    {"kind": "definedTypeNode", "name": "header", "type": struct_node(vec![
                        struct_field("discriminator", number("u8")),
                        struct_field("version", number("u8")),
                        struct_field("delegator", public_key()),
                    ])}
                ],
                "pdas": [], "errors": [{"kind": "errorNode", "name": "expired", "code": 6000, "message": "The delegation expired"}]
            }
        })
    }

    #[test]
    fn infers_single_byte_discriminators_from_a_discriminator_enum() {
        let schema = parse_codama(&subscriptions_like_idl(), ADDRESS).unwrap();
        assert_eq!(
            schema.accounts[0].discriminators,
            vec![Discriminator::Constant {
                offset: 0,
                bytes: vec![1],
                inferred: true
            }]
        );
        assert_eq!(
            schema.accounts[1].discriminators,
            vec![Discriminator::Constant {
                offset: 0,
                bytes: vec![2],
                inferred: true
            }]
        );
        assert_eq!(schema.types.len(), 2);
        assert_eq!(schema.errors[0].code, 6000);

        let raw = [
            vec![2, 1],
            vec![7; 32],
            vec![9; 32],
            5u64.to_le_bytes().to_vec(),
        ]
        .concat();
        let decoded = decode_account(&schema.accounts[1], &schema.types, &raw).unwrap();
        assert_eq!(decoded["header"]["discriminator"], 2);
        assert_eq!(
            decoded["header"]["delegator"],
            bs58::encode([7; 32]).into_string()
        );
        assert_eq!(decoded["amount"], "5");
        assert!(decode_account(&schema.accounts[0], &schema.types, &raw).is_err());
    }

    #[test]
    fn resolves_field_discriminators_from_argument_defaults() {
        let schema = parse_codama(&subscriptions_like_idl(), ADDRESS).unwrap();
        let instruction = &schema.instructions[0];
        assert_eq!(
            instruction.discriminators,
            vec![Discriminator::Constant {
                offset: 0,
                bytes: vec![3],
                inferred: false
            }]
        );
        assert!(instruction.args[0].omitted);
        assert_eq!(
            instruction.accounts[1].default_value.as_ref().unwrap()["publicKey"],
            "11111111111111111111111111111111"
        );
    }

    #[test]
    fn resolves_constant_and_anchor_style_field_discriminators() {
        let mut field = struct_field("discriminator", nodes::fixed_size(nodes::bytes(), 8));
        field["defaultValue"] = bytes_value(&[9; 8]);
        field["defaultValueStrategy"] = json!("omitted");
        let idl = json!({"kind": "programNode", "name": "demo", "publicKey": ADDRESS,
            "accounts": [
                {"kind": "accountNode", "name": "state", "data": struct_node(vec![field, struct_field("count", number("u32"))]),
                 "discriminators": [{"kind": "fieldDiscriminatorNode", "name": "discriminator", "offset": 0}]},
                {"kind": "accountNode", "name": "tagged", "data": struct_node(vec![struct_field("count", number("u32"))]),
                 "discriminators": [
                    {"kind": "constantDiscriminatorNode", "offset": 4, "constant": nodes::constant_value(number("u16"), json!({"kind": "numberValueNode", "number": 513}))},
                    {"kind": "sizeDiscriminatorNode", "size": 6}
                 ]}
            ],
            "instructions": [], "definedTypes": []});
        let schema = parse_codama(&idl, ADDRESS).unwrap();
        assert_eq!(
            schema.accounts[0].discriminators,
            vec![Discriminator::Constant {
                offset: 0,
                bytes: vec![9; 8],
                inferred: false
            }]
        );
        assert_eq!(
            schema.accounts[1].discriminators,
            vec![
                Discriminator::Constant {
                    offset: 4,
                    bytes: vec![1, 2],
                    inferred: false
                },
                Discriminator::Size { size: 6 }
            ]
        );
        let decoded = decode_account(
            &schema.accounts[0],
            &schema.types,
            &[vec![9; 8], vec![1, 0, 0, 0]].concat(),
        )
        .unwrap();
        assert_eq!(decoded, json!({"count": 1}));
    }
}
