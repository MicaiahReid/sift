//! A Borsh-compatible codec driven by Codama type nodes.
//!
//! Decoded values use plain JSON: integers up to 32 bits and floats are JSON
//! numbers, wider integers are decimal strings, public keys are base58
//! strings, byte sequences are arrays of integers, options are `null` or the
//! inner value, structs are objects, tuples and sets are arrays, maps are
//! arrays of `{"key", "value"}` entries, and enums are
//! `{"kind": VARIANT}` objects with an optional `fields` payload. Struct
//! fields whose `defaultValueStrategy` is `omitted` are hidden on decode and
//! filled from their default on encode.

use std::collections::BTreeMap;

use anyhow::{Context, Result, bail};
use serde_json::{Map, Number, Value};
use sift_types::{AccountSchema, Discriminator};
use tracing::{debug, trace};

use crate::nodes::{
    self, decode_bytes_value, encode_to_string, field_is_omitted, fixed_size_of, kind,
    value_node_to_json,
};

/// Details about bytes an IDL account layout did not describe.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct AccountDecodeInfo {
    /// Total bytes not described by the IDL layout: bytes after the layout
    /// plus bytes left unread inside size-bounded regions such as
    /// `fixedSizeTypeNode` and `sizePrefixTypeNode`.
    pub trailing_byte_count: usize,
    /// Number of non-zero bytes among the undescribed bytes.
    pub non_zero_trailing_byte_count: usize,
}

impl AccountDecodeInfo {
    /// Whether undescribed bytes contain non-zero data beyond expected padding.
    pub fn has_unmodelled_data(self) -> bool {
        self.non_zero_trailing_byte_count > 0
    }

    fn record(&mut self, bytes: &[u8]) {
        self.trailing_byte_count += bytes.len();
        self.non_zero_trailing_byte_count += bytes.iter().filter(|byte| **byte != 0).count();
    }

    fn merge(&mut self, other: Self) {
        self.trailing_byte_count += other.trailing_byte_count;
        self.non_zero_trailing_byte_count += other.non_zero_trailing_byte_count;
    }
}

/// Whether `bytes` satisfy every discriminator; an empty list always matches.
pub fn matches_discriminators(discriminators: &[Discriminator], bytes: &[u8]) -> bool {
    discriminators
        .iter()
        .all(|discriminator| match discriminator {
            Discriminator::Constant {
                offset,
                bytes: expected,
                ..
            } => bytes.get(*offset..offset + expected.len()) == Some(expected.as_slice()),
            Discriminator::Size { size } => bytes.len() == *size,
        })
}

/// Decodes one account, requiring its discriminators and accepting trailing allocation data.
pub fn decode_account(
    account: &AccountSchema,
    definitions: &BTreeMap<String, Value>,
    bytes: &[u8],
) -> Result<Value> {
    Ok(decode_account_with_info(account, definitions, bytes)?.0)
}

/// Decodes an account and reports bytes the IDL does not describe.
///
/// Programs may reserve more space than their fields occupy or intentionally
/// store program-specific data after their serialized layout. A successful
/// layout decode remains useful in either case. Callers that rewrite the
/// account data should reject [`AccountDecodeInfo::has_unmodelled_data`] to
/// avoid dropping that data.
pub fn decode_account_with_info(
    account: &AccountSchema,
    definitions: &BTreeMap<String, Value>,
    bytes: &[u8],
) -> Result<(Value, AccountDecodeInfo)> {
    if !matches_discriminators(&account.discriminators, bytes) {
        bail!("account data does not match {} discriminator", account.name)
    }
    let mut decoder = Decoder::new(bytes, definitions);
    debug!(account = %account.name, byte_length = bytes.len(), "decoding account data");
    let value = match decoder.decode(&account.ty) {
        Ok(value) => value,
        Err(error) => {
            debug!(account = %account.name, byte_length = bytes.len(), error = %error, "failed to decode account data");
            return Err(error);
        }
    };
    let mut info = decoder.unread;
    info.record(&bytes[decoder.offset.min(bytes.len())..]);
    debug!(
        account = %account.name,
        decoded_length = decoder.offset,
        trailing_byte_count = info.trailing_byte_count,
        non_zero_trailing_byte_count = info.non_zero_trailing_byte_count,
        "decoded account data"
    );
    Ok((value, info))
}

/// Encodes an account value, filling omitted fields such as discriminators from their defaults.
pub fn encode_account(
    account: &AccountSchema,
    definitions: &BTreeMap<String, Value>,
    value: &Value,
) -> Result<Vec<u8>> {
    let mut output = Vec::new();
    encode_type(&account.ty, value, definitions, &mut output)?;
    if !matches_discriminators(&account.discriminators, &output) {
        bail!(
            "encoded {} data does not carry its discriminator; the layout's discriminator field has no default value",
            account.name
        )
    }
    Ok(output)
}

/// Encodes a `constantValueNode` into bytes.
pub fn encode_constant(constant: &Value, definitions: &BTreeMap<String, Value>) -> Result<Vec<u8>> {
    if kind(constant) != Some("constantValueNode") {
        bail!("expected a constantValueNode, found {constant}")
    }
    let mut output = Vec::new();
    encode_type(
        constant
            .get("type")
            .context("constantValueNode lacks type")?,
        &value_node_to_json(
            constant
                .get("value")
                .context("constantValueNode lacks value")?,
        )?,
        definitions,
        &mut output,
    )?;
    Ok(output)
}

/// Parses a CLI string as JSON when possible and otherwise treats it as a string.
pub fn parse_cli_value(input: &str) -> Value {
    serde_json::from_str(input).unwrap_or_else(|_| Value::String(input.to_owned()))
}

/// Reads the value at a dotted field and bracket-index path, or `None` when absent.
///
/// Returns an error only when the path itself is malformed; a path that does
/// not exist in the value is not an error.
pub fn lookup_path<'a>(target: &'a Value, path: &str) -> Result<Option<&'a Value>> {
    let mut cursor = target;
    for token in parse_path(path)? {
        cursor = match token {
            PathToken::Field(name) => match cursor.get(&name) {
                Some(next) => next,
                None => return Ok(None),
            },
            PathToken::Index(index) => match cursor.get(index) {
                Some(next) => next,
                None => return Ok(None),
            },
        };
    }
    Ok(Some(cursor))
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

