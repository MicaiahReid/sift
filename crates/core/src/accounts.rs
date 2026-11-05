//! Discovery and decoding of IDL-typed program accounts.

use anyhow::Result;
use serde_json::{Value, json};
use sift_types::{AccountSchema, ProgramSchema};
use tracing::debug;

use crate::{RpcClient, codec::decode_account_with_info};

/// One program account decoded against an IDL account layout.
#[derive(Clone, Debug, PartialEq)]
pub struct DecodedAccount {
    /// Account address.
    pub address: String,
    /// IDL account name that matched the account's discriminator.
    pub account_type: String,
    /// Decoded field values.
    pub data: Value,
    /// A non-fatal limitation encountered while decoding this account.
    pub note: Option<String>,
}

/// Decode outcome for a batch of accounts, including how many did not decode.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct DecodedAccounts {
    /// Accounts whose bytes decoded successfully.
    pub accounts: Vec<DecodedAccount>,
    /// Addresses that carried the discriminator but failed to decode.
    pub undecodable: Vec<String>,
}

/// Builds the `getProgramAccounts` memcmp filter that selects one account type.
pub fn discriminator_filter(layout: &AccountSchema) -> Value {
    json!([{ "memcmp": { "offset": 0, "bytes": bs58::encode(&layout.discriminator).into_string() }}])
}

/// Decodes raw account bytes against one layout, keeping failures as addresses.
pub fn decode_accounts(
    accounts: impl IntoIterator<Item = (String, Vec<u8>)>,
    layout: &AccountSchema,
    schema: &ProgramSchema,
) -> DecodedAccounts {
    let mut result = DecodedAccounts::default();
    for (address, bytes) in accounts {
        match decode_account_with_info(layout, &schema.types, &bytes) {
            Ok((data, info)) => result.accounts.push(DecodedAccount {
                address,
                account_type: layout.name.clone(),
                data,
                note: info.has_unmodelled_data().then(|| {
                    format!(
                        "Parsed with {} unmodelled trailing byte(s) ({} non-zero); the IDL does not describe this data.",
                        info.trailing_byte_count, info.non_zero_trailing_byte_count,
                    )
                }),
            }),
            Err(error) => {
                debug!(account = %layout.name, address, byte_length = bytes.len(), error = %error, "failed to decode account data");
                result.undecodable.push(address);
            }
        }
    }
    result
}

/// Fetches and decodes every account of one type owned by the schema's program.
pub async fn fetch_decoded_accounts(
    rpc: &RpcClient,
    schema: &ProgramSchema,
    layout: &AccountSchema,
) -> Result<DecodedAccounts> {
    let accounts = rpc
        .program_accounts(&schema.address, discriminator_filter(layout))
        .await?;
    Ok(decode_accounts(accounts, layout, schema))
}

/// Selects the account layout for raw bytes, honoring an explicit type name when given.
pub fn detect_layout<'a>(
    schema: &'a ProgramSchema,
    bytes: &[u8],
    requested: Option<&str>,
) -> Result<&'a AccountSchema> {
    if let Some(name) = requested {
        return crate::find_account(schema, name);
    }
    schema
        .accounts
        .iter()
        .find(|layout| bytes.starts_with(&layout.discriminator))
        .ok_or_else(|| {
            let known = schema
                .accounts
                .iter()
                .map(|layout| layout.name.as_str())
                .collect::<Vec<_>>()
                .join(", ");
            anyhow::anyhow!(
                "no registered account discriminator matches the first bytes; known account types: {known}"
            )
        })
}
