//! Compact, human- and agent-readable renderings of program schemas.
//!
//! A normalized [`ProgramSchema`] carries full type JSON for every account,
//! instruction, and named type. These helpers reduce that to short signatures
//! such as `Vec<Proposal>` or `{admin: pubkey, bump: u8}` so a caller can learn
//! a program's shape without reading the whole IDL.

use std::collections::{BTreeMap, BTreeSet};

use serde_json::{Map, Value, json};
use sift_types::ProgramSchema;

/// Renders an IDL type expression as a short signature string.
///
/// Primitive names pass through unchanged, containers render as `Vec<T>`,
/// `Option<T>`, `[T; N]`, and `HashMap<K, V>`, defined types render by name,
/// and inline structs or enums render as `{field: T, ...}` or `A | B(T) | C{x: T}`.
pub fn type_label(ty: &Value) -> String {
    match ty {
        Value::String(name) => name.clone(),
        Value::Object(object) => object_label(object),
        other => other.to_string(),
    }
}

fn object_label(object: &Map<String, Value>) -> String {
    if let Some(name) = object
        .get("defined")
        .and_then(|defined| defined.as_str().or_else(|| defined.get("name")?.as_str()))
    {
        return name.to_owned();
    }
    if let Some(inner) = object.get("vec") {
        return format!("Vec<{}>", type_label(inner));
    }
    if let Some(inner) = object.get("option") {
        return format!("Option<{}>", type_label(inner));
    }
    if let Some(inner) = object.get("coption") {
        return format!("COption<{}>", type_label(inner));
    }
    if let Some(array) = object.get("array").and_then(Value::as_array)
        && let [inner, length] = array.as_slice()
    {
        return format!("[{}; {length}]", type_label(inner));
    }
    if let Some(map) = object.get("hashMap").or_else(|| object.get("bTreeMap"))
        && let Some([key, value]) = map.as_array().map(Vec::as_slice)
    {
        return format!("HashMap<{}, {}>", type_label(key), type_label(value));
    }
    match object.get("kind").and_then(Value::as_str) {
        Some("struct") => struct_label(object),
        Some("enum") => enum_label(object),
        Some("type") => object.get("alias").map(type_label).unwrap_or_default(),
        _ => Value::Object(object.clone()).to_string(),
    }
}

fn struct_label(object: &Map<String, Value>) -> String {
    let Some(fields) = object.get("fields").and_then(Value::as_array) else {
        return "{}".to_owned();
    };
    let rendered = fields.iter().map(field_label).collect::<Vec<_>>();
    format!("{{{}}}", rendered.join(", "))
}

fn field_label(field: &Value) -> String {
    match (field.get("name").and_then(Value::as_str), field.get("type")) {
        (Some(name), Some(ty)) => format!("{name}: {}", type_label(ty)),
        // Tuple struct and tuple variant fields are bare type expressions.
        (None, _) => type_label(field),
        (Some(name), None) => name.to_owned(),
    }
}

fn enum_label(object: &Map<String, Value>) -> String {
    let Some(variants) = object.get("variants").and_then(Value::as_array) else {
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
            let Some(fields) = variant.get("fields").and_then(Value::as_array) else {
                return name;
            };
            let named = fields.iter().any(|field| field.get("name").is_some());
            let inner = fields
                .iter()
                .map(field_label)
                .collect::<Vec<_>>()
                .join(", ");
            if named {
                format!("{name}{{{inner}}}")
            } else {
                format!("{name}({inner})")
            }
        })
        .collect::<Vec<_>>();
    rendered.join(" | ")
}

/// Lists the fields of a struct-like type as `{name, type}` pairs.
///
/// Returns `None` when the type is not a struct with named fields.
pub fn field_summaries(ty: &Value) -> Option<Vec<Value>> {
    let fields = ty.get("fields")?.as_array()?;
    Some(
        fields
            .iter()
            .map(|field| {
                json!({
                    "name": field.get("name").cloned().unwrap_or(Value::Null),
                    "type": type_label(field.get("type").unwrap_or(field)),
                })
            })
            .collect(),
    )
}

/// Builds a compact JSON overview of a schema's accounts, instructions, and types.
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
                "args": instruction.args.iter().map(|arg| json!({"name": arg.name, "type": type_label(&arg.ty)})).collect::<Vec<_>>(),
                "accounts": instruction.accounts.iter().map(|account| {
                    let mut flags = Vec::new();
                    if account.signer { flags.push("signer"); }
                    if account.writable { flags.push("writable"); }
                    if account.pda.is_some() { flags.push("pda"); }
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
    json!({
        "name": schema.name,
        "address": schema.address,
        "accounts": accounts,
        "instructions": instructions,
        "types": types,
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
            if let Some(name) = object
                .get("defined")
                .and_then(|defined| defined.as_str().or_else(|| defined.get("name")?.as_str()))
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

    use super::{definition_with_dependencies, type_label};
    use crate::parse_idl;

    #[test]
    fn renders_containers_defined_types_and_inline_enums() {
        assert_eq!(
            type_label(&json!({"vec": {"option": "u64"}})),
            "Vec<Option<u64>>"
        );
        assert_eq!(type_label(&json!({"array": ["u8", 32]})), "[u8; 32]");
        assert_eq!(
            type_label(&json!({"defined": {"name": "Proposal"}})),
            "Proposal"
        );
        assert_eq!(
            type_label(
                &json!({"kind": "enum", "variants": [{"name": "A"}, {"name": "B", "fields": ["u8"]}, {"name": "C", "fields": [{"name": "x", "type": "pubkey"}]}]})
            ),
            "A | B(u8) | C{x: pubkey}"
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
    }
}
