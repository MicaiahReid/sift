//! IDL loading, Borsh-compatible layouts, RPC access, and unsigned transaction building.

pub mod accounts;
pub mod codec;
pub mod discovery;
pub mod idl;
pub mod program_metadata;
pub mod query;
pub mod registry;
pub mod resolve;
pub mod rpc;
pub mod summary;
pub mod transaction;

pub use accounts::{
    DecodedAccount, DecodedAccounts, decode_accounts, detect_layout, discriminator_filter,
    fetch_decoded_accounts,
};
pub use codec::{
    AccountDecodeInfo, decode_account, decode_account_with_info, encode_account, lookup_path,
    parse_cli_value, patch_value,
};
pub use discovery::{LocalIdlCandidate, discover_local_idls};
pub use idl::{find_account, find_instruction, parse_idl};
pub use program_metadata::{CanonicalIdl, discover_canonical_idl, load_canonical_idl};
pub use query::{Aggregate, AggregateOp, Condition, Operator};
pub use registry::{LoadedSchema, Registry, default_registry_path};
pub use resolve::{AutoResolvedSchema, resolve_schema_auto};
pub use rpc::{RpcAccount, RpcClient};
pub use transaction::{BuildOptions, BuiltTransaction, TransactionVersion, build_transaction};