fn node_kind<'a>(ty: &'a Value, context: &str) -> Result<&'a str> {
    kind(ty).ok_or_else(|| anyhow::anyhow!("{context}: type node lacks kind: {ty}"))
}

fn child<'a>(ty: &'a Value, key: &str) -> Result<&'a Value> {
    ty.get(key)
        .ok_or_else(|| anyhow::anyhow!("{} lacks {key}", kind(ty).unwrap_or("type node")))
}

fn default_u8() -> Value {
    nodes::number("u8")
}

fn resolve_definition<'a>(
    definitions: &'a BTreeMap<String, Value>,
    link: &Value,
) -> Result<&'a Value> {
    let name = link
        .get("name")
        .and_then(Value::as_str)
        .context("definedTypeLinkNode lacks name")?;
    definitions
        .get(name)
        .ok_or_else(|| anyhow::anyhow!("unknown defined type {name}"))
}

struct Decoder<'a> {
    bytes: &'a [u8],
    offset: usize,
    definitions: &'a BTreeMap<String, Value>,
    /// Fixed-size strings are zero padded; the padding is not part of the text.
    trim_nulls: bool,
    /// Bytes left unread inside size-bounded regions, which the layout does
    /// not describe and a re-encode would overwrite with zeros.
    unread: AccountDecodeInfo,
}

impl<'a> Decoder<'a> {
    fn new(bytes: &'a [u8], definitions: &'a BTreeMap<String, Value>) -> Self {
        Decoder {
            bytes,
            offset: 0,
            definitions,
            trim_nulls: false,
            unread: AccountDecodeInfo::default(),
        }
    }

