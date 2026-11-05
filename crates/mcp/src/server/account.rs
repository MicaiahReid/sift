//! Account listing, single-account decoding, and offline patching tools.

use std::collections::BTreeMap;

use anyhow::{Context, Result, bail};
use base64::Engine;
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::{Map, Value, json};
use sift_core::{
    Aggregate, AggregateOp, Condition, Operator, decode_account_with_info, detect_layout,
    encode_account, find_account, patch_value,
    query::{aggregate, matches, project, sort_by_path},
};

use super::SiftServer;

const DEFAULT_LIMIT: usize = 25;
const MAX_LIMIT: usize = 200;

/// Comparison operator for a `where` condition.
#[derive(Clone, Copy, Debug, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ConditionOperator {
    /// Equal to `value`.
    Eq,
    /// Not equal to `value`.
    Ne,
    /// Greater than `value` (numeric strings compare as numbers).
    Gt,
    /// Greater than or equal to `value`.
    Gte,
    /// Less than `value`.
    Lt,
    /// Less than or equal to `value`.
    Lte,
    /// String contains `value` as a substring, or array contains `value` as an element.
    Contains,
    /// Equal to any element of the array `value`.
    In,
    /// Field is present and non-null (`value: true`, default) or absent (`value: false`).
    Exists,
}

impl From<ConditionOperator> for Operator {
    fn from(value: ConditionOperator) -> Self {
        match value {
            ConditionOperator::Eq => Operator::Eq,
            ConditionOperator::Ne => Operator::Ne,
            ConditionOperator::Gt => Operator::Gt,
            ConditionOperator::Gte => Operator::Gte,
            ConditionOperator::Lt => Operator::Lt,
            ConditionOperator::Lte => Operator::Lte,
            ConditionOperator::Contains => Operator::Contains,
            ConditionOperator::In => Operator::In,
            ConditionOperator::Exists => Operator::Exists,
        }
    }
}

/// One server-side filter over decoded account fields.
#[derive(Clone, Debug, Deserialize, JsonSchema)]
pub struct WhereCondition {
    /// Field path such as `author`, `status`, `config.fee`, or `votes[0]`. Enum paths compare their variant name when `value` is a string.
    pub path: String,
    /// Comparison operator.
    pub op: ConditionOperator,
    /// Operand. Pass numbers as JSON numbers or numeric strings; `in` takes an array.
    #[serde(default)]
    pub value: Value,
}

impl From<WhereCondition> for Condition {
    fn from(value: WhereCondition) -> Self {
        Condition {
            path: value.path,
            op: value.op.into(),
            value: value.value,
        }
    }
}

/// Reduction operator for an `aggregate` entry.
#[derive(Clone, Copy, Debug, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum AggregateOperator {
    /// Exact sum of numeric values (wide integers stay exact).
    Sum,
    /// Smallest value with the address holding it.
    Min,
    /// Largest value with the address holding it.
    Max,
    /// Arithmetic mean.
    Avg,
    /// Number of accounts where the path is present and non-null.
    Count,
}

impl From<AggregateOperator> for AggregateOp {
    fn from(value: AggregateOperator) -> Self {
        match value {
            AggregateOperator::Sum => AggregateOp::Sum,
            AggregateOperator::Min => AggregateOp::Min,
            AggregateOperator::Max => AggregateOp::Max,
            AggregateOperator::Avg => AggregateOp::Avg,
            AggregateOperator::Count => AggregateOp::Count,
        }
    }
}

/// One server-side reduction over all accounts matching `where`.
#[derive(Clone, Debug, Deserialize, JsonSchema)]
pub struct AggregateSpec {
    /// Reduction to apply.
    pub op: AggregateOperator,
    /// Field path to reduce, for example `for_votes_lamports`.
    pub path: String,
}

impl From<AggregateSpec> for Aggregate {
    fn from(value: AggregateSpec) -> Self {
        Aggregate {
            op: value.op.into(),
            path: value.path,
        }
    }
}

