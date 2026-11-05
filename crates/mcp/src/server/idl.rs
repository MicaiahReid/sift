//! IDL registry tools.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::{Value, json};
use sift_core::{
    discover_canonical_idl,
    summary::{definition_with_dependencies, schema_summary},
};
use sift_types::IdlSource;

use super::{SiftServer, source_json, source_location};

/// Input for `idl_add`.
#[derive(Debug, Deserialize, JsonSchema)]
pub struct IdlAddParams {
    /// Program base58 address. The program does not need to be registered yet.
    pub program: String,
    /// Path to a local JSON IDL, absolute or relative to the server's working directory.
    #[serde(default)]
    pub file: Option<String>,
    /// HTTP(S) URL of a JSON IDL document.
    #[serde(default)]
    pub url: Option<String>,
    /// Register the canonical on-chain Program Metadata IDL instead of a file or URL.
    #[serde(default)]
    pub onchain: Option<bool>,
    /// Optional friendly alias to register alongside the address.
    #[serde(default)]
    pub alias: Option<String>,
    /// RPC URL or cluster shorthand (`mainnet`, `devnet`, `testnet`, `localhost`) overriding the server default.
    #[serde(default)]
    pub rpc_url: Option<String>,
}

/// Input for `idl_alias`.
#[derive(Debug, Deserialize, JsonSchema)]
pub struct IdlAliasParams {
    /// Registered program alias or base58 address.
    pub program: String,
    /// New alias to add.
    pub alias: String,
}

/// Input for `idl_prefer`.
#[derive(Debug, Deserialize, JsonSchema)]
pub struct IdlPreferParams {
    /// Registered program alias or base58 address.
    pub program: String,
    /// Source ID from `idl_list`, for example `onchain-1` or `local-1`.
    pub source_id: String,
}

/// Input for `idl_remove`.
#[derive(Debug, Deserialize, JsonSchema)]
pub struct IdlRemoveParams {
    /// Registered program alias or base58 address.
    pub program: String,
}

/// Level of detail returned by `idl_show`.
#[derive(Clone, Copy, Debug, Default, Deserialize, JsonSchema, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum ShowDetail {
    /// Compact overview: field names with short type signatures.
    #[default]
    Summary,
    /// The complete normalized schema JSON. Large; prefer `definition`.
    Full,
}

/// Input for `idl_show`.
#[derive(Debug, Deserialize, JsonSchema)]
pub struct IdlShowParams {
    /// Program alias or base58 address.
    pub program: String,
    /// `summary` (default) or `full`.
    #[serde(default)]
    pub detail: ShowDetail,
    /// Name of one account, instruction, or type to expand in full with its referenced types. Case-insensitive.
    #[serde(default)]
    pub definition: Option<String>,
    /// RPC URL or cluster shorthand overriding the server default; only used to load on-chain IDLs.
    #[serde(default)]
    pub rpc_url: Option<String>,
}

pub(super) async fn list(server: &SiftServer) -> Result<Value> {
    let registry = server.registry().lock().await;
    let programs = registry
        .config
        .programs
        .iter()
        .map(|(address, entry)| {
            let active = entry.priority.first().map(String::as_str);
            let sources = entry
                .priority
                .iter()
                .filter_map(|id| {
                    entry
                        .sources
                        .get(id)
                        .map(|source| source_json(id, source, Some(id.as_str()) == active))
                })
                .collect::<Vec<_>>();
            json!({
                "address": address,
                "aliases": entry.aliases,
                "sources": sources,
            })
        })
        .collect::<Vec<_>>();
    Ok(json!({
        "registry_path": registry.path.display().to_string(),
        "programs": programs,
    }))
}

pub(super) async fn add(server: &SiftServer, params: IdlAddParams) -> Result<Value> {
    sift_core::registry::validate_address(&params.program)
        .with_context(|| format!("program {} is not a valid base58 address", params.program))?;
    let rpc = server.rpc(params.rpc_url.as_deref(), None)?;
    let source = match (
        params.file.as_deref(),
        params.url.as_deref(),
        params.onchain.unwrap_or(false),
    ) {
        (Some(file), None, false) => {
            let path = absolute_path(file, server.options().workdir.as_deref());
            if !path.is_file() {
                bail!(
                    "IDL file {} does not exist; paths are resolved relative to {}",
                    path.display(),
                    server
                        .options()
                        .workdir
                        .as_deref()
                        .map(|dir| dir.display().to_string())
                        .unwrap_or_else(|| "the server's working directory".to_owned())
                );
            }
            IdlSource::File {
                name: file_name(&path),
                path,
            }
        }
        (None, Some(url), false) => {
            let url: url::Url = url
                .parse()
                .with_context(|| format!("`url` {url} is not a valid URL"))?;
            IdlSource::Url {
                name: url.to_string(),
                url,
            }
        }
        (None, None, true) => {
            let idl = discover_canonical_idl(&rpc, &params.program)
                .await
                .context("no canonical on-chain IDL was found for this program")?;
            IdlSource::OnChain {
                name: "canonical-onchain".to_owned(),
                address: idl.address,
            }
        }
        (None, None, false) => bail!("provide exactly one of `file`, `url`, or `onchain: true`"),
        _ => bail!("`file`, `url`, and `onchain` are mutually exclusive; provide only one"),
    };
    let mut registry = server.registry().lock().await;
    // Validate against a copy so a broken IDL never reaches the saved registry.
    let mut trial = registry.clone();
    trial.add_source(&params.program, source.clone(), params.alias.clone())?;
    let loaded = trial
        .load_schema_with_source(&params.program, None, &rpc)
        .await
        .context("the IDL could not be loaded or parsed, so it was not registered")?;
    registry.add_source(&params.program, source.clone(), params.alias.clone())?;
    registry.save().await?;
    let entry = registry
        .config
        .programs
        .get(&params.program)
        .expect("program was just registered");
    let source_id = entry.priority.first().cloned().unwrap_or_default();
    Ok(json!({
        "program": params.program,
        "aliases": entry.aliases,
        "source": source_json(&source_id, &source, true),
        "schema": {
            "name": loaded.schema.name,
            "accounts": loaded.schema.accounts.iter().map(|account| account.name.clone()).collect::<Vec<_>>(),
            "instruction_count": loaded.schema.instructions.len(),
        },
    }))
}

