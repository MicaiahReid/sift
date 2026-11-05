//! Construction and inspection helpers for Codama type and value nodes.
//!
//! Sift stores every layout as Codama JSON nodes. These helpers build the
//! common nodes, follow `definedTypeLinkNode`s, compute fixed byte sizes, and
//! turn Codama value nodes into the plain JSON values the codec encodes.

use std::collections::BTreeMap;

use anyhow::{Context, Result, bail};
use base64::Engine;
use serde_json::{Map, Value, json};

/// Returns a node's `kind`, or `None` for non-object values.
pub fn kind(node: &Value) -> Option<&str> {
    node.get("kind").and_then(Value::as_str)
}

/// Builds a little-endian `numberTypeNode`.
pub fn number(format: &str) -> Value {
    json!({"kind": "numberTypeNode", "format": format, "endian": "le"})
}

/// Builds a `booleanTypeNode` stored in one byte.
pub fn boolean() -> Value {
    json!({"kind": "booleanTypeNode", "size": number("u8")})
}

/// Builds a `publicKeyTypeNode`.
pub fn public_key() -> Value {
    json!({"kind": "publicKeyTypeNode"})
}

/// Builds a `stringTypeNode` with the given encoding.
pub fn string(encoding: &str) -> Value {
    json!({"kind": "stringTypeNode", "encoding": encoding})
}

/// Builds an unsized `bytesTypeNode`.
pub fn bytes() -> Value {
    json!({"kind": "bytesTypeNode"})
}

/// Wraps a node in a `sizePrefixTypeNode` using `prefix` as the length encoding.
pub fn size_prefix(inner: Value, prefix: Value) -> Value {
    json!({"kind": "sizePrefixTypeNode", "type": inner, "prefix": prefix})
}

/// Wraps a node in a `fixedSizeTypeNode` of `size` bytes.
pub fn fixed_size(inner: Value, size: usize) -> Value {
    json!({"kind": "fixedSizeTypeNode", "type": inner, "size": size})
}

/// Builds an `arrayTypeNode` with the given count node.
pub fn array(item: Value, count: Value) -> Value {
    json!({"kind": "arrayTypeNode", "item": item, "count": count})
}

/// Builds a `fixedCountNode`.
pub fn fixed_count(value: u64) -> Value {
    json!({"kind": "fixedCountNode", "value": value})
}

/// Builds a `prefixedCountNode` using `prefix` as the length encoding.
pub fn prefixed_count(prefix: Value) -> Value {
    json!({"kind": "prefixedCountNode", "prefix": prefix})
}

/// Builds a `remainderCountNode`.
pub fn remainder_count() -> Value {
    json!({"kind": "remainderCountNode"})
}

/// Builds an `optionTypeNode` with a one-byte prefix.
pub fn option(item: Value) -> Value {
    json!({"kind": "optionTypeNode", "item": item, "prefix": number("u8"), "fixed": false})
}

/// Builds a `structTypeNode` from `structFieldTypeNode`s.
pub fn struct_node(fields: Vec<Value>) -> Value {
    json!({"kind": "structTypeNode", "fields": fields})
}

/// Builds a `structFieldTypeNode`.
pub fn struct_field(name: &str, ty: Value) -> Value {
    json!({"kind": "structFieldTypeNode", "name": name, "type": ty})
}

/// Builds a `tupleTypeNode`.
pub fn tuple(items: Vec<Value>) -> Value {
    json!({"kind": "tupleTypeNode", "items": items})
}

/// Builds an `enumTypeNode` stored with the given size node.
pub fn enum_node(variants: Vec<Value>, size: Value) -> Value {
    json!({"kind": "enumTypeNode", "variants": variants, "size": size})
}

/// Builds a `definedTypeLinkNode`.
pub fn defined_link(name: &str) -> Value {
    json!({"kind": "definedTypeLinkNode", "name": name})
}

/// Builds a `bytesValueNode` from raw bytes.
pub fn bytes_value(data: &[u8]) -> Value {
    json!({"kind": "bytesValueNode", "data": hex::encode(data), "encoding": "base16"})
}

/// Builds a `constantValueNode`.
pub fn constant_value(ty: Value, value: Value) -> Value {
    json!({"kind": "constantValueNode", "type": ty, "value": value})
}

