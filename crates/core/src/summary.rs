//! Compact, human- and agent-readable renderings of program schemas.
//!
//! A normalized [`ProgramSchema`] carries full Codama type nodes for every
//! account, instruction, and named type. These helpers reduce that to short
//! signatures such as `Vec<Proposal>` or `{admin: pubkey, bump: u8}` so a
//! caller can learn a program's shape without reading the whole IDL.

use std::collections::{BTreeMap, BTreeSet};

use serde_json::{Map, Value, json};
use sift_types::ProgramSchema;

use crate::nodes::{field_is_omitted, kind};

/// Renders a Codama type node as a short signature string.
///
/// Numbers render as their format (`u64`, `u32be`), containers render as
/// `Vec<T>`, `Option<T>`, `[T; N]`, `Set<T>`, `Map<K, V>` and `(A, B)`, defined
/// types render by name, and inline structs or enums render as
/// `{field: T, ...}` or `A | B(T) | C{x: T}`.
pub fn type_label(ty: &Value) -> String {
    let Some(node_kind) = kind(ty) else {
        return ty.to_string();
    };
    let inner = || ty.get("type").map(type_label).unwrap_or_default();
    match node_kind {
        "numberTypeNode" => {
            let format = ty.get("format").and_then(Value::as_str).unwrap_or("number");
            if ty.get("endian").and_then(Value::as_str) == Some("be") {
                format!("{format}be")
            } else {
                format.to_owned()
            }
        }
        "booleanTypeNode" => "bool".to_owned(),
        "publicKeyTypeNode" => "pubkey".to_owned(),
        "stringTypeNode" => match ty.get("encoding").and_then(Value::as_str) {
            None | Some("utf8") => "string".to_owned(),
            Some(encoding) => format!("string({encoding})"),
        },
        "bytesTypeNode" => "bytes".to_owned(),
        "sizePrefixTypeNode" => inner(),
        "fixedSizeTypeNode" => {
            let size = ty.get("size").map(Value::to_string).unwrap_or_default();
            match ty.get("type").and_then(kind) {
                Some("bytesTypeNode") => format!("[u8; {size}]"),
                _ => format!("{}[{size}]", inner()),
            }
        }
        "arrayTypeNode" => {
            let item = ty.get("item").map(type_label).unwrap_or_default();
            match ty.get("count") {
                Some(count) if kind(count) == Some("fixedCountNode") => format!(
                    "[{item}; {}]",
                    count.get("value").map(Value::to_string).unwrap_or_default()
                ),
                _ => format!("Vec<{item}>"),
            }
        }
        "setTypeNode" => format!(
            "Set<{}>",
            ty.get("item").map(type_label).unwrap_or_default()
        ),
        "mapTypeNode" => format!(
            "Map<{}, {}>",
            ty.get("key").map(type_label).unwrap_or_default(),
            ty.get("value").map(type_label).unwrap_or_default()
        ),
        "optionTypeNode" | "zeroableOptionTypeNode" | "remainderOptionTypeNode" => {
            format!(
                "Option<{}>",
                ty.get("item").map(type_label).unwrap_or_default()
            )
        }
        "tupleTypeNode" => format!(
            "({})",
            ty.get("items")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
                .map(type_label)
                .collect::<Vec<_>>()
                .join(", ")
        ),
        "structTypeNode" => struct_label(ty),
        "enumTypeNode" => enum_label(ty),
        "definedTypeLinkNode" => ty
            .get("name")
            .and_then(Value::as_str)
            .unwrap_or("?")
            .to_owned(),
        "amountTypeNode" | "dateTimeTypeNode" | "solAmountTypeNode" => {
            ty.get("number").map(type_label).unwrap_or_default()
        }
        "hiddenPrefixTypeNode"
        | "hiddenSuffixTypeNode"
        | "preOffsetTypeNode"
        | "postOffsetTypeNode"
        | "sentinelTypeNode" => inner(),
        other => other.trim_end_matches("TypeNode").to_owned(),
    }
}

fn struct_label(ty: &Value) -> String {
    let Some(fields) = ty.get("fields").and_then(Value::as_array) else {
        return "{}".to_owned();
    };
    let rendered = fields
        .iter()
        .filter(|field| !field_is_omitted(field))
        .map(field_label)
        .collect::<Vec<_>>();
    format!("{{{}}}", rendered.join(", "))
}