pub(super) async fn alias(server: &SiftServer, params: IdlAliasParams) -> Result<Value> {
    let mut registry = server.registry().lock().await;
    registry.add_alias(&params.program, &params.alias)?;
    registry.save().await?;
    let address = registry.program_address(&params.program)?;
    let aliases = registry
        .config
        .programs
        .get(&address)
        .map(|entry| entry.aliases.clone())
        .unwrap_or_default();
    Ok(json!({ "program": address, "aliases": aliases }))
}

pub(super) async fn prefer(server: &SiftServer, params: IdlPreferParams) -> Result<Value> {
    let mut registry = server.registry().lock().await;
    let address = registry.program_address(&params.program)?;
    if let Err(error) = registry.prefer_source(&address, &params.source_id) {
        let known = registry
            .config
            .programs
            .get(&address)
            .map(|entry| entry.sources.keys().cloned().collect::<Vec<_>>())
            .unwrap_or_default();
        return Err(error.context(format!("known source IDs for {address}: {known:?}")));
    }
    registry.save().await?;
    let entry = registry
        .config
        .programs
        .get(&address)
        .expect("program exists after prefer");
    Ok(json!({
        "program": address,
        "priority": entry.priority,
    }))
}

pub(super) async fn remove(server: &SiftServer, params: IdlRemoveParams) -> Result<Value> {
    let mut registry = server.registry().lock().await;
    let address = registry.program_address(&params.program)?;
    let Some(entry) = registry.config.programs.remove(&address) else {
        bail!("program {} is not registered", params.program)
    };
    registry.save().await?;
    Ok(json!({
        "removed": address,
        "aliases": entry.aliases,
        "sources": entry.sources.keys().collect::<Vec<_>>(),
    }))
}

pub(super) async fn show(server: &SiftServer, params: IdlShowParams) -> Result<Value> {
    let rpc = server.rpc(params.rpc_url.as_deref(), None)?;
    let resolved = server.schema(&params.program, &rpc).await?;
    let schema = &resolved.resolved.schema;
    let body = if let Some(name) = params.definition.as_deref() {
        let Some(found) = definition_with_dependencies(schema, name) else {
            let accounts = schema
                .accounts
                .iter()
                .map(|account| account.name.as_str())
                .collect::<Vec<_>>();
            let instructions = schema
                .instructions
                .iter()
                .map(|instruction| instruction.name.as_str())
                .collect::<Vec<_>>();
            let types = schema.types.keys().collect::<Vec<_>>();
            bail!(
                "{} has no account, instruction, or type named {name}; accounts: {accounts:?}; instructions: {instructions:?}; types: {types:?}",
                schema.name
            )
        };
        let mut found = found;
        if let Some(object) = found.as_object_mut() {
            object.insert("program".to_owned(), json!(schema.address));
            object.insert(
                "idl_source".to_owned(),
                json!(source_location(&resolved.resolved.source)),
            );
        }
        found
    } else {
        match params.detail {
            ShowDetail::Summary => {
                let mut summary = schema_summary(schema);
                if let Some(object) = summary.as_object_mut() {
                    object.insert("idl_source".to_owned(), json!(resolved.location()));
                }
                summary
            }
            ShowDetail::Full => json!({
                "idl_source": resolved.location(),
                "schema": schema,
            }),
        }
    };
    Ok(resolved.annotate(body))
}

fn absolute_path(file: &str, workdir: Option<&Path>) -> PathBuf {
    let path = PathBuf::from(file);
    if path.is_absolute() {
        return path;
    }
    match workdir {
        Some(dir) => dir.join(path),
        None => std::env::current_dir()
            .map(|dir| dir.join(&path))
            .unwrap_or(path),
    }
}

fn file_name(path: &Path) -> String {
    path.file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| path.display().to_string())
}