/// Input for `account_list`.
#[derive(Debug, Deserialize, JsonSchema)]
pub struct AccountListParams {
    /// Program alias or base58 address.
    pub program: String,
    /// IDL account type name, for example `Proposal`. Case-insensitive. See `idl_show`.
    pub account_type: String,
    /// Conditions that every returned account must satisfy. All conditions are ANDed.
    #[serde(default, rename = "where")]
    pub conditions: Vec<WhereCondition>,
    /// Field paths to return instead of the whole decoded account, for example `["title", "status", "for_votes_bp"]`. The response keys each value by its path.
    #[serde(default)]
    pub fields: Option<Vec<String>>,
    /// Reductions computed over every account matching `where` (not just the returned page), for example `[{"op": "sum", "path": "stake"}, {"op": "max", "path": "stake"}]`. `min`/`max` return the value, the `address` of the account holding it, and that account's `fields` when `fields` is set. Combine with `limit: 0` to get totals without any rows.
    #[serde(default)]
    pub aggregate: Vec<AggregateSpec>,
    /// Field path to order by before paging. Missing fields sort last.
    #[serde(default)]
    pub sort_by: Option<String>,
    /// Sort in descending order. Defaults to false.
    #[serde(default)]
    pub descending: Option<bool>,
    /// Maximum accounts to return. Defaults to 25, capped at 200. Use 0 to only count matches.
    #[serde(default)]
    pub limit: Option<usize>,
    /// Number of matching accounts to skip. Defaults to 0; use `next_offset` from a previous response.
    #[serde(default)]
    pub offset: Option<usize>,
    /// Bypass the 60-second decoded-account cache and refetch from the RPC.
    #[serde(default)]
    pub refresh: Option<bool>,
    /// RPC URL or cluster shorthand (`mainnet`, `devnet`, `testnet`, `localhost`) overriding the server default.
    #[serde(default)]
    pub rpc_url: Option<String>,
    /// RPC commitment (`processed`, `confirmed`, `finalized`) overriding the server default.
    #[serde(default)]
    pub commitment: Option<String>,
}

/// Input for `account_get`.
#[derive(Debug, Deserialize, JsonSchema)]
pub struct AccountGetParams {
    /// Base58 address of the account.
    pub address: String,
    /// Program alias or address whose IDL decodes the account. Defaults to the account's owner.
    #[serde(default)]
    pub program: Option<String>,
    /// IDL account type name. Defaults to discriminator detection.
    #[serde(default)]
    pub account_type: Option<String>,
    /// Field paths to return instead of the whole decoded account.
    #[serde(default)]
    pub fields: Option<Vec<String>>,
    /// RPC URL or cluster shorthand overriding the server default.
    #[serde(default)]
    pub rpc_url: Option<String>,
    /// RPC commitment overriding the server default.
    #[serde(default)]
    pub commitment: Option<String>,
}

/// Output encoding for patched bytes.
#[derive(Clone, Copy, Debug, Default, Deserialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum BytesEncoding {
    /// Standard padded base64.
    #[default]
    Base64,
    /// Base58.
    Base58,
    /// Lowercase hexadecimal.
    Hex,
}

/// Input for `account_patch`.
#[derive(Debug, Deserialize, JsonSchema)]
pub struct AccountPatchParams {
    /// Base58 address of the account to fetch and patch.
    pub address: String,
    /// Program alias or base58 address whose IDL describes the account.
    pub program: String,
    /// IDL account type name. Defaults to discriminator detection.
    #[serde(default)]
    pub account_type: Option<String>,
    /// Map of field path to replacement JSON value, for example `{"authority": "<base58>", "count": "42"}`. Integers wider than 32 bits should be strings.
    pub set: BTreeMap<String, Value>,
    /// Encoding of the returned bytes. Defaults to base64.
    #[serde(default)]
    pub encoding: BytesEncoding,
    /// RPC URL or cluster shorthand overriding the server default.
    #[serde(default)]
    pub rpc_url: Option<String>,
    /// RPC commitment overriding the server default.
    #[serde(default)]
    pub commitment: Option<String>,
}

