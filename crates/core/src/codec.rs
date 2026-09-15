//! A bounded Borsh codec for the practical Anchor/Codama layout subset.

use anyhow::{Context, Result, bail};
use serde_json::{Map, Number, Value};
use tracing::{debug, trace};
use wip_types::AccountSchema;

/// Details about bytes present after an IDL account layout has been decoded.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct AccountDecodeInfo {
    /// Total bytes not described by the IDL layout.
    pub trailing_byte_count: usize,
    /// Number of non-zero bytes among the trailing bytes.
    pub non_zero_trailing_byte_count: usize,
}

impl AccountDecodeInfo {
    /// Whether trailing bytes contain non-zero data beyond expected allocation padding.
    pub fn has_unmodelled_data(self) -> bool {
        self.non_zero_trailing_byte_count > 0
    }
}

/// Decodes one account, requiring its discriminator and accepting trailing allocation data.
pub fn decode_account(
    account: &AccountSchema,
    definitions: &std::collections::BTreeMap<String, Value>,
    bytes: &[u8],
) -> Result<Value> {
    Ok(decode_account_with_info(account, definitions, bytes)?.0)
}

/// Decodes an account and reports bytes the IDL does not describe.
///
/// Anchor accounts may reserve more space than their Borsh fields occupy or
/// intentionally store program-specific data after their serialized layout.
/// A successful layout decode remains useful in either case. Callers that
/// rewrite the account data should reject [`AccountDecodeInfo::has_unmodelled_data`]
/// to avoid dropping that data.
pub fn decode_account_with_info(
    account: &AccountSchema,
    definitions: &std::collections::BTreeMap<String, Value>,
    bytes: &[u8],
) -> Result<(Value, AccountDecodeInfo)> {
    if !bytes.starts_with(&account.discriminator) {
        bail!("account data does not match {} discriminator", account.name)
    }
    let mut decoder = Decoder {
        bytes: &bytes[account.discriminator.len()..],
        offset: 0,
        definitions,
    };
    debug!(account = %account.name, byte_length = bytes.len(), "decoding account data");
    let value = match decoder.decode(&account.ty) {
        Ok(v) => v,
        Err(e) => {
            debug!(account = %account.name, byte_length = bytes.len(), error = %e, "failed to decode account data");
            return Err(e);
        }
    };
    debug!(account = %account.name, decoded_length = decoder.offset, decoder_bytes_length = decoder.bytes.len(), "decoded account data");
    let trailing = &decoder.bytes[decoder.offset..];
    let info = AccountDecodeInfo {
        trailing_byte_count: trailing.len(),
        non_zero_trailing_byte_count: trailing.iter().filter(|byte| **byte != 0).count(),
    };
    if info.has_unmodelled_data() {
        debug!(
            account = %account.name,
            trailing_byte_count = info.trailing_byte_count,
            non_zero_trailing_byte_count = info.non_zero_trailing_byte_count,
            "accepted unmodelled trailing account data"
        );
    } else if !trailing.is_empty() {
        debug!(
            account = %account.name,
            padding_length = trailing.len(),
            "accepted zero-filled account allocation padding"
        );
    }
    Ok((value, info))
}

/// Encodes an account value with the schema's discriminator.
pub fn encode_account(
    account: &AccountSchema,
    definitions: &std::collections::BTreeMap<String, Value>,
    value: &Value,
) -> Result<Vec<u8>> {
    let mut output = account.discriminator.clone();
    encode_type(&account.ty, value, definitions, &mut output)?;
    Ok(output)
}

/// Parses a CLI string as JSON when possible and otherwise treats it as a string.
pub fn parse_cli_value(input: &str) -> Value {
    serde_json::from_str(input).unwrap_or_else(|_| Value::String(input.to_owned()))
}