    fn take(&mut self, count: usize) -> Result<&'a [u8]> {
        let end = self
            .offset
            .checked_add(count)
            .ok_or_else(|| anyhow::anyhow!("length overflow"))?;
        let value = self
            .bytes
            .get(self.offset..end)
            .ok_or_else(|| anyhow::anyhow!("unexpected end of data"))?;
        self.offset = end;
        Ok(value)
    }

    fn remaining(&mut self) -> &'a [u8] {
        let rest = &self.bytes[self.offset.min(self.bytes.len())..];
        self.offset = self.bytes.len();
        rest
    }

    /// Decodes `ty` from a bounded slice, recording bytes the inner layout leaves unread.
    fn decode_bounded(&mut self, slice: &'a [u8], trim_nulls: bool, ty: &Value) -> Result<Value> {
        let mut inner = Decoder {
            trim_nulls,
            ..Decoder::new(slice, self.definitions)
        };
        let value = inner.decode(ty)?;
        self.unread.merge(inner.unread);
        self.unread.record(&slice[inner.offset.min(slice.len())..]);
        Ok(value)
    }

    fn decode(&mut self, ty: &Value) -> Result<Value> {
        trace!(ty = ?ty, offset = self.offset, "decoding Codama type node");
        let kind = node_kind(ty, "decode")?;
        match kind {
            "numberTypeNode" => self.number(ty),
            "booleanTypeNode" => {
                match self.read_integer(ty.get("size").unwrap_or(&default_u8()))? {
                    0 => Ok(Value::Bool(false)),
                    1 => Ok(Value::Bool(true)),
                    other => bail!("invalid boolean byte {other}"),
                }
            }
            "publicKeyTypeNode" => Ok(Value::String(bs58::encode(self.take(32)?).into_string())),
            "stringTypeNode" => {
                let encoding = ty.get("encoding").and_then(Value::as_str).unwrap_or("utf8");
                let mut bytes = self.remaining();
                if self.trim_nulls && encoding == "utf8" {
                    let end = bytes
                        .iter()
                        .rposition(|byte| *byte != 0)
                        .map_or(0, |i| i + 1);
                    bytes = &bytes[..end];
                }
                Ok(Value::String(encode_to_string(bytes, encoding)?))
            }
            "bytesTypeNode" => Ok(byte_json(self.remaining())),
            "sizePrefixTypeNode" => {
                let length = self.read_length(child(ty, "prefix")?)?;
                let slice = self.take(length)?;
                self.decode_bounded(slice, false, child(ty, "type")?)
            }
            "fixedSizeTypeNode" => {
                let size = child(ty, "size")?
                    .as_u64()
                    .and_then(|size| usize::try_from(size).ok())
                    .context("fixedSizeTypeNode size must be an integer")?;
                let slice = self.take(size)?;
                self.decode_bounded(slice, true, child(ty, "type")?)
            }
            "arrayTypeNode" | "setTypeNode" => {
                let item = child(ty, "item")?;
                let count = self.read_count(child(ty, "count")?)?;
                self.sequence(count, |decoder| decoder.decode(item))
                    .map(Value::Array)
            }
            "mapTypeNode" => {
                let key = child(ty, "key")?;
                let value = child(ty, "value")?;
                let count = self.read_count(child(ty, "count")?)?;
                self.sequence(count, |decoder| {
                    let mut entry = Map::new();
                    entry.insert("key".to_owned(), decoder.decode(key)?);
                    entry.insert("value".to_owned(), decoder.decode(value)?);
                    Ok(Value::Object(entry))
                })
                .map(Value::Array)
            }
            "tupleTypeNode" => child(ty, "items")?
                .as_array()
                .context("tupleTypeNode items must be an array")?
                .iter()
                .map(|item| self.decode(item))
                .collect::<Result<Vec<_>>>()
                .map(Value::Array),
            "structTypeNode" => {
                let mut result = Map::new();
                for field in child(ty, "fields")?
                    .as_array()
                    .context("structTypeNode fields must be an array")?
                {
                    let name = field
                        .get("name")
                        .and_then(Value::as_str)
                        .context("structFieldTypeNode lacks name")?;
                    let value = self
                        .decode(child(field, "type")?)
                        .with_context(|| format!("field {name}"))?;
                    if !field_is_omitted(field) {
                        result.insert(name.to_owned(), value);
                    }
                }
                Ok(Value::Object(result))
            }
            "enumTypeNode" => self.enumeration(ty),
            "optionTypeNode" => {
                let prefix = ty.get("prefix").cloned().unwrap_or_else(default_u8);
                let item = child(ty, "item")?;
                if self.read_integer(&prefix)? == 0 {
                    if ty.get("fixed").and_then(Value::as_bool) == Some(true) {
                        let size = fixed_size_of(item, self.definitions)
                            .context("fixed optionTypeNode item has no fixed size")?;
                        self.take(size)?;
                    }
                    return Ok(Value::Null);
                }
                self.decode(item)
            }
            "zeroableOptionTypeNode" => {
                let item = child(ty, "item")?;
                let size = fixed_size_of(item, self.definitions)
                    .context("zeroableOptionTypeNode item has no fixed size")?;
                let zero = match ty.get("zeroValue") {
                    Some(constant) => encode_constant(constant, self.definitions)?,
                    None => vec![0; size],
                };
                let window = self
                    .bytes
                    .get(self.offset..self.offset + size)
                    .context("unexpected end of data")?;
                if window == zero.as_slice() {
                    self.offset += size;
                    return Ok(Value::Null);
                }
                self.decode(item)
            }
            "remainderOptionTypeNode" => {
                if self.offset >= self.bytes.len() {
                    Ok(Value::Null)
                } else {
                    self.decode(child(ty, "item")?)
                }
            }
            "definedTypeLinkNode" => self.decode(resolve_definition(self.definitions, ty)?),
            "amountTypeNode" | "dateTimeTypeNode" | "solAmountTypeNode" => {
                self.decode(child(ty, "number")?)
            }
            "hiddenPrefixTypeNode" => {
                self.expect_constants(child(ty, "prefix")?)?;
                self.decode(child(ty, "type")?)
            }
            "hiddenSuffixTypeNode" => {
                let value = self.decode(child(ty, "type")?)?;
                self.expect_constants(child(ty, "suffix")?)?;
                Ok(value)
            }
            "preOffsetTypeNode" => {
                let start = self.offset;
                self.apply_offset(ty, start)?;
                self.decode(child(ty, "type")?)
            }
            "postOffsetTypeNode" => {
                let start = self.offset;
                let value = self.decode(child(ty, "type")?)?;
                self.apply_offset(ty, start)?;
                Ok(value)
            }
            "sentinelTypeNode" => {
                let sentinel = encode_constant(child(ty, "sentinel")?, self.definitions)?;
                let rest = &self.bytes[self.offset.min(self.bytes.len())..];
                let position = rest
                    .windows(sentinel.len().max(1))
                    .position(|window| window == sentinel.as_slice())
                    .context("sentinel not found")?;
                let slice = &rest[..position];
                self.offset += position + sentinel.len();
                self.decode_bounded(slice, false, child(ty, "type")?)
            }
            other => bail!("unsupported Codama type node {other}"),
        }
    }

    fn sequence(
        &mut self,
        count: Option<usize>,
        mut item: impl FnMut(&mut Self) -> Result<Value>,
    ) -> Result<Vec<Value>> {
        let mut items = Vec::new();
        match count {
            Some(count) => {
                for _ in 0..count {
                    items.push(item(self)?);
                }
            }
            None => {
                while self.offset < self.bytes.len() {
                    let before = self.offset;
                    items.push(item(self)?);
                    // An item that consumes nothing would repeat forever.
                    if self.offset == before {
                        bail!("remainder item consumed no bytes; the sequence cannot terminate")
                    }
                }
            }
        }
        Ok(items)
    }

    /// Reads a count node, returning `None` for remainder counts.
    fn read_count(&mut self, count: &Value) -> Result<Option<usize>> {
        match node_kind(count, "count")? {
            "fixedCountNode" => Ok(Some(
                child(count, "value")?
                    .as_u64()
                    .and_then(|value| usize::try_from(value).ok())
                    .context("fixedCountNode value must be an integer")?,
            )),
            "prefixedCountNode" => Ok(Some(self.read_length(child(count, "prefix")?)?)),
            "remainderCountNode" => Ok(None),
            other => bail!("unsupported count node {other}"),
        }
    }

    fn read_length(&mut self, prefix: &Value) -> Result<usize> {
        usize::try_from(self.read_integer(prefix)?).context("length does not fit usize")
    }

    /// Reads an integer-valued node, following links and transparent wrappers.
    fn read_integer(&mut self, ty: &Value) -> Result<i128> {
        let ty = nodes::resolve_link(ty, self.definitions);
        match kind(ty) {
            Some("numberTypeNode") => {
                let format = child(ty, "format")?
                    .as_str()
                    .context("numberTypeNode format must be a string")?;
                let big_endian = ty.get("endian").and_then(Value::as_str) == Some("be");
                match format {
                    "shortU16" => self.short_u16().map(i128::from),
                    "f32" | "f64" => bail!("expected an integer, found {format}"),
                    _ => {
                        let (signed, width) = integer_format(format)?;
                        if width > 16 {
                            bail!("{format} is too wide to use as a length or discriminator")
                        }
                        let mut bytes = self.take(width)?.to_vec();
                        if big_endian {
                            bytes.reverse();
                        }
                        Ok(integer_from_le(&bytes, signed))
                    }
                }
            }
            _ => match self.decode(ty)? {
                Value::Number(number) => number.as_i128().context("integer does not fit"),
                Value::String(text) => text.parse().context("integer string is invalid"),
                other => bail!("expected an integer, decoded {other}"),
            },
        }
    }

    fn short_u16(&mut self) -> Result<u16> {
        let mut value = 0_u32;
        for index in 0..3 {
            let byte = self.take(1)?[0];
            value |= u32::from(byte & 0x7f) << (7 * index);
            if byte & 0x80 == 0 {
                break;
            }
        }
        u16::try_from(value).context("shortU16 is out of range")
    }

    fn number(&mut self, ty: &Value) -> Result<Value> {
        let format = child(ty, "format")?
            .as_str()
            .context("numberTypeNode format must be a string")?;
        let big_endian = ty.get("endian").and_then(Value::as_str) == Some("be");
        match format {
            "shortU16" => Ok(Value::Number(self.short_u16()?.into())),
            "f32" | "f64" => {
                let width = if format == "f32" { 4 } else { 8 };
                let mut bytes = self.take(width)?.to_vec();
                if big_endian {
                    bytes.reverse();
                }
                let float = if width == 4 {
                    f64::from(f32::from_le_bytes(
                        bytes.as_slice().try_into().expect("4 bytes"),
                    ))
                } else {
                    f64::from_le_bytes(bytes.as_slice().try_into().expect("8 bytes"))
                };
                Ok(Number::from_f64(float).map_or(Value::Null, Value::Number))
            }
            _ => {
                let (signed, width) = integer_format(format)?;
                let mut bytes = self.take(width)?.to_vec();
                if big_endian {
                    bytes.reverse();
                }
                Ok(integer_json(&bytes, signed))
            }
        }
    }

    fn enumeration(&mut self, ty: &Value) -> Result<Value> {
        let size = ty.get("size").cloned().unwrap_or_else(default_u8);
        let discriminator = self.read_integer(&size)?;
        let variants = child(ty, "variants")?
            .as_array()
            .context("enumTypeNode variants must be an array")?;
        for (index, variant) in variants.iter().enumerate() {
            // Codama defaults a variant's discriminator to its index.
            let value = variant
                .get("discriminator")
                .and_then(Value::as_i64)
                .map_or(index as i128, i128::from);
            if value != discriminator {
                continue;
            }
            let name = variant
                .get("name")
                .and_then(Value::as_str)
                .context("enum variant lacks name")?;
            let mut out = Map::new();
            out.insert("kind".to_owned(), Value::String(name.to_owned()));
            match node_kind(variant, "enum variant")? {
                "enumEmptyVariantTypeNode" => {}
                "enumTupleVariantTypeNode" => {
                    out.insert("fields".to_owned(), self.decode(child(variant, "tuple")?)?);
                }
                "enumStructVariantTypeNode" => {
                    out.insert("fields".to_owned(), self.decode(child(variant, "struct")?)?);
                }
                other => bail!("unsupported enum variant node {other}"),
            }
            return Ok(Value::Object(out));
        }
        bail!("enum discriminator {discriminator} is out of range")
    }

    fn expect_constants(&mut self, constants: &Value) -> Result<()> {
        for constant in constants
            .as_array()
            .context("hidden constants must be an array")?
        {
            let expected = encode_constant(constant, self.definitions)?;
            let actual = self.take(expected.len())?;
            if actual != expected.as_slice() {
                bail!("hidden constant mismatch: expected {expected:?}, found {actual:?}")
            }
        }
        Ok(())
    }

    fn apply_offset(&mut self, ty: &Value, start: usize) -> Result<()> {
        let offset = child(ty, "offset")?
            .as_i64()
            .context("offset must be an integer")?;
        let strategy = ty
            .get("strategy")
            .and_then(Value::as_str)
            .unwrap_or("relative");
        let base = match strategy {
            "relative" | "padded" => i64::try_from(self.offset)?,
            "absolute" => {
                if offset < 0 {
                    i64::try_from(self.bytes.len())?
                } else {
                    0
                }
            }
            "preOffset" => i64::try_from(start)?,
            other => bail!("unsupported offset strategy {other}"),
        };
        let target = base + offset;
        if target < 0 || target as usize > self.bytes.len() {
            bail!("offset {target} is outside the data")
        }
        self.offset = target as usize;
        Ok(())
    }
}