/// Follows `definedTypeLinkNode`s until a concrete node or an unknown name.
pub fn resolve_link<'a>(mut ty: &'a Value, definitions: &'a BTreeMap<String, Value>) -> &'a Value {
    for _ in 0..32 {
        if kind(ty) != Some("definedTypeLinkNode") {
            return ty;
        }
        match ty
            .get("name")
            .and_then(Value::as_str)
            .and_then(|name| definitions.get(name))
        {
            Some(next) => ty = next,
            None => return ty,
        }
    }
    ty
}

/// Strips wrappers that change encoding but not the shape of the value.
pub fn unwrap_wrappers(mut ty: &Value) -> &Value {
    loop {
        match kind(ty) {
            Some(
                "fixedSizeTypeNode"
                | "sizePrefixTypeNode"
                | "hiddenPrefixTypeNode"
                | "hiddenSuffixTypeNode"
                | "preOffsetTypeNode"
                | "postOffsetTypeNode"
                | "sentinelTypeNode",
            ) => ty = &ty["type"],
            Some("amountTypeNode" | "dateTimeTypeNode" | "solAmountTypeNode") => {
                ty = &ty["number"];
            }
            _ => return ty,
        }
    }
}

/// Resolves links and strips transparent wrappers, exposing the value's shape.
pub fn shape<'a>(ty: &'a Value, definitions: &'a BTreeMap<String, Value>) -> &'a Value {
    let mut current = ty;
    for _ in 0..32 {
        let next = unwrap_wrappers(resolve_link(current, definitions));
        if std::ptr::eq(next, current) {
            return current;
        }
        current = next;
    }
    current
}

/// Returns a struct node's `structFieldTypeNode`s, or `None` for other shapes.
pub fn struct_fields(ty: &Value) -> Option<&Vec<Value>> {
    (kind(ty) == Some("structTypeNode"))
        .then(|| ty.get("fields").and_then(Value::as_array))
        .flatten()
}

/// Returns the item node of an array or set shape.
pub fn sequence_item(ty: &Value) -> Option<&Value> {
    match kind(ty) {
        Some("arrayTypeNode" | "setTypeNode") => ty.get("item"),
        _ => None,
    }
}

/// Whether a field is always encoded from its default and hidden from callers.
pub fn field_is_omitted(field: &Value) -> bool {
    field.get("defaultValueStrategy").and_then(Value::as_str) == Some("omitted")
}

/// Whether a type decodes to a sequence of bytes worth rendering in hex.
pub fn is_byte_sequence(ty: &Value, definitions: &BTreeMap<String, Value>) -> bool {
    let shaped = shape(ty, definitions);
    match kind(shaped) {
        Some("bytesTypeNode") => true,
        Some("arrayTypeNode") => shaped
            .get("item")
            .map(|item| shape(item, definitions))
            .is_some_and(|item| {
                kind(item) == Some("numberTypeNode")
                    && item.get("format").and_then(Value::as_str) == Some("u8")
            }),
        _ => false,
    }
}

/// Byte width of a Codama number format, or `None` for variable-width formats.
pub fn number_width(format: &str) -> Option<usize> {
    Some(match format {
        "u8" | "i8" => 1,
        "u16" | "i16" => 2,
        "u32" | "i32" | "f32" => 4,
        "u64" | "i64" | "f64" => 8,
        "u128" | "i128" => 16,
        "u256" | "i256" => 32,
        _ => return None,
    })
}