/// Applies a dotted field and bracket-index path to a decoded dynamic value.
pub fn patch_value(target: &mut Value, path: &str, replacement: Value) -> Result<()> {
    let tokens = parse_path(path)?;
    let mut cursor = target;
    for (index, token) in tokens.iter().enumerate() {
        let final_token = index + 1 == tokens.len();
        match token {
            PathToken::Field(name) => {
                let object = cursor
                    .as_object_mut()
                    .ok_or_else(|| anyhow::anyhow!("{name} is not a struct field"))?;
                if final_token {
                    let slot = object
                        .get_mut(name)
                        .ok_or_else(|| anyhow::anyhow!("unknown field {name}"))?;
                    *slot = replacement;
                    return Ok(());
                }
                cursor = object
                    .get_mut(name)
                    .ok_or_else(|| anyhow::anyhow!("unknown field {name}"))?;
            }
            PathToken::Index(index) => {
                let array = cursor
                    .as_array_mut()
                    .ok_or_else(|| anyhow::anyhow!("[{index}] is not an array index"))?;
                let slot = array
                    .get_mut(*index)
                    .ok_or_else(|| anyhow::anyhow!("array index {index} is out of range"))?;
                if final_token {
                    *slot = replacement;
                    return Ok(());
                }
                cursor = slot;
            }
        }
    }
    bail!("patch path is empty")
}

#[derive(Debug)]
enum PathToken {
    Field(String),
    Index(usize),
}

fn parse_path(path: &str) -> Result<Vec<PathToken>> {
    if path.is_empty() {
        bail!("patch path cannot be empty")
    }
    let mut out = Vec::new();
    let mut field = String::new();
    let chars: Vec<_> = path.chars().collect();
    let mut index = 0;
    while index < chars.len() {
        match chars[index] {
            '.' => {
                if field.is_empty() {
                    bail!("invalid patch path {path}")
                }
                out.push(PathToken::Field(std::mem::take(&mut field)));
                index += 1;
            }
            '[' => {
                if !field.is_empty() {
                    out.push(PathToken::Field(std::mem::take(&mut field)));
                }
                let end = chars[index..]
                    .iter()
                    .position(|ch| *ch == ']')
                    .ok_or_else(|| anyhow::anyhow!("missing ] in patch path"))?
                    + index;
                let number: String = chars[index + 1..end].iter().collect();
                out.push(PathToken::Index(
                    number.parse().context("array index must be numeric")?,
                ));
                index = end + 1;
            }
            ch => {
                field.push(ch);
                index += 1;
            }
        }
    }
    if !field.is_empty() {
        out.push(PathToken::Field(field));
    }
    Ok(out)
}