fn byte_json(bytes: &[u8]) -> Value {
    Value::Array(
        bytes
            .iter()
            .map(|byte| Value::Number(Number::from(*byte)))
            .collect(),
    )
}

/// Splits an integer format such as `u64` into its signedness and byte width.
fn integer_format(format: &str) -> Result<(bool, usize)> {
    let width = nodes::number_width(format)
        .ok_or_else(|| anyhow::anyhow!("unsupported number format {format}"))?;
    Ok((format.starts_with('i'), width))
}

fn integer_from_le(bytes: &[u8], signed: bool) -> i128 {
    let mut buffer = [0_u8; 16];
    buffer[..bytes.len()].copy_from_slice(bytes);
    let negative = signed && bytes.last().is_some_and(|byte| byte & 0x80 != 0);
    if negative {
        for byte in &mut buffer[bytes.len()..] {
            *byte = 0xff;
        }
    }
    i128::from_le_bytes(buffer)
}

/// Renders a little-endian integer as JSON: a number up to 32 bits, else a decimal string.
fn integer_json(bytes: &[u8], signed: bool) -> Value {
    if bytes.len() <= 16 {
        let (text, small) = if signed {
            let value = integer_from_le(bytes, true);
            (value.to_string(), Number::from_i128(value))
        } else {
            let mut buffer = [0_u8; 16];
            buffer[..bytes.len()].copy_from_slice(bytes);
            let value = u128::from_le_bytes(buffer);
            (value.to_string(), Number::from_u128(value))
        };
        return if bytes.len() <= 4 {
            Value::Number(small.expect("32-bit integers fit"))
        } else {
            Value::String(text)
        };
    }
    Value::String(wide_integer_to_decimal(bytes, signed))
}

fn wide_integer_to_decimal(bytes: &[u8], signed: bool) -> String {
    let negative = signed && bytes.last().is_some_and(|byte| byte & 0x80 != 0);
    let mut magnitude = bytes.to_vec();
    if negative {
        // Two's complement: invert and add one to obtain the magnitude.
        let mut carry = true;
        for byte in &mut magnitude {
            *byte = !*byte;
            if carry {
                let (sum, overflow) = byte.overflowing_add(1);
                *byte = sum;
                carry = overflow;
            }
        }
    }
    // Base 1e9 limbs, least significant first.
    let mut limbs = vec![0_u64];
    for byte in magnitude.iter().rev() {
        let mut carry = u64::from(*byte);
        for limb in &mut limbs {
            let value = *limb * 256 + carry;
            *limb = value % 1_000_000_000;
            carry = value / 1_000_000_000;
        }
        while carry > 0 {
            limbs.push(carry % 1_000_000_000);
            carry /= 1_000_000_000;
        }
    }
    let mut text = String::new();
    if negative {
        text.push('-');
    }
    text.push_str(&limbs.last().copied().unwrap_or(0).to_string());
    for limb in limbs.iter().rev().skip(1) {
        text.push_str(&format!("{limb:09}"));
    }
    text
}

