//! Discovery and decoding of IDL-typed program accounts.

use anyhow::Result;
use serde_json::{Value, json};
use sift_types::{AccountSchema, Discriminator, ProgramSchema};
use tracing::debug;

use crate::{
    RpcClient,
    codec::{decode_account_with_info, matches_discriminators},
};

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
    /// Accounts fetched without any discriminator filter that did not decode
    /// as this layout, and so are presumed to be other account types.
    pub unmatched: usize,
}

/// Builds the `getProgramAccounts` filters that select one account type.
///
/// Constant discriminators become `memcmp` filters and size discriminators
/// become `dataSize` filters. A layout without discriminators yields an empty
/// list, meaning every program account is fetched and matched by decoding.
pub fn account_filters(layout: &AccountSchema) -> Value {
    Value::Array(
        layout
            .discriminators
            .iter()
            .map(|discriminator| match discriminator {
                Discriminator::Constant { offset, bytes, .. } => json!({
                    "memcmp": { "offset": offset, "bytes": bs58::encode(bytes).into_string() }
                }),
                Discriminator::Size { size } => json!({ "dataSize": size }),
            })
            .collect(),
    )
}

/// Decodes raw account bytes against one layout, keeping failures as addresses.
///
/// When the layout has no discriminators the input is every account the
/// program owns, so accounts that do not decode cleanly are counted in
/// [`DecodedAccounts::unmatched`] rather than reported as undecodable.
pub fn decode_accounts(
    accounts: impl IntoIterator<Item = (String, Vec<u8>)>,
    layout: &AccountSchema,
    schema: &ProgramSchema,
) -> DecodedAccounts {
    let filtered = !layout.discriminators.is_empty();
    let mut result = DecodedAccounts::default();
    for (address, bytes) in accounts {
        match decode_account_with_info(layout, &schema.types, &bytes) {
            Ok((_, info)) if !filtered && info.has_unmodelled_data() => result.unmatched += 1,
            Ok((data, info)) => result.accounts.push(DecodedAccount {
                address,
                account_type: layout.name.clone(),
                data,
                note: info.has_unmodelled_data().then(|| {
                    format!(
                        "Parsed with {} unmodelled byte(s) ({} non-zero); the IDL does not describe this data.",
                        info.trailing_byte_count, info.non_zero_trailing_byte_count,
                    )
                }),
            }),
            Err(error) if !filtered => {
                debug!(account = %layout.name, address, error = %error, "account did not decode as the requested layout");
                result.unmatched += 1;
            }
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
        .program_accounts(&schema.address, account_filters(layout))
        .await?;
    Ok(decode_accounts(accounts, layout, schema))
}

/// Selects the account layout for raw bytes, honoring an explicit type name when given.
///
/// Layouts with discriminators are matched by their byte patterns. Layouts
/// without discriminators are matched by decoding the data; a match requires a
/// clean decode with no unmodelled trailing bytes and must be unambiguous.
pub fn detect_layout<'a>(
    schema: &'a ProgramSchema,
    bytes: &[u8],
    requested: Option<&str>,
) -> Result<&'a AccountSchema> {
    if let Some(name) = requested {
        return crate::find_account(schema, name);
    }
    if let Some(layout) = schema.accounts.iter().find(|layout| {
        !layout.discriminators.is_empty() && matches_discriminators(&layout.discriminators, bytes)
    }) {
        return Ok(layout);
    }
    let candidates = schema
        .accounts
        .iter()
        .filter(|layout| layout.discriminators.is_empty())
        .filter(|layout| {
            decode_account_with_info(layout, &schema.types, bytes)
                .is_ok_and(|(_, info)| !info.has_unmodelled_data())
        })
        .collect::<Vec<_>>();
    let known = || {
        schema
            .accounts
            .iter()
            .map(|layout| layout.name.as_str())
            .collect::<Vec<_>>()
            .join(", ")
    };
    match candidates.as_slice() {
        [layout] => Ok(layout),
        [] => Err(anyhow::anyhow!(
            "no registered account discriminator matches the first bytes; known account types: {}",
            known()
        )),
        many => Err(anyhow::anyhow!(
            "the data decodes as several account types without discriminators ({}); pass the type explicitly",
            many.iter()
                .map(|layout| layout.name.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::nodes::{number, public_key, struct_field, struct_node};

    fn schema() -> ProgramSchema {
        ProgramSchema {
            name: "demo".into(),
            address: "11111111111111111111111111111111".into(),
            accounts: vec![
                AccountSchema {
                    name: "tagged".into(),
                    discriminators: vec![Discriminator::Constant {
                        offset: 0,
                        bytes: vec![7],
                        inferred: false,
                    }],
                    ty: struct_node(vec![struct_field("tag", number("u8"))]),
                },
                AccountSchema {
                    name: "wide".into(),
                    discriminators: vec![],
                    ty: struct_node(vec![struct_field("key", public_key())]),
                },
                AccountSchema {
                    name: "narrow".into(),
                    discriminators: vec![],
                    ty: struct_node(vec![struct_field("count", number("u16"))]),
                },
            ],
            instructions: vec![],
            types: Default::default(),
            pdas: vec![],
            errors: vec![],
        }
    }

    #[test]
    fn builds_memcmp_and_data_size_filters() {
        let layout = AccountSchema {
            name: "x".into(),
            discriminators: vec![
                Discriminator::Constant {
                    offset: 4,
                    bytes: vec![1, 2],
                    inferred: true,
                },
                Discriminator::Size { size: 6 },
            ],
            ty: struct_node(vec![]),
        };
        assert_eq!(
            account_filters(&layout),
            json!([{"memcmp": {"offset": 4, "bytes": "5T"}}, {"dataSize": 6}])
        );
        assert_eq!(account_filters(&schema().accounts[1]), json!([]));
    }

    #[test]
    fn detects_layouts_by_discriminator_then_by_clean_decode() {
        let schema = schema();
        assert_eq!(detect_layout(&schema, &[7], None).unwrap().name, "tagged");
        assert_eq!(detect_layout(&schema, &[1; 32], None).unwrap().name, "wide");
        assert_eq!(
            detect_layout(&schema, &[1, 0], None).unwrap().name,
            "narrow"
        );
        assert!(detect_layout(&schema, &[1, 2, 3], None).is_err());
    }

    #[test]
    fn unfiltered_listings_drop_other_account_types_quietly() {
        let schema = schema();
        let result = decode_accounts(
            vec![("a".to_owned(), vec![1; 32]), ("b".to_owned(), vec![9; 40])],
            &schema.accounts[1],
            &schema,
        );
        assert_eq!(result.accounts.len(), 1);
        assert_eq!(result.unmatched, 1);
        assert!(result.undecodable.is_empty());
    }
}