fn field_label(field: &Value) -> String {
    match (field.get("name").and_then(Value::as_str), field.get("type")) {
        (Some(name), Some(ty)) => format!("{name}: {}", type_label(ty)),
        (None, Some(ty)) => type_label(ty),
        (None, None) => type_label(field),
        (Some(name), None) => name.to_owned(),
    }
}

fn enum_label(ty: &Value) -> String {
    let Some(variants) = ty.get("variants").and_then(Value::as_array) else {
        return "enum".to_owned();
    };
    let rendered = variants
        .iter()
        .map(|variant| {
            let name = variant
                .get("name")
                .and_then(Value::as_str)
                .unwrap_or("?")
                .to_owned();
            match kind(variant) {
                Some("enumTupleVariantTypeNode") => {
                    let items = variant
                        .get("tuple")
                        .and_then(|tuple| tuple.get("items"))
                        .and_then(Value::as_array)
                        .into_iter()
                        .flatten()
                        .map(type_label)
                        .collect::<Vec<_>>()
                        .join(", ");
                    format!("{name}({items})")
                }
                Some("enumStructVariantTypeNode") => format!(
                    "{name}{}",
                    variant.get("struct").map(struct_label).unwrap_or_default()
                ),
                _ => name,
            }
        })
        .collect::<Vec<_>>();
    rendered.join(" | ")
}

/// Lists the caller-visible fields of a struct-like type as `{name, type}` pairs.
///
/// Fields encoded from an omitted default, such as discriminators, are left
/// out. Returns `None` when the type is not a struct with named fields.
pub fn field_summaries(ty: &Value) -> Option<Vec<Value>> {
    let fields = ty.get("fields")?.as_array()?;
    Some(
        fields
            .iter()
            .filter(|field| !field_is_omitted(field))
            .map(|field| {
                json!({
                    "name": field.get("name").cloned().unwrap_or(Value::Null),
                    "type": type_label(field.get("type").unwrap_or(field)),
                })
            })
            .collect(),
    )
}

/// Builds a compact JSON overview of a schema's accounts, instructions, types, and errors.
///
/// Field documentation and discriminators are omitted; use
/// [`definition_with_dependencies`] to read one definition in full.
pub fn schema_summary(schema: &ProgramSchema) -> Value {
    let accounts = schema
        .accounts
        .iter()
        .map(|account| {
            json!({
                "name": account.name,
                "fields": field_summaries(&account.ty).unwrap_or_default(),
            })
        })
        .collect::<Vec<_>>();
    let instructions = schema
        .instructions
        .iter()
        .map(|instruction| {
            json!({
                "name": instruction.name,
                "args": instruction.args.iter().filter(|arg| !arg.omitted).map(|arg| json!({"name": arg.name, "type": type_label(&arg.ty)})).collect::<Vec<_>>(),
                "accounts": instruction.accounts.iter().map(|account| {
                    let mut flags = Vec::new();
                    if account.signer { flags.push("signer"); }
                    if account.writable { flags.push("writable"); }
                    if account.optional { flags.push("optional"); }
                    if account.is_pda() { flags.push("pda"); }
                    else if account.default_value.is_some() { flags.push("default"); }
                    json!({"name": account.name, "flags": flags})
                }).collect::<Vec<_>>(),
            })
        })
        .collect::<Vec<_>>();
    let types = schema
        .types
        .iter()
        .map(|(name, ty)| (name.clone(), Value::String(type_label(ty))))
        .collect::<Map<_, _>>();
    let errors = schema
        .errors
        .iter()
        .map(|error| json!({"code": error.code, "name": error.name, "message": error.message}))
        .collect::<Vec<_>>();
    json!({
        "name": schema.name,
        "format": schema.format,
        "address": schema.address,
        "accounts": accounts,
        "instructions": instructions,
        "types": types,
        "errors": errors,
    })
}