/// Parses a JSON integer (number or decimal string) into little-endian bytes of `width`.
fn integer_to_le(value: &Value, signed: bool, width: usize, format: &str) -> Result<Vec<u8>> {
    let text = match value {
        Value::Number(number) => number.to_string(),
        Value::String(text) => text.trim().to_owned(),
        _ => bail!("{format} value must be an integer or decimal string"),
    };
    let (negative, digits) = match text.strip_prefix('-') {
        Some(rest) => (true, rest),
        None => (false, text.as_str()),
    };
    if digits.is_empty() || !digits.bytes().all(|byte| byte.is_ascii_digit()) {
        bail!("{format} value {text} is not an integer")
    }
    if negative && !signed {
        bail!("{format} value {text} cannot be negative")
    }
    let mut magnitude = vec![0_u8; width];
    for digit in digits.bytes() {
        let mut carry = u32::from(digit - b'0');
        for byte in &mut magnitude {
            let value = u32::from(*byte) * 10 + carry;
            *byte = (value & 0xff) as u8;
            carry = value >> 8;
        }
        if carry != 0 {
            bail!("{format} value {text} is out of range")
        }
    }
    if signed {
        let top_set = magnitude[width - 1] & 0x80 != 0;
        let is_min = top_set
            && magnitude[width - 1] == 0x80
            && magnitude[..width - 1].iter().all(|byte| *byte == 0);
        if top_set && !(negative && is_min) {
            bail!("{format} value {text} is out of range")
        }
        if negative {
            let mut carry = true;
            for byte in &mut magnitude {
                *byte = !*byte;
                if carry {
                    let (sum, overflow) = byte.overflowing_add(1);
                    *byte = sum;
                    carry = overflow;
                }
            }
        }
    }
    Ok(magnitude)
}

/// Encodes a dynamic value according to one Codama type node.
pub fn encode_type(
    ty: &Value,
    value: &Value,
    definitions: &BTreeMap<String, Value>,
    output: &mut Vec<u8>,
) -> Result<()> {
    let kind = node_kind(ty, "encode")?;
    match kind {
        "numberTypeNode" => encode_number(ty, value, output),
        "booleanTypeNode" => {
            let flag = value
                .as_bool()
                .ok_or_else(|| anyhow::anyhow!("bool value required"))?;
            encode_type(
                ty.get("size").unwrap_or(&default_u8()),
                &Value::Number(u8::from(flag).into()),
                definitions,
                output,
            )
        }
        "publicKeyTypeNode" => {
            let text = value
                .as_str()
                .ok_or_else(|| anyhow::anyhow!("public key must be a base58 string"))?;
            let bytes = bs58::decode(text)
                .into_vec()
                .context("public key is not valid base58")?;
            if bytes.len() != 32 {
                bail!("public key must be 32 bytes")
            }
            output.extend_from_slice(&bytes);
            Ok(())
        }
        "stringTypeNode" => {
            let text = value
                .as_str()
                .ok_or_else(|| anyhow::anyhow!("string value required"))?;
            let encoding = ty.get("encoding").and_then(Value::as_str).unwrap_or("utf8");
            output.extend_from_slice(&nodes::decode_encoded_string(text, encoding)?);
            Ok(())
        }
        "bytesTypeNode" => {
            output.extend_from_slice(&byte_array(value)?);
            Ok(())
        }
        "sizePrefixTypeNode" => {
            let mut inner = Vec::new();
            encode_type(child(ty, "type")?, value, definitions, &mut inner)?;
            encode_type(
                child(ty, "prefix")?,
                &Value::Number(inner.len().into()),
                definitions,
                output,
            )?;
            output.extend_from_slice(&inner);
            Ok(())
        }
        "fixedSizeTypeNode" => {
            let size = child(ty, "size")?
                .as_u64()
                .and_then(|size| usize::try_from(size).ok())
                .context("fixedSizeTypeNode size must be an integer")?;
            let mut inner = Vec::new();
            encode_type(child(ty, "type")?, value, definitions, &mut inner)?;
            if inner.len() > size {
                bail!(
                    "value needs {} bytes but the layout fixes {size}",
                    inner.len()
                )
            }
            inner.resize(size, 0);
            output.extend_from_slice(&inner);
            Ok(())
        }
        "arrayTypeNode" | "setTypeNode" => {
            let items = value
                .as_array()
                .ok_or_else(|| anyhow::anyhow!("array value must be an array"))?;
            encode_count(child(ty, "count")?, items.len(), definitions, output)?;
            let item = child(ty, "item")?;
            for entry in items {
                encode_type(item, entry, definitions, output)?;
            }
            Ok(())
        }
        "mapTypeNode" => {
            let entries = value.as_array().ok_or_else(|| {
                anyhow::anyhow!("map value must be an array of {{key, value}} entries")
            })?;
            encode_count(child(ty, "count")?, entries.len(), definitions, output)?;
            for entry in entries {
                encode_type(
                    child(ty, "key")?,
                    entry.get("key").context("map entry lacks key")?,
                    definitions,
                    output,
                )?;
                encode_type(
                    child(ty, "value")?,
                    entry.get("value").context("map entry lacks value")?,
                    definitions,
                    output,
                )?;
            }
            Ok(())
        }
        "tupleTypeNode" => {
            let items = child(ty, "items")?
                .as_array()
                .context("tupleTypeNode items must be an array")?;
            let values = value
                .as_array()
                .ok_or_else(|| anyhow::anyhow!("tuple value must be an array"))?;
            if values.len() != items.len() {
                bail!(
                    "tuple value has {} items but the layout has {}",
                    values.len(),
                    items.len()
                )
            }
            for (item, entry) in items.iter().zip(values) {
                encode_type(item, entry, definitions, output)?;
            }
            Ok(())
        }
        "structTypeNode" => {
            let values = value
                .as_object()
                .ok_or_else(|| anyhow::anyhow!("struct value must be an object"))?;
            for field in child(ty, "fields")?
                .as_array()
                .context("structTypeNode fields must be an array")?
            {
                let name = field
                    .get("name")
                    .and_then(Value::as_str)
                    .context("structFieldTypeNode lacks name")?;
                let supplied = values.get(name);
                let default;
                let field_value = match (supplied, field.get("defaultValue")) {
                    (Some(value), _) if !field_is_omitted(field) => value,
                    (_, Some(default_node)) => {
                        default = value_node_to_json(default_node)?;
                        &default
                    }
                    (Some(value), None) => value,
                    (None, None) => bail!("missing field {name}"),
                };
                encode_type(child(field, "type")?, field_value, definitions, output)
                    .with_context(|| format!("field {name}"))?;
            }
            Ok(())
        }
        "enumTypeNode" => encode_enum(ty, value, definitions, output),
        "optionTypeNode" => {
            let prefix = ty.get("prefix").cloned().unwrap_or_else(default_u8);
            let item = child(ty, "item")?;
            if value.is_null() {
                encode_type(&prefix, &Value::Number(0.into()), definitions, output)?;
                if ty.get("fixed").and_then(Value::as_bool) == Some(true) {
                    let size = fixed_size_of(item, definitions)
                        .context("fixed optionTypeNode item has no fixed size")?;
                    output.extend(std::iter::repeat_n(0, size));
                }
                return Ok(());
            }
            encode_type(&prefix, &Value::Number(1.into()), definitions, output)?;
            encode_type(item, value, definitions, output)
        }
        "zeroableOptionTypeNode" => {
            let item = child(ty, "item")?;
            if value.is_null() {
                let zero = match ty.get("zeroValue") {
                    Some(constant) => encode_constant(constant, definitions)?,
                    None => vec![
                        0;
                        fixed_size_of(item, definitions)
                            .context("zeroableOptionTypeNode item has no fixed size")?
                    ],
                };
                output.extend_from_slice(&zero);
                return Ok(());
            }
            encode_type(item, value, definitions, output)
        }
        "remainderOptionTypeNode" => {
            if value.is_null() {
                return Ok(());
            }
            encode_type(child(ty, "item")?, value, definitions, output)
        }
        "definedTypeLinkNode" => encode_type(
            resolve_definition(definitions, ty)?,
            value,
            definitions,
            output,
        ),
        "amountTypeNode" | "dateTimeTypeNode" | "solAmountTypeNode" => {
            encode_type(child(ty, "number")?, value, definitions, output)
        }
        "hiddenPrefixTypeNode" => {
            for constant in child(ty, "prefix")?
                .as_array()
                .context("hiddenPrefixTypeNode prefix must be an array")?
            {
                output.extend_from_slice(&encode_constant(constant, definitions)?);
            }
            encode_type(child(ty, "type")?, value, definitions, output)
        }
        "hiddenSuffixTypeNode" => {
            encode_type(child(ty, "type")?, value, definitions, output)?;
            for constant in child(ty, "suffix")?
                .as_array()
                .context("hiddenSuffixTypeNode suffix must be an array")?
            {
                output.extend_from_slice(&encode_constant(constant, definitions)?);
            }
            Ok(())
        }
        "preOffsetTypeNode" => {
            encode_padding(ty, 0, output)?;
            encode_type(child(ty, "type")?, value, definitions, output)
        }
        "postOffsetTypeNode" => {
            let start = output.len();
            encode_type(child(ty, "type")?, value, definitions, output)?;
            encode_padding(ty, output.len() - start, output)
        }
        "sentinelTypeNode" => {
            let sentinel = encode_constant(child(ty, "sentinel")?, definitions)?;
            let mut inner = Vec::new();
            encode_type(child(ty, "type")?, value, definitions, &mut inner)?;
            // The decoder stops at the first sentinel, so one inside the value
            // would truncate it and shift everything that follows.
            if !sentinel.is_empty()
                && inner
                    .windows(sentinel.len())
                    .any(|window| window == sentinel.as_slice())
            {
                bail!("value contains the sentinel bytes {sentinel:?} that delimit it")
            }
            output.extend_from_slice(&inner);
            output.extend_from_slice(&sentinel);
            Ok(())
        }
        other => bail!("unsupported Codama type node {other}"),
    }
}