/// Computes the exact encoded size of a type when it does not depend on the value.
pub fn fixed_size_of(ty: &Value, definitions: &BTreeMap<String, Value>) -> Option<usize> {
    let ty = resolve_link(ty, definitions);
    match kind(ty)? {
        "numberTypeNode" => number_width(ty.get("format")?.as_str()?),
        "booleanTypeNode" => ty
            .get("size")
            .map_or(Some(1), |size| fixed_size_of(size, definitions)),
        "publicKeyTypeNode" => Some(32),
        "fixedSizeTypeNode" => usize::try_from(ty.get("size")?.as_u64()?).ok(),
        "arrayTypeNode" | "setTypeNode" => {
            let count = fixed_count_value(ty.get("count")?)?;
            Some(count * fixed_size_of(ty.get("item")?, definitions)?)
        }
        "mapTypeNode" => {
            let count = fixed_count_value(ty.get("count")?)?;
            let key = fixed_size_of(ty.get("key")?, definitions)?;
            let value = fixed_size_of(ty.get("value")?, definitions)?;
            Some(count * (key + value))
        }
        "tupleTypeNode" => ty
            .get("items")?
            .as_array()?
            .iter()
            .map(|item| fixed_size_of(item, definitions))
            .sum(),
        "structTypeNode" => ty
            .get("fields")?
            .as_array()?
            .iter()
            .map(|field| fixed_size_of(field.get("type")?, definitions))
            .sum(),
        "optionTypeNode" => {
            if ty.get("fixed").and_then(Value::as_bool) != Some(true) {
                return None;
            }
            let prefix = ty
                .get("prefix")
                .map_or(Some(1), |prefix| fixed_size_of(prefix, definitions))?;
            Some(prefix + fixed_size_of(ty.get("item")?, definitions)?)
        }
        "zeroableOptionTypeNode" => fixed_size_of(ty.get("item")?, definitions),
        "enumTypeNode" => {
            let size = ty
                .get("size")
                .map_or(Some(1), |size| fixed_size_of(size, definitions))?;
            let mut payload = None;
            for variant in ty.get("variants")?.as_array()? {
                let variant_size = match kind(variant)? {
                    "enumEmptyVariantTypeNode" => 0,
                    "enumTupleVariantTypeNode" => {
                        fixed_size_of(variant.get("tuple")?, definitions)?
                    }
                    "enumStructVariantTypeNode" => {
                        fixed_size_of(variant.get("struct")?, definitions)?
                    }
                    _ => return None,
                };
                if payload.is_some_and(|known| known != variant_size) {
                    return None;
                }
                payload = Some(variant_size);
            }
            Some(size + payload.unwrap_or(0))
        }
        "amountTypeNode" | "dateTimeTypeNode" | "solAmountTypeNode" => {
            fixed_size_of(ty.get("number")?, definitions)
        }
        "hiddenPrefixTypeNode" | "hiddenSuffixTypeNode" => {
            let constants = ty
                .get("prefix")
                .or_else(|| ty.get("suffix"))?
                .as_array()?
                .iter()
                .map(|constant| fixed_size_of(constant.get("type")?, definitions))
                .sum::<Option<usize>>()?;
            Some(constants + fixed_size_of(ty.get("type")?, definitions)?)
        }
        "preOffsetTypeNode" | "postOffsetTypeNode" => {
            if ty.get("strategy").and_then(Value::as_str) != Some("padded") {
                return None;
            }
            let offset = usize::try_from(ty.get("offset")?.as_i64()?).ok()?;
            Some(offset + fixed_size_of(ty.get("type")?, definitions)?)
        }
        _ => None,
    }
}

fn fixed_count_value(count: &Value) -> Option<usize> {
    (kind(count) == Some("fixedCountNode"))
        .then(|| usize::try_from(count.get("value")?.as_u64()?).ok())
        .flatten()
}

/// Converts a Codama value node into the JSON value shape the codec encodes.
///
/// Numbers stay JSON numbers, byte values become arrays of integers, enum
/// values become `{"kind": VARIANT, "fields": ...}` objects, and map values
/// become arrays of `{"key", "value"}` entries.
pub fn value_node_to_json(node: &Value) -> Result<Value> {
    let Some(kind) = kind(node) else {
        bail!("value node lacks kind: {node}")
    };
    Ok(match kind {
        "numberValueNode" => node
            .get("number")
            .cloned()
            .context("numberValueNode lacks number")?,
        "booleanValueNode" => node
            .get("boolean")
            .cloned()
            .context("booleanValueNode lacks boolean")?,
        "stringValueNode" => node
            .get("string")
            .cloned()
            .context("stringValueNode lacks string")?,
        "publicKeyValueNode" => node
            .get("publicKey")
            .cloned()
            .context("publicKeyValueNode lacks publicKey")?,
        "bytesValueNode" => Value::Array(
            decode_bytes_value(node)?
                .into_iter()
                .map(|byte| Value::Number(byte.into()))
                .collect(),
        ),
        "noneValueNode" => Value::Null,
        "someValueNode" => value_node_to_json(node.get("value").context("someValueNode lacks value")?)?,
        "arrayValueNode" | "tupleValueNode" | "setValueNode" => Value::Array(
            node.get("items")
                .and_then(Value::as_array)
                .context("value node lacks items")?
                .iter()
                .map(value_node_to_json)
                .collect::<Result<_>>()?,
        ),
        "structValueNode" => {
            let mut object = Map::new();
            for field in node
                .get("fields")
                .and_then(Value::as_array)
                .context("structValueNode lacks fields")?
            {
                let name = field
                    .get("name")
                    .and_then(Value::as_str)
                    .context("structFieldValueNode lacks name")?;
                object.insert(
                    name.to_owned(),
                    value_node_to_json(field.get("value").context("structFieldValueNode lacks value")?)?,
                );
            }
            Value::Object(object)
        }
        "enumValueNode" => {
            let mut object = Map::new();
            object.insert(
                "kind".to_owned(),
                node.get("variant")
                    .cloned()
                    .context("enumValueNode lacks variant")?,
            );
            if let Some(value) = node.get("value") {
                object.insert("fields".to_owned(), value_node_to_json(value)?);
            }
            Value::Object(object)
        }
        "mapValueNode" => Value::Array(
            node.get("entries")
                .and_then(Value::as_array)
                .context("mapValueNode lacks entries")?
                .iter()
                .map(|entry| {
                    Ok(json!({
                        "key": value_node_to_json(entry.get("key").context("map entry lacks key")?)?,
                        "value": value_node_to_json(entry.get("value").context("map entry lacks value")?)?,
                    }))
                })
                .collect::<Result<_>>()?,
        ),
        "constantValueNode" => {
            value_node_to_json(node.get("value").context("constantValueNode lacks value")?)?
        }
        other => bail!("unsupported Codama value node {other}"),
    })
}