struct Decoder<'a> {
    bytes: &'a [u8],
    offset: usize,
    definitions: &'a std::collections::BTreeMap<String, Value>,
}
impl Decoder<'_> {
    fn take(&mut self, count: usize) -> Result<&[u8]> {
        let end = self
            .offset
            .checked_add(count)
            .ok_or_else(|| anyhow::anyhow!("length overflow"))?;
        let value = self
            .bytes
            .get(self.offset..end)
            .ok_or_else(|| anyhow::anyhow!("unexpected end of account data"))?;
        self.offset = end;
        Ok(value)
    }
    fn u8(&mut self) -> Result<u8> {
        Ok(self.take(1)?[0])
    }
    fn length(&mut self) -> Result<usize> {
        usize::try_from(u32::from_le_bytes(
            self.take(4)?.try_into().expect("4 bytes"),
        ))
        .context("length does not fit usize")
    }
    fn decode(&mut self, ty: &Value) -> Result<Value> {
        trace!(ty = ?ty, offset = self.offset, "decoding IDL type");
        if let Some(name) = ty.as_str() {
            return self.decode_name(name);
        }
        let object = ty
            .as_object()
            .ok_or_else(|| anyhow::anyhow!("unsupported non-object IDL type {ty}"))?;
        if let Some(inner) = object.get("vec") {
            let length = self.length()?;
            debug!(length, "decoding vec of length");
            return (0..length)
                .map(|_| self.decode(inner))
                .collect::<Result<Vec<_>>>()
                .map(Value::Array);
        }
        if let Some(inner) = object.get("option") {
            return if self.u8()? == 0 {
                Ok(Value::Null)
            } else {
                self.decode(inner)
            };
        }
        if let Some(array) = object.get("array").and_then(Value::as_array) {
            let [inner, length] = array.as_slice() else {
                bail!("array type must have [type, length]")
            };
            debug!(length = %length, "decoding fixed-length array");
            return (0..length
                .as_u64()
                .ok_or_else(|| anyhow::anyhow!("array length must be integer"))?)
                .map(|_| self.decode(inner))
                .collect::<Result<Vec<_>>>()
                .map(Value::Array);
        }
        if let Some(name) = defined_name(object) {
            debug!(name, "decoding defined type");
            return self.decode(
                self.definitions
                    .get(name)
                    .ok_or_else(|| anyhow::anyhow!("unknown defined type {name}"))?,
            );
        }
        match object.get("kind").and_then(Value::as_str) {
            Some("struct") => {
                let mut result = Map::new();
                for field in object
                    .get("fields")
                    .and_then(Value::as_array)
                    .ok_or_else(|| anyhow::anyhow!("struct lacks fields"))?
                {
                    let name = field
                        .get("name")
                        .and_then(Value::as_str)
                        .ok_or_else(|| anyhow::anyhow!("struct field lacks name"))?;
                    result.insert(
                        name.to_owned(),
                        self.decode(
                            field
                                .get("type")
                                .ok_or_else(|| anyhow::anyhow!("field {name} lacks type"))?,
                        )?,
                    );
                }
                Ok(Value::Object(result))
            }
            Some("enum") => self.decode_enum(object),
            _ => bail!(
                "unsupported IDL codec; only Borsh scalars, structs, enums, arrays, vec, option, and defined types are supported"
            ),
        }
    }
    fn decode_name(&mut self, name: &str) -> Result<Value> {
        let integer = |bytes: &[u8]| {
            Value::String(
                bytes
                    .iter()
                    .rev()
                    .fold(0u128, |value, byte| value * 256 + u128::from(*byte))
                    .to_string(),
            )
        };
        match name {
            "u8" => Ok(Value::Number(Number::from(self.u8()?))),
            "i8" => Ok(Value::Number(Number::from(self.u8()? as i8))),
            "u16" | "i16" => Ok(integer(self.take(2)?)),
            "u32" | "i32" => Ok(integer(self.take(4)?)),
            "u64" | "i64" | "u128" | "i128" | "u256" | "i256" => {
                let width = if name.ends_with("256") {
                    32
                } else if name.ends_with("128") {
                    16
                } else {
                    8
                };
                Ok(integer(self.take(width)?))
            }
            "bool" => match self.u8()? {
                0 => Ok(Value::Bool(false)),
                1 => Ok(Value::Bool(true)),
                _ => bail!("invalid Borsh bool"),
            },
            "string" => {
                let length = self.length()?;
                Ok(Value::String(
                    String::from_utf8(self.take(length)?.to_vec())
                        .context("string is not UTF-8")?,
                ))
            }
            "publicKey" | "pubkey" => Ok(Value::String(bs58::encode(self.take(32)?).into_string())),
            "bytes" => {
                let length = self.length()?;
                Ok(Value::Array(
                    self.take(length)?
                        .iter()
                        .map(|byte| Value::Number(Number::from(*byte)))
                        .collect(),
                ))
            }
            other => self.decode(
                self.definitions
                    .get(other)
                    .ok_or_else(|| anyhow::anyhow!("unknown IDL type {other}"))?,
            ),
        }
    }
    fn decode_enum(&mut self, object: &Map<String, Value>) -> Result<Value> {
        let variants = object
            .get("variants")
            .and_then(Value::as_array)
            .ok_or_else(|| anyhow::anyhow!("enum lacks variants"))?;
        let variant = variants
            .get(usize::from(self.u8()?))
            .ok_or_else(|| anyhow::anyhow!("enum discriminator is out of range"))?;
        let name = variant
            .get("name")
            .and_then(Value::as_str)
            .ok_or_else(|| anyhow::anyhow!("enum variant lacks name"))?;
        let mut out = Map::new();
        out.insert("kind".to_owned(), Value::String(name.to_owned()));
        if let Some(fields) = variant.get("fields").and_then(Value::as_array) {
            let values = fields
                .iter()
                .map(|field| self.decode(field.get("type").unwrap_or(field)))
                .collect::<Result<Vec<_>>>()?;
            out.insert("fields".to_owned(), Value::Array(values));
        }
        Ok(Value::Object(out))
    }
}

fn defined_name(object: &Map<String, Value>) -> Option<&str> {
    object.get("defined").and_then(Value::as_str).or_else(|| {
        object
            .get("defined")
            .and_then(|value| value.get("name"))
            .and_then(Value::as_str)
    })
}