/// Writes the zero padding an offset node adds when encoding sequentially.
///
/// Only forward movement can be expressed in a sequential writer, so
/// backwards and absolute offsets are rejected.
fn encode_padding(ty: &Value, written: usize, output: &mut Vec<u8>) -> Result<()> {
    let offset = child(ty, "offset")?
        .as_i64()
        .context("offset must be an integer")?;
    let strategy = ty
        .get("strategy")
        .and_then(Value::as_str)
        .unwrap_or("relative");
    let padding = match strategy {
        "padded" | "relative" if offset >= 0 => offset as usize,
        "preOffset" if offset >= 0 && offset as usize >= written => offset as usize - written,
        _ => bail!("cannot encode a {strategy} offset of {offset} sequentially"),
    };
    output.extend(std::iter::repeat_n(0, padding));
    Ok(())
}

fn encode_count(
    count: &Value,
    length: usize,
    definitions: &BTreeMap<String, Value>,
    output: &mut Vec<u8>,
) -> Result<()> {
    match node_kind(count, "count")? {
        "fixedCountNode" => {
            let expected = child(count, "value")?
                .as_u64()
                .context("fixedCountNode value must be an integer")?;
            if expected != length as u64 {
                bail!("value has {length} items but the layout fixes {expected}")
            }
            Ok(())
        }
        "prefixedCountNode" => encode_type(
            child(count, "prefix")?,
            &Value::Number(length.into()),
            definitions,
            output,
        ),
        "remainderCountNode" => Ok(()),
        other => bail!("unsupported count node {other}"),
    }
}

fn encode_number(ty: &Value, value: &Value, output: &mut Vec<u8>) -> Result<()> {
    let format = child(ty, "format")?
        .as_str()
        .context("numberTypeNode format must be a string")?;
    let big_endian = ty.get("endian").and_then(Value::as_str) == Some("be");
    match format {
        "shortU16" => {
            let mut remaining = u16::try_from(
                value
                    .as_u64()
                    .or_else(|| value.as_str().and_then(|text| text.parse().ok()))
                    .context("shortU16 value must be an integer")?,
            )
            .context("shortU16 value is out of range")?;
            loop {
                let byte = (remaining & 0x7f) as u8;
                remaining >>= 7;
                if remaining == 0 {
                    output.push(byte);
                    break;
                }
                output.push(byte | 0x80);
            }
            Ok(())
        }
        "f32" | "f64" => {
            let float = value
                .as_f64()
                .or_else(|| value.as_str().and_then(|text| text.parse().ok()))
                .context("float value must be a number")?;
            let mut bytes = if format == "f32" {
                (float as f32).to_le_bytes().to_vec()
            } else {
                float.to_le_bytes().to_vec()
            };
            if big_endian {
                bytes.reverse();
            }
            output.extend_from_slice(&bytes);
            Ok(())
        }
        _ => {
            let (signed, width) = integer_format(format)?;
            let mut bytes = integer_to_le(value, signed, width, format)?;
            if big_endian {
                bytes.reverse();
            }
            output.extend_from_slice(&bytes);
            Ok(())
        }
    }
}