/// Decodes the raw bytes of a `bytesValueNode`.
pub fn decode_bytes_value(node: &Value) -> Result<Vec<u8>> {
    let data = node
        .get("data")
        .and_then(Value::as_str)
        .context("bytesValueNode lacks data")?;
    let encoding = node
        .get("encoding")
        .and_then(Value::as_str)
        .unwrap_or("base16");
    decode_encoded_string(data, encoding)
}

/// Decodes text in one of Codama's string encodings into bytes.
pub fn decode_encoded_string(text: &str, encoding: &str) -> Result<Vec<u8>> {
    Ok(match encoding {
        "utf8" => text.as_bytes().to_vec(),
        "base16" => hex::decode(text).context("invalid base16 data")?,
        "base58" => bs58::decode(text)
            .into_vec()
            .context("invalid base58 data")?,
        "base64" => base64::engine::general_purpose::STANDARD
            .decode(text)
            .context("invalid base64 data")?,
        other => bail!("unsupported string encoding {other}"),
    })
}

/// Encodes bytes as text in one of Codama's string encodings.
pub fn encode_to_string(bytes: &[u8], encoding: &str) -> Result<String> {
    Ok(match encoding {
        "utf8" => String::from_utf8(bytes.to_vec()).context("string is not UTF-8")?,
        "base16" => hex::encode(bytes),
        "base58" => bs58::encode(bytes).into_string(),
        "base64" => base64::engine::general_purpose::STANDARD.encode(bytes),
        other => bail!("unsupported string encoding {other}"),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn computes_fixed_sizes_through_links_and_wrappers() {
        let mut definitions = BTreeMap::new();
        definitions.insert(
            "header".to_owned(),
            struct_node(vec![
                struct_field("discriminator", number("u8")),
                struct_field("owner", public_key()),
            ]),
        );
        let ty = struct_node(vec![
            struct_field("header", defined_link("header")),
            struct_field("name", fixed_size(string("utf8"), 16)),
            struct_field("flags", array(boolean(), fixed_count(3))),
        ]);
        assert_eq!(fixed_size_of(&ty, &definitions), Some(33 + 16 + 3));
        assert_eq!(
            fixed_size_of(&size_prefix(string("utf8"), number("u32")), &definitions),
            None
        );
    }

    #[test]
    fn converts_value_nodes_to_codec_json() {
        let node = json!({"kind": "structValueNode", "fields": [
            {"kind": "structFieldValueNode", "name": "id", "value": {"kind": "numberValueNode", "number": 7}},
            {"kind": "structFieldValueNode", "name": "tag", "value": {"kind": "bytesValueNode", "data": "0102", "encoding": "base16"}},
            {"kind": "structFieldValueNode", "name": "state", "value": {"kind": "enumValueNode", "enum": defined_link("state"), "variant": "open"}},
        ]});
        assert_eq!(
            value_node_to_json(&node).unwrap(),
            json!({"id": 7, "tag": [1, 2], "state": {"kind": "open"}})
        );
    }
}