pub(super) async fn list(server: &SiftServer, params: AccountListParams) -> Result<Value> {
    let limit = params.limit.unwrap_or(DEFAULT_LIMIT).min(MAX_LIMIT);
    let offset = params.offset.unwrap_or(0);
    let rpc = server.rpc(params.rpc_url.as_deref(), params.commitment.as_deref())?;
    let resolved = server.schema(&params.program, &rpc).await?;
    let schema = &resolved.resolved.schema;
    let layout = find_account(schema, &params.account_type).with_context(|| {
        format!(
            "known account types for {}: {:?}",
            schema.name,
            schema
                .accounts
                .iter()
                .map(|account| account.name.as_str())
                .collect::<Vec<_>>()
        )
    })?;
    let (decoded, cache_age) = server
        .decoded_accounts(&rpc, schema, layout, params.refresh.unwrap_or(false))
        .await?;
    let total = decoded.accounts.len();
    let conditions = params
        .conditions
        .into_iter()
        .map(Condition::from)
        .collect::<Vec<_>>();
    let mut matched = Vec::with_capacity(total);
    for account in decoded.accounts {
        if matches(&account.data, &conditions)? {
            matched.push(account);
        }
    }
    if let Some(sort_by) = params.sort_by.as_deref() {
        sort_by_path(
            &mut matched,
            sort_by,
            params.descending.unwrap_or(false),
            |account| &account.data,
        );
    }
    let matched_count = matched.len();
    let aggregates = params
        .aggregate
        .into_iter()
        .map(Aggregate::from)
        .collect::<Vec<_>>();
    let aggregated = if aggregates.is_empty() {
        None
    } else {
        let mut reduced = aggregate(
            matched
                .iter()
                .map(|account| (account.address.as_str(), &account.data)),
            &aggregates,
        )?;
        // Extremes name the account they came from; attaching the requested
        // fields saves a follow-up lookup and makes clear the key is an address.
        for value in reduced.values_mut() {
            let Some(object) = value.as_object_mut() else {
                continue;
            };
            let Some(Value::String(address)) = object.remove("key") else {
                continue;
            };
            if let Some(fields) = params.fields.as_deref()
                && let Some(account) = matched.iter().find(|account| account.address == address)
            {
                object.insert("fields".to_owned(), project(&account.data, fields)?);
            }
            object.insert("address".to_owned(), Value::String(address));
        }
        Some(reduced)
    };
    let page = matched.into_iter().skip(offset).take(limit);
    let mut accounts = Vec::new();
    let mut noted = 0_usize;
    for account in page {
        let data = match params.fields.as_deref() {
            Some(fields) => project(&account.data, fields)?,
            None => account.data,
        };
        let mut entry = Map::new();
        entry.insert("address".to_owned(), json!(account.address));
        entry.insert("data".to_owned(), data);
        // The same limitation repeats across most accounts of a type, so it is
        // reported once at the top level with a per-account marker only.
        if account.note.is_some() {
            noted += 1;
            entry.insert("trailing_data".to_owned(), json!(true));
        }
        accounts.push(Value::Object(entry));
    }
    let returned = accounts.len();
    let next_offset = (offset + returned < matched_count && limit > 0).then(|| offset + returned);
    let mut response = json!({
        "program": schema.address,
        "account_type": layout.name,
        "idl_source": resolved.location(),
        "rpc_url": rpc.url,
        "total": total,
        "matched": matched_count,
        "offset": offset,
        "returned": returned,
        "next_offset": next_offset,
        "accounts": accounts,
    });
    let object = response.as_object_mut().expect("response is an object");
    if let Some(aggregated) = aggregated {
        object.insert("aggregates".to_owned(), Value::Object(aggregated));
    }
    if noted > 0 {
        object.insert(
            "note".to_owned(),
            json!(format!(
                "{noted} returned account(s) marked `trailing_data` carry bytes after the IDL layout that the IDL does not describe; the decoded fields are still valid."
            )),
        );
    }
    if !decoded.undecodable.is_empty() {
        object.insert(
            "undecodable".to_owned(),
            json!({
                "count": decoded.undecodable.len(),
                "sample": decoded.undecodable.iter().take(5).collect::<Vec<_>>(),
                "note": "These accounts carry the discriminator but did not decode against the IDL layout.",
            }),
        );
    }
    if cache_age > std::time::Duration::ZERO {
        object.insert("cache_age_secs".to_owned(), json!(cache_age.as_secs()));
    }
    Ok(resolved.annotate(response))
}

