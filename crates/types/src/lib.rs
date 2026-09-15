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
    /// Sources in highest-to-lowest priority order.
    #[serde(default)]
    pub sources: Vec<IdlSource>,
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
    /// The program's canonical on-chain IDL, resolved through PMP then Anchor.
    OnChain {
        /// Stable display name for this source.
        name: String,
        /// Optional explicitly supplied IDL account address.
        address: Option<String>,
    },
}

/// A normalized representation of one usable program IDL.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ProgramSchema {
    /// Friendly IDL program name, if one was declared.
    pub name: String,
    /// Program base58 address supplied by the caller or IDL metadata.
    pub address: String,
    /// Account layouts selectable by account discriminator and name.
    #[serde(default)]
    pub accounts: Vec<AccountSchema>,
    /// Instruction layouts selectable by instruction name.
    #[serde(default)]
    pub instructions: Vec<InstructionSchema>,
    /// Named layout definitions referenced by accounts and instructions.
    #[serde(default)]
    pub types: BTreeMap<String, serde_json::Value>,
}

/// One account's discriminator and Borsh-compatible layout.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct AccountSchema {
    /// Name used by `sift account list TYPE` and `--type TYPE`.
    pub name: String,
    /// Prefix that uniquely identifies this account type, usually eight bytes.
    pub discriminator: Vec<u8>,
    /// Anchor/Codama-compatible type expression describing account bytes after the prefix.
    pub ty: serde_json::Value,
}

/// One instruction's data and account metadata contract.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct InstructionSchema {
    /// Name used by `sift transaction build INSTRUCTION`.
    pub name: String,
    /// Prefix written before encoded argument values.
    pub discriminator: Vec<u8>,
    /// Positional/named argument definitions.
    #[serde(default)]
    pub args: Vec<FieldSchema>,
    /// Flat account metas required by the instruction.
    #[serde(default)]
    pub accounts: Vec<InstructionAccountSchema>,
}

/// A named field in a struct or instruction argument list.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct FieldSchema {
    /// Exact IDL field name.
    pub name: String,
    /// Borsh-compatible type expression for the field.
    pub ty: serde_json::Value,
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
    /// Optional PDA recipe represented as IDL JSON for proof-only resolution.
    #[serde(default)]
    pub pda: Option<serde_json::Value>,
}