/// Encodes a dynamic value according to one Borsh-compatible type expression.
pub fn encode_type(
    ty: &Value,
    value: &Value,
    definitions: &std::collections::BTreeMap<String, Value>,
    output: &mut Vec<u8>,
) -> Result<()> {
    if let Some(name) = ty.as_str() {
        return encode_name(name, value, definitions, output);
    }
    let object = ty
        .as_object()
        .ok_or_else(|| anyhow::anyhow!("unsupported non-object type"))?;
    if let Some(inner) = object.get("vec") {
        let array = value
            .as_array()
            .ok_or_else(|| anyhow::anyhow!("vec value must be an array"))?;
        output.extend_from_slice(
            &(u32::try_from(array.len()).context("vec too long")?).to_le_bytes(),
        );
        for item in array {
            encode_type(inner, item, definitions, output)?;
        }
        return Ok(());
    }
    if let Some(inner) = object.get("option") {
        if value.is_null() {
            output.push(0);
        } else {
            output.push(1);
            encode_type(inner, value, definitions, output)?;
        }
        return Ok(());
    }
    if let Some(array) = object.get("array").and_then(Value::as_array) {
        let [inner, length] = array.as_slice() else {
            bail!("array type must have [type, length]")
        };
        let values = value
            .as_array()
            .ok_or_else(|| anyhow::anyhow!("array value must be an array"))?;
        if values.len() != length.as_u64().unwrap_or(0) as usize {
            bail!("array value has wrong length")
        }
        for item in values {
            encode_type(inner, item, definitions, output)?;
        }
        return Ok(());
    }
    if let Some(name) = defined_name(object) {
        return encode_type(
            definitions
                .get(name)
                .ok_or_else(|| anyhow::anyhow!("unknown defined type {name}"))?,
            value,
            definitions,
            output,
        );
    }
    match object.get("kind").and_then(Value::as_str) {
        Some("struct") => {
            let values = value
                .as_object()
                .ok_or_else(|| anyhow::anyhow!("struct value must be an object"))?;
            for field in object
                .get("fields")
                .and_then(Value::as_array)
                .ok_or_else(|| anyhow::anyhow!("struct lacks fields"))?
            {
                let name = field
                    .get("name")
                    .and_then(Value::as_str)
                    .ok_or_else(|| anyhow::anyhow!("struct field lacks name"))?;
                encode_type(
                    field
                        .get("type")
                        .ok_or_else(|| anyhow::anyhow!("field {name} lacks type"))?,
                    values
                        .get(name)
                        .ok_or_else(|| anyhow::anyhow!("missing field {name}"))?,
                    definitions,
                    output,
                )?;
            }
            Ok(())
        }
        Some("enum") => encode_enum(object, value, definitions, output),
        _ => bail!("unsupported IDL codec"),
    }
}

fn encode_name(
    name: &str,
    value: &Value,
    definitions: &std::collections::BTreeMap<String, Value>,
    output: &mut Vec<u8>,
) -> Result<()> {
    let integer = || {
        value
            .as_u64()
            .map(|number| number.to_string())
            .or_else(|| value.as_str().map(ToOwned::to_owned))
            .ok_or_else(|| anyhow::anyhow!("{name} value must be an integer or decimal string"))
    };
    match name {
        "u8" => output.push(integer()?.parse::<u8>()?),
        "i8" => output.push(integer()?.parse::<i8>()? as u8),
        "u16" | "i16" => output.extend_from_slice(&(integer()?.parse::<u16>()?).to_le_bytes()),
        "u32" | "i32" => output.extend_from_slice(&(integer()?.parse::<u32>()?).to_le_bytes()),
        "u64" | "i64" => output.extend_from_slice(&(integer()?.parse::<u64>()?).to_le_bytes()),
        "u128" | "i128" => output.extend_from_slice(&(integer()?.parse::<u128>()?).to_le_bytes()),
        "u256" | "i256" => bail!("u256/i256 encoding is not implemented"),
        "bool" => output.push(
            if value
                .as_bool()
                .ok_or_else(|| anyhow::anyhow!("bool value required"))?
            {
                1
            } else {
                0
            },
        ),
        "string" => {
            let text = value
                .as_str()
                .ok_or_else(|| anyhow::anyhow!("string value required"))?;
            output.extend_from_slice(&(u32::try_from(text.len())?).to_le_bytes());
            output.extend_from_slice(text.as_bytes());
        }
        "publicKey" | "pubkey" => {
            let text = value
                .as_str()
                .ok_or_else(|| anyhow::anyhow!("public key must be base58 string"))?;
            let bytes = bs58::decode(text).into_vec()?;
            if bytes.len() != 32 {
                bail!("public key must be 32 bytes")
            }
            output.extend_from_slice(&bytes);
        }
        "bytes" => {
            let bytes = byte_array(value)?;
            output.extend_from_slice(&(u32::try_from(bytes.len())?).to_le_bytes());
            output.extend_from_slice(&bytes);
        }
        other => {
            return encode_type(
                definitions
                    .get(other)
                    .ok_or_else(|| anyhow::anyhow!("unknown IDL type {other}"))?,
                value,
                definitions,
                output,
            );
        }
    }
    Ok(())
}