fn encode_enum(
    ty: &Value,
    value: &Value,
    definitions: &BTreeMap<String, Value>,
    output: &mut Vec<u8>,
) -> Result<()> {
    let (variant_name, payload) = match value {
        Value::String(name) => (name.as_str(), None),
        Value::Object(object) => (
            object
                .get("kind")
                .and_then(Value::as_str)
                .context("enum value lacks kind")?,
            object.get("fields"),
        ),
        _ => bail!("enum value must be a variant name or an object with kind and optional fields"),
    };
    let size = ty.get("size").cloned().unwrap_or_else(default_u8);
    for (index, variant) in child(ty, "variants")?
        .as_array()
        .context("enumTypeNode variants must be an array")?
        .iter()
        .enumerate()
    {
        // Codama defaults a variant's discriminator to its index.
        let discriminator = variant
            .get("discriminator")
            .and_then(Value::as_i64)
            .unwrap_or(index as i64);
        if variant.get("name").and_then(Value::as_str) != Some(variant_name) {
            continue;
        }
        encode_type(
            &size,
            &Value::Number(discriminator.into()),
            definitions,
            output,
        )?;
        return match node_kind(variant, "enum variant")? {
            "enumEmptyVariantTypeNode" => Ok(()),
            "enumTupleVariantTypeNode" => encode_type(
                child(variant, "tuple")?,
                payload.context("enum variant requires fields")?,
                definitions,
                output,
            ),
            "enumStructVariantTypeNode" => encode_type(
                child(variant, "struct")?,
                payload.context("enum variant requires fields")?,
                definitions,
                output,
            ),
            other => bail!("unsupported enum variant node {other}"),
        };
    }
    bail!("unknown enum variant {variant_name}")
}