/// Returns a definition's full JSON together with every named type it references.
///
/// `name` is matched case-insensitively against accounts, instructions, and
/// named types, in that order. The result contains the matched definition
/// under `kind`/`definition` plus a `types` map of transitive dependencies.
pub fn definition_with_dependencies(schema: &ProgramSchema, name: &str) -> Option<Value> {
    let (kind, definition) = if let Some(account) = schema
        .accounts
        .iter()
        .find(|account| account.name.eq_ignore_ascii_case(name))
    {
        ("account", serde_json::to_value(account).ok()?)
    } else if let Some(instruction) = schema
        .instructions
        .iter()
        .find(|instruction| instruction.name.eq_ignore_ascii_case(name))
    {
        ("instruction", serde_json::to_value(instruction).ok()?)
    } else if let Some((name, ty)) = schema
        .types
        .iter()
        .find(|(candidate, _)| candidate.eq_ignore_ascii_case(name))
    {
        ("type", json!({"name": name, "ty": ty}))
    } else {
        return None;
    };
    let mut seen = BTreeSet::new();
    collect_defined_names(&definition, &mut seen);
    let mut pending = seen.iter().cloned().collect::<Vec<_>>();
    let mut types = BTreeMap::new();
    while let Some(type_name) = pending.pop() {
        let Some(ty) = schema.types.get(&type_name) else {
            continue;
        };
        let mut nested = BTreeSet::new();
        collect_defined_names(ty, &mut nested);
        for dependency in nested {
            if seen.insert(dependency.clone()) {
                pending.push(dependency);
            }
        }
        types.insert(type_name, ty.clone());
    }
    Some(json!({"kind": kind, "definition": definition, "types": types}))
}

fn collect_defined_names(value: &Value, out: &mut BTreeSet<String>) {
    match value {
        Value::Object(object) => {
            if kind(value) == Some("definedTypeLinkNode")
                && let Some(name) = object.get("name").and_then(Value::as_str)
            {
                out.insert(name.to_owned());
            }
            for child in object.values() {
                collect_defined_names(child, out);
            }
        }
        Value::Array(items) => items
            .iter()
            .for_each(|item| collect_defined_names(item, out)),
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::{definition_with_dependencies, schema_summary, type_label};
    use crate::anchor::convert_type;
    use crate::parse_idl;

    #[test]
    fn renders_containers_defined_types_and_inline_enums() {
        assert_eq!(
            type_label(&convert_type(&json!({"vec": {"option": "u64"}})).unwrap()),
            "Vec<Option<u64>>"
        );
        assert_eq!(
            type_label(&convert_type(&json!({"array": ["u8", 32]})).unwrap()),
            "[u8; 32]"
        );
        assert_eq!(
            type_label(&convert_type(&json!({"defined": {"name": "Proposal"}})).unwrap()),
            "Proposal"
        );
        assert_eq!(
            type_label(
                &convert_type(&json!({"kind": "enum", "variants": [{"name": "A"}, {"name": "B", "fields": ["u8"]}, {"name": "C", "fields": [{"name": "x", "type": "pubkey"}]}]})).unwrap()
            ),
            "A | B(u8) | C{x: pubkey}"
        );
        assert_eq!(
            type_label(
                &json!({"kind": "fixedSizeTypeNode", "size": 32, "type": {"kind": "stringTypeNode", "encoding": "utf8"}})
            ),
            "string[32]"
        );
    }

    #[test]
    fn definition_lookup_collects_transitive_type_dependencies() {
        let idl = json!({
            "name": "demo",
            "accounts": [{"name": "State", "discriminator": [1,2,3,4,5,6,7,8], "type": {"kind": "struct", "fields": [{"name": "status", "type": {"defined": {"name": "Status"}}}]}}],
            "types": [
                {"name": "Status", "type": {"kind": "enum", "variants": [{"name": "Open", "fields": [{"defined": {"name": "Window"}}]}]}},
                {"name": "Window", "type": {"kind": "struct", "fields": [{"name": "start", "type": "i64"}]}},
                {"name": "Unused", "type": {"kind": "struct", "fields": []}}
            ],
            "instructions": []
        });
        let schema = parse_idl(&idl.to_string(), "11111111111111111111111111111111").unwrap();
        let found = definition_with_dependencies(&schema, "state").unwrap();
        assert_eq!(found["kind"], "account");
        let types = found["types"].as_object().unwrap();
        assert!(types.contains_key("Status") && types.contains_key("Window"));
        assert!(!types.contains_key("Unused"));
        let summary = schema_summary(&schema);
        assert_eq!(
            summary["accounts"][0]["fields"],
            json!([{"name": "status", "type": "Status"}])
        );
    }
}
