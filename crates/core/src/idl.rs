//! IDL document detection and parsing into the Codama-shaped [`ProgramSchema`].

use anyhow::{Context, Result, bail};
use serde_json::Value;
use sift_types::{AccountSchema, InstructionSchema, ProgramSchema};
use tracing::trace;

use crate::{anchor::parse_anchor, codama::parse_codama};

/// The dialect an IDL document is written in.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum IdlFormat {
    /// A legacy or 0.30+ Anchor IDL.
    Anchor,
    /// A Codama `rootNode` or `programNode` document.
    Codama,
}

impl IdlFormat {
    /// Human-readable dialect name.
    pub fn label(self) -> &'static str {
        match self {
            IdlFormat::Anchor => "Anchor",
            IdlFormat::Codama => "Codama",
        }
    }
}

/// Identifies the dialect of a parsed IDL document, or `None` for unrelated JSON.
pub fn detect_format(root: &Value) -> Option<IdlFormat> {
    match root.get("kind").and_then(Value::as_str) {
        Some("rootNode" | "programNode") => Some(IdlFormat::Codama),
        _ => (root.get("instructions").is_some_and(Value::is_array)
            || root.get("accounts").is_some_and(Value::is_array))
        .then_some(IdlFormat::Anchor),
    }
}

/// Returns the program address an IDL document declares for itself, if any.
pub fn declared_program_address(root: &Value) -> Option<String> {
    match detect_format(root)? {
        IdlFormat::Codama => root
            .pointer("/program/publicKey")
            .or_else(|| root.get("publicKey")),
        IdlFormat::Anchor => root
            .get("address")
            .or_else(|| root.pointer("/metadata/address")),
    }
    .and_then(Value::as_str)
    .map(ToOwned::to_owned)
}

/// Parses an Anchor (legacy or current) or Codama IDL into a schema.
pub fn parse_idl(input: &str, program_address: &str) -> Result<ProgramSchema> {
    trace!(
        program_address,
        byte_length = input.len(),
        "parsing IDL document"
    );
    let root: Value = serde_json::from_str(input).context("IDL is not valid JSON")?;
    match detect_format(&root) {
        Some(IdlFormat::Codama) => parse_codama(&root, program_address),
        Some(IdlFormat::Anchor) => parse_anchor(&root, program_address),
        None => bail!("unsupported IDL: expected a Codama rootNode or an Anchor document"),
    }
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
    use super::*;
    use serde_json::json;

    #[test]
    fn detects_dialects_and_declared_addresses() {
        let codama =
            json!({"kind": "rootNode", "program": {"kind": "programNode", "publicKey": "A"}});
        assert_eq!(detect_format(&codama), Some(IdlFormat::Codama));
        assert_eq!(declared_program_address(&codama).as_deref(), Some("A"));
        let anchor = json!({"address": "B", "instructions": []});
        assert_eq!(detect_format(&anchor), Some(IdlFormat::Anchor));
        assert_eq!(declared_program_address(&anchor).as_deref(), Some("B"));
        assert_eq!(detect_format(&json!({"idl": "x"})), None);
    }

    #[test]
    fn parses_a_codama_root_into_codama_types() {
        let input = r#"{"kind":"rootNode","program":{"kind":"programNode","name":"demo","publicKey":"11111111111111111111111111111111","accounts":[{"kind":"accountNode","name":"state","data":{"kind":"structTypeNode","fields":[{"kind":"structFieldTypeNode","name":"count","type":{"kind":"numberTypeNode","format":"u64","endian":"le"}}]}}],"instructions":[{"kind":"instructionNode","name":"increment","arguments":[{"kind":"instructionArgumentNode","name":"amount","type":{"kind":"numberTypeNode","format":"u64","endian":"le"}}],"accounts":[{"kind":"instructionAccountNode","name":"state","isSigner":false,"isWritable":true}]}],"definedTypes":[]}}"#;
        let schema = parse_idl(input, "11111111111111111111111111111111").unwrap();
        assert_eq!(schema.name, "demo");
        assert_eq!(schema.accounts.len(), 1);
        assert_eq!(schema.accounts[0].ty["kind"], "structTypeNode");
        assert!(schema.instructions[0].accounts[0].writable);
    }

    #[test]
    fn parses_an_anchor_document_into_codama_types() {
        let input = r#"{"name":"demo","accounts":[{"name":"State","type":{"kind":"struct","fields":[{"name":"count","type":"u64"}]}}],"instructions":[]}"#;
        let schema = parse_idl(input, "11111111111111111111111111111111").unwrap();
        assert_eq!(
            schema.accounts[0].ty["fields"][1]["type"]["kind"],
            "numberTypeNode"
        );
        assert_eq!(schema.accounts[0].discriminators.len(), 1);
    }
}