fn encode_enum(
    object: &Map<String, Value>,
    value: &Value,
    definitions: &std::collections::BTreeMap<String, Value>,
    output: &mut Vec<u8>,
) -> Result<()> {
    let values = value.as_object().ok_or_else(|| {
        anyhow::anyhow!("enum value must be an object with kind and optional fields")
    })?;
    let kind = values
        .get("kind")
        .and_then(Value::as_str)
        .ok_or_else(|| anyhow::anyhow!("enum value lacks kind"))?;
    let variants = object
        .get("variants")
        .and_then(Value::as_array)
        .ok_or_else(|| anyhow::anyhow!("enum lacks variants"))?;
    let (index, variant) = variants
        .iter()
        .enumerate()
        .find(|(_, candidate)| candidate.get("name").and_then(Value::as_str) == Some(kind))
        .ok_or_else(|| anyhow::anyhow!("unknown enum variant {kind}"))?;
    output.push(u8::try_from(index)?);
    if let Some(fields) = variant.get("fields").and_then(Value::as_array) {
        let supplied = values
            .get("fields")
            .and_then(Value::as_array)
            .ok_or_else(|| anyhow::anyhow!("enum fields required"))?;
        if supplied.len() != fields.len() {
            bail!("enum field count mismatch")
        }
        for (field, supplied) in fields.iter().zip(supplied) {
            encode_type(
                field.get("type").unwrap_or(field),
                supplied,
                definitions,
                output,
            )?;
        }
    }
    Ok(())
}

fn byte_array(value: &Value) -> Result<Vec<u8>> {
    value
        .as_array()
        .ok_or_else(|| anyhow::anyhow!("byte value must be an array"))?
        .iter()
        .map(|value| {
            value
                .as_u64()
                .filter(|number| *number <= 255)
                .map(|number| number as u8)
                .ok_or_else(|| anyhow::anyhow!("invalid byte"))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::collections::BTreeMap;
    #[test]
    fn decodes_patches_and_reencodes() {
        let account = AccountSchema {
            name: "State".into(),
            discriminator: vec![1; 8],
            ty: json!({"kind":"struct","fields":[{"name":"count","type":"u64"},{"name":"enabled","type":"bool"}]}),
        };
        let raw = [vec![1; 8], 7u64.to_le_bytes().to_vec(), vec![1]].concat();
        let mut value = decode_account(&account, &BTreeMap::new(), &raw).unwrap();
        patch_value(&mut value, "count", json!(9)).unwrap();
        assert_eq!(
            encode_account(&account, &BTreeMap::new(), &value).unwrap(),
            [vec![1; 8], 9u64.to_le_bytes().to_vec(), vec![1]].concat()
        );
    }

    #[test]
    fn accepts_zero_filled_account_allocation_padding() {
        let account = AccountSchema {
            name: "State".into(),
            discriminator: vec![1; 8],
            ty: json!({"kind":"struct","fields":[{"name":"count","type":"u64"}]}),
        };
        let raw = [vec![1; 8], 7u64.to_le_bytes().to_vec(), vec![0; 16]].concat();

        assert_eq!(
            decode_account(&account, &BTreeMap::new(), &raw).unwrap(),
            json!({"count": "7"})
        );
    }

    #[test]
    fn reports_non_zero_trailing_account_data() {
        let account = AccountSchema {
            name: "State".into(),
            discriminator: vec![1; 8],
            ty: json!({"kind":"struct","fields":[{"name":"count","type":"u64"}]}),
        };
        let raw = [vec![1; 8], 7u64.to_le_bytes().to_vec(), vec![0, 1, 0]].concat();

        let (value, info) = decode_account_with_info(&account, &BTreeMap::new(), &raw).unwrap();

        assert_eq!(value, json!({"count": "7"}));
        assert_eq!(info.trailing_byte_count, 3);
        assert_eq!(info.non_zero_trailing_byte_count, 1);
        assert!(info.has_unmodelled_data());
    }
}
