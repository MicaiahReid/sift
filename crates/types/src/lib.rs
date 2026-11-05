//! Shared, serializable contracts used by the CLI and core implementation.

use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;

use serde::{Deserialize, Serialize};
use url::Url;

/// User-maintained registry of IDL sources indexed by program address.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct RegistryConfig {
    /// Format version used to make future migrations explicit.
    #[serde(default = "default_schema_version")]
    pub schema_version: u32,
    /// Configured programs keyed by their canonical base58 address.
    #[serde(default)]
    pub programs: BTreeMap<String, ProgramConfig>,
}

fn default_schema_version() -> u32 {
    1
}

/// IDL source preferences for a single deployed program.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProgramConfig {
    /// Alternative local names accepted anywhere a program address is accepted.
    #[serde(default)]
    pub aliases: BTreeSet<String>,
    /// Sources indexed by stable identifiers used by `priority`.
    #[serde(default)]
    pub sources: BTreeMap<String, IdlSource>,
    /// Source identifiers in highest-to-lowest priority order.
    #[serde(default)]
    pub priority: Vec<String>,
}

/// A resolvable location holding an IDL document.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "kebab-case")]
pub enum IdlSource {
    /// A JSON file that can be read from the local filesystem.
    File {
        /// Stable display name for this source.
        name: String,
        /// Absolute or config-relative local JSON file path.
        path: PathBuf,
    },
    /// A JSON document retrieved through HTTP(S).
    Url {
        /// Stable display name for this source.
        name: String,
        /// HTTP(S) location of the JSON document.
        url: Url,
    },
    /// A verified canonical Program Metadata IDL account.
    OnChain {
        /// Stable display name for this source.
        name: String,
        /// Verified Program Metadata account address.
        address: String,
    },
}

/// The dialect an IDL document is written in.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
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

/// A normalized representation of one usable program IDL.
///
/// Every type expression (`ty`, `types`, argument types) is a Codama type node
/// such as `{"kind":"numberTypeNode","format":"u64","endian":"le"}`. Anchor
/// IDLs are converted into this dialect when parsed, so consumers only need to
/// understand Codama nodes.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ProgramSchema {
    /// Friendly IDL program name, if one was declared.
    pub name: String,
    /// Dialect of the IDL document this schema was parsed from.
    pub format: IdlFormat,
    /// Program base58 address supplied by the caller or IDL metadata.
    pub address: String,
    /// Account layouts selectable by account discriminator and name.
    #[serde(default)]
    pub accounts: Vec<AccountSchema>,
    /// Instruction layouts selectable by instruction name.
    #[serde(default)]
    pub instructions: Vec<InstructionSchema>,
    /// Named Codama type nodes referenced by `definedTypeLinkNode`s.
    #[serde(default)]
    pub types: BTreeMap<String, serde_json::Value>,
    /// Codama `pdaNode`s declared by the program, referenced by `pdaLinkNode`s.
    #[serde(default)]
    pub pdas: Vec<serde_json::Value>,
    /// Program error codes and messages.
    #[serde(default)]
    pub errors: Vec<ErrorSchema>,
}

/// A byte pattern that identifies one account or instruction layout.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Discriminator {
    /// Fixed bytes that must appear at `offset`.
    Constant {
        /// Byte offset of the pattern within the account or instruction data.
        offset: usize,
        /// Expected bytes.
        bytes: Vec<u8>,
        /// Whether sift derived this pattern heuristically because the IDL did
        /// not declare one, by matching the account name against a
        /// discriminator enum stored in the layout's leading field.
        #[serde(default, skip_serializing_if = "std::ops::Not::not")]
        inferred: bool,
    },
    /// The data must be exactly `size` bytes long.
    Size {
        /// Required data length.
        size: usize,
    },
}

/// One account's discriminators and Codama layout.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct AccountSchema {
    /// Name used by `sift account list TYPE` and `--type TYPE`.
    pub name: String,
    /// Patterns that identify this account type; empty when the IDL declares none.
    #[serde(default)]
    pub discriminators: Vec<Discriminator>,
    /// Codama type node describing the complete account data, discriminator included.
    pub ty: serde_json::Value,
}

/// One instruction's data and account metadata contract.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct InstructionSchema {
    /// Name used by `sift transaction build INSTRUCTION`.
    pub name: String,
    /// Patterns that identify this instruction in instruction data.
    #[serde(default)]
    pub discriminators: Vec<Discriminator>,
    /// Instruction data fields in encoding order, discriminator arguments included.
    #[serde(default)]
    pub args: Vec<FieldSchema>,
    /// Flat account metas required by the instruction.
    #[serde(default)]
    pub accounts: Vec<InstructionAccountSchema>,
    /// How omitted optional accounts are represented in the compiled instruction.
    #[serde(default)]
    pub optional_account_strategy: OptionalAccountStrategy,
}

/// Codama's representation of an optional account that the caller left out.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum OptionalAccountStrategy {
    /// The account is skipped entirely.
    Omitted,
    /// The program's own address stands in for the account.
    #[default]
    ProgramId,
}

/// A named field in a struct or instruction argument list.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct FieldSchema {
    /// Exact IDL field name.
    pub name: String,
    /// Codama type node for the field.
    pub ty: serde_json::Value,
    /// Codama value node used when the caller supplies no value.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default_value: Option<serde_json::Value>,
    /// Whether callers never supply this field; its default is always encoded.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub omitted: bool,
}

/// Account metadata required by an instruction.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct InstructionAccountSchema {
    /// Exact IDL account name.
    pub name: String,
    /// Whether the runtime requires an account signature.
    #[serde(default)]
    pub signer: bool,
    /// Whether the instruction is allowed to modify this account.
    #[serde(default)]
    pub writable: bool,
    /// Whether the caller may leave this account out.
    #[serde(default)]
    pub optional: bool,
    /// Codama contextual value node describing how the address is derived when
    /// the caller does not supply one, such as a `pdaValueNode` or a fixed
    /// `publicKeyValueNode`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default_value: Option<serde_json::Value>,
}

impl InstructionAccountSchema {
    /// Whether this account's default address is a program-derived address.
    pub fn is_pda(&self) -> bool {
        self.default_value
            .as_ref()
            .and_then(|value| value.get("kind"))
            .and_then(serde_json::Value::as_str)
            == Some("pdaValueNode")
    }
}

/// One program error.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ErrorSchema {
    /// Numeric error code returned by the program.
    pub code: u32,
    /// Error identifier.
    pub name: String,
    /// Human-readable message, possibly empty.
    #[serde(default)]
    pub message: String,
}