pub(super) async fn get(server: &SiftServer, params: AccountGetParams) -> Result<Value> {
    sift_core::registry::validate_address(&params.address)
        .with_context(|| format!("{} is not a valid base58 address", params.address))?;
    let rpc = server.rpc(params.rpc_url.as_deref(), params.commitment.as_deref())?;
    let account = rpc
        .account(&params.address)
        .await?
        .with_context(|| format!("account {} does not exist on {}", params.address, rpc.url))?;
    let program = params.program.as_deref().unwrap_or(&account.owner);
    let resolved = server.schema(program, &rpc).await?;
    let schema = &resolved.resolved.schema;
    if schema.address != account.owner {
        bail!(
            "account {} is owned by {}, not by {} ({}); pass `program` explicitly only when the owner's IDL is wrong",
            params.address,
            account.owner,
            schema.name,
            schema.address
        )
    }
    let layout = detect_layout(schema, &account.data, params.account_type.as_deref())?;
    let (data, info) = decode_account_with_info(layout, &schema.types, &account.data)?;
    let data = match params.fields.as_deref() {
        Some(fields) => project(&data, fields)?,
        None => data,
    };
    let mut response = json!({
        "address": params.address,
        "owner": account.owner,
        "program_name": schema.name,
        "account_type": layout.name,
        "idl_source": resolved.location(),
        "data_length": account.data.len(),
        "data": data,
    });
    if info.has_unmodelled_data()
        && let Some(object) = response.as_object_mut()
    {
        object.insert(
            "note".to_owned(),
            json!(format!(
                "Parsed with {} unmodelled trailing byte(s) ({} non-zero); the IDL does not describe this data.",
                info.trailing_byte_count, info.non_zero_trailing_byte_count
            )),
        );
    }
    Ok(resolved.annotate(response))
}

pub(super) async fn patch(server: &SiftServer, params: AccountPatchParams) -> Result<Value> {
    if params.set.is_empty() {
        bail!("`set` must contain at least one field path to replace")
    }
    let rpc = server.rpc(params.rpc_url.as_deref(), params.commitment.as_deref())?;
    let resolved = server.schema(&params.program, &rpc).await?;
    let schema = &resolved.resolved.schema;
    let bytes = rpc
        .account_data(&params.address)
        .await?
        .with_context(|| format!("account {} does not exist on {}", params.address, rpc.url))?;
    let layout = detect_layout(schema, &bytes, params.account_type.as_deref())?;
    let (mut value, info) = decode_account_with_info(layout, &schema.types, &bytes)?;
    if info.has_unmodelled_data() {
        bail!(
            "cannot patch {}: the IDL does not describe {} trailing byte(s), {} of which are non-zero; patching would discard them",
            layout.name,
            info.trailing_byte_count,
            info.non_zero_trailing_byte_count
        )
    }
    for (path, replacement) in params.set {
        patch_value(&mut value, &path, replacement)
            .with_context(|| format!("cannot apply replacement at {path}"))?;
    }
    let encoded = encode_account(layout, &schema.types, &value)?;
    let (encoding, text) = match params.encoding {
        BytesEncoding::Base64 => (
            "base64",
            base64::engine::general_purpose::STANDARD.encode(&encoded),
        ),
        BytesEncoding::Base58 => ("base58", bs58::encode(&encoded).into_string()),
        BytesEncoding::Hex => (
            "hex",
            encoded.iter().map(|byte| format!("{byte:02x}")).collect(),
        ),
    };
    Ok(resolved.annotate(json!({
        "address": params.address,
        "account_type": layout.name,
        "length": encoded.len(),
        "encoding": encoding,
        "bytes": text,
        "data": value,
    })))
}