fn byte_array(value: &Value) -> Result<Vec<u8>> {
    if let Some(node) = value.as_object()
        && kind(value) == Some("bytesValueNode")
        && node.contains_key("data")
    {
        return decode_bytes_value(value);
    }
    value
        .as_array()
        .ok_or_else(|| anyhow::anyhow!("byte value must be an array of integers"))?
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
    use crate::nodes::{
        array, boolean, bytes_value, constant_value, defined_link, enum_node, fixed_count,
        fixed_size, number, option, prefixed_count, public_key, size_prefix, string, struct_field,
        struct_node,
    };
    use serde_json::json;

    fn anchor_like_account() -> AccountSchema {
        let mut discriminator = struct_field("discriminator", fixed_size(nodes::bytes(), 8));
        discriminator["defaultValue"] = bytes_value(&[1; 8]);
        discriminator["defaultValueStrategy"] = json!("omitted");
        AccountSchema {
            name: "State".into(),
            discriminators: vec![Discriminator::Constant {
                offset: 0,
                bytes: vec![1; 8],
                inferred: false,
            }],
            ty: struct_node(vec![
                discriminator,
                struct_field("count", number("u64")),
                struct_field("enabled", boolean()),
            ]),
        }
    }

    #[test]
    fn decodes_patches_and_reencodes() {
        let account = anchor_like_account();
        let raw = [vec![1; 8], 7u64.to_le_bytes().to_vec(), vec![1]].concat();
        let mut value = decode_account(&account, &BTreeMap::new(), &raw).unwrap();
        assert_eq!(value, json!({"count": "7", "enabled": true}));
        patch_value(&mut value, "count", json!(9)).unwrap();
        assert_eq!(
            encode_account(&account, &BTreeMap::new(), &value).unwrap(),
            [vec![1; 8], 9u64.to_le_bytes().to_vec(), vec![1]].concat()
        );
    }

    #[test]
    fn accepts_zero_filled_account_allocation_padding() {
        let account = anchor_like_account();
        let raw = [
            vec![1; 8],
            7u64.to_le_bytes().to_vec(),
            vec![1],
            vec![0; 16],
        ]
        .concat();
        let (value, info) = decode_account_with_info(&account, &BTreeMap::new(), &raw).unwrap();
        assert_eq!(value["count"], "7");
        assert!(!info.has_unmodelled_data());
        assert_eq!(info.trailing_byte_count, 16);
    }

    #[test]
    fn reports_non_zero_trailing_account_data() {
        let account = anchor_like_account();
        let raw = [
            vec![1; 8],
            7u64.to_le_bytes().to_vec(),
            vec![1],
            vec![0, 1, 0],
        ]
        .concat();
        let (_, info) = decode_account_with_info(&account, &BTreeMap::new(), &raw).unwrap();
        assert_eq!(info.trailing_byte_count, 3);
        assert_eq!(info.non_zero_trailing_byte_count, 1);
        assert!(info.has_unmodelled_data());
    }

    #[test]
    fn rejects_data_with_the_wrong_discriminator() {
        let account = anchor_like_account();
        let raw = [vec![2; 8], 7u64.to_le_bytes().to_vec(), vec![1]].concat();
        assert!(decode_account(&account, &BTreeMap::new(), &raw).is_err());
    }

    #[test]
    fn round_trips_codama_specific_nodes() {
        let mut definitions = BTreeMap::new();
        definitions.insert(
            "status".to_owned(),
            enum_node(
                vec![
                    json!({"kind": "enumEmptyVariantTypeNode", "name": "idle", "discriminator": 5}),
                    json!({"kind": "enumTupleVariantTypeNode", "name": "busy", "tuple": nodes::tuple(vec![number("u16")])}),
                    json!({"kind": "enumStructVariantTypeNode", "name": "done", "struct": struct_node(vec![struct_field("at", number("i64"))])}),
                ],
                number("u16"),
            ),
        );
        let ty = struct_node(vec![
            struct_field("label", fixed_size(string("utf8"), 8)),
            struct_field("note", size_prefix(string("utf8"), number("u16"))),
            struct_field(
                "big",
                json!({"kind": "numberTypeNode", "format": "u32", "endian": "be"}),
            ),
            struct_field("signed", number("i16")),
            struct_field(
                "owner",
                json!({"kind": "zeroableOptionTypeNode", "item": public_key()}),
            ),
            struct_field("maybe", option(number("u8"))),
            struct_field("tags", array(number("u8"), prefixed_count(number("u8")))),
            struct_field("pair", array(boolean(), fixed_count(2))),
            struct_field("status", defined_link("status")),
            struct_field("busy", defined_link("status")),
            struct_field("done", defined_link("status")),
            struct_field("wide", number("u128")),
            struct_field(
                "tagged",
                json!({"kind": "hiddenPrefixTypeNode", "type": number("u8"), "prefix": [constant_value(number("u8"), json!({"kind": "numberValueNode", "number": 0xAB}))]}),
            ),
            struct_field(
                "rest",
                json!({"kind": "remainderOptionTypeNode", "item": number("u8")}),
            ),
        ]);
        let value = json!({
            "label": "hi",
            "note": "hello",
            "big": 258,
            "signed": -2,
            "owner": null,
            "maybe": 4,
            "tags": [1, 2, 3],
            "pair": [true, false],
            "status": {"kind": "idle"},
            "busy": {"kind": "busy", "fields": [513]},
            "done": {"kind": "done", "fields": {"at": "-5"}},
            "wide": "340282366920938463463374607431768211455",
            "tagged": 9,
            "rest": null,
        });
        let mut bytes = Vec::new();
        encode_type(&ty, &value, &definitions, &mut bytes).unwrap();
        assert_eq!(&bytes[..8], b"hi\0\0\0\0\0\0");
        assert_eq!(&bytes[8..15], [5, 0, b'h', b'e', b'l', b'l', b'o']);
        assert_eq!(&bytes[15..19], [0, 0, 1, 2]);
        assert_eq!(&bytes[19..21], [0xfe, 0xff]);
        let mut decoder = Decoder::new(&bytes, &definitions);
        assert_eq!(decoder.decode(&ty).unwrap(), value);
        assert_eq!(decoder.offset, bytes.len());
    }

    #[test]
    fn renders_and_parses_256_bit_integers() {
        let ty = number("i256");
        let mut bytes = Vec::new();
        encode_type(&ty, &json!("-1"), &BTreeMap::new(), &mut bytes).unwrap();
        assert_eq!(bytes, vec![0xff; 32]);
        let definitions = BTreeMap::new();
        assert_eq!(
            Decoder::new(&bytes, &definitions).decode(&ty).unwrap(),
            json!("-1")
        );
        let max = "57896044618658097711785492504343953926634992332820282019728792003956564819967";
        let mut bytes = Vec::new();
        encode_type(&ty, &json!(max), &BTreeMap::new(), &mut bytes).unwrap();
        assert_eq!(
            Decoder::new(&bytes, &definitions).decode(&ty).unwrap(),
            json!(max)
        );
    }

    fn single_field_account(name: &str, ty: Value) -> AccountSchema {
        AccountSchema {
            name: name.into(),
            discriminators: vec![],
            ty: struct_node(vec![struct_field("value", ty)]),
        }
    }

    /// Bytes left unread inside a bounded region used to be dropped silently,
    /// so `[7, 42, 0, 0]` decoded as clean and re-encoded as `[7, 0, 0, 0]`.
    #[test]
    fn reports_non_zero_bytes_left_inside_bounded_regions() {
        let definitions = BTreeMap::new();
        let fixed = single_field_account("fixed", fixed_size(number("u8"), 4));
        let (value, info) = decode_account_with_info(&fixed, &definitions, &[7, 42, 0, 0]).unwrap();
        assert_eq!(value["value"], 7);
        assert_eq!(info.trailing_byte_count, 3);
        assert_eq!(info.non_zero_trailing_byte_count, 1);
        let (_, info) = decode_account_with_info(&fixed, &definitions, &[7, 0, 0, 0]).unwrap();
        assert!(!info.has_unmodelled_data());

        let prefixed = single_field_account("prefixed", size_prefix(number("u8"), number("u8")));
        let (_, info) = decode_account_with_info(&prefixed, &definitions, &[2, 7, 1]).unwrap();
        assert!(info.has_unmodelled_data());
    }

    #[test]
    fn rejects_remainder_items_that_consume_no_bytes() {
        let definitions = BTreeMap::new();
        let ty = array(nodes::tuple(vec![]), json!({"kind": "remainderCountNode"}));
        assert!(Decoder::new(&[1], &definitions).decode(&ty).is_err());
        assert_eq!(
            Decoder::new(&[], &definitions).decode(&ty).unwrap(),
            json!([])
        );
    }

    /// Codama assigns an unspecified variant discriminator from the variant's
    /// index, not from the previous variant's explicit value.
    #[test]
    fn defaults_enum_discriminators_to_variant_indices() {
        let definitions = BTreeMap::new();
        let ty = enum_node(
            vec![
                json!({"kind": "enumEmptyVariantTypeNode", "name": "first", "discriminator": 5}),
                json!({"kind": "enumEmptyVariantTypeNode", "name": "second"}),
            ],
            number("u8"),
        );
        assert_eq!(
            Decoder::new(&[1], &definitions).decode(&ty).unwrap(),
            json!({"kind": "second"})
        );
        assert!(Decoder::new(&[6], &definitions).decode(&ty).is_err());
        let mut bytes = Vec::new();
        encode_type(&ty, &json!("second"), &definitions, &mut bytes).unwrap();
        assert_eq!(bytes, vec![1]);
    }

    #[test]
    fn rejects_values_that_contain_their_sentinel() {
        let definitions = BTreeMap::new();
        let ty = json!({
            "kind": "sentinelTypeNode",
            "type": string("utf8"),
            "sentinel": constant_value(number("u8"), json!({"kind": "numberValueNode", "number": 0})),
        });
        let mut bytes = Vec::new();
        assert!(encode_type(&ty, &json!("a\0b"), &definitions, &mut bytes).is_err());
        encode_type(&ty, &json!("ab"), &definitions, &mut bytes).unwrap();
        assert_eq!(bytes, b"ab\0");
    }
}
