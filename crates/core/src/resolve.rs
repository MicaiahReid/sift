//! Non-interactive schema resolution with automatic on-chain IDL registration.

use std::path::Path;

use anyhow::{Context, Result};
use sift_types::{IdlSource, ProgramSchema};
use tracing::{debug, warn};

use crate::{Registry, RpcClient, discover_canonical_idl, discover_local_idls, parse_idl};

/// A schema plus how it was obtained.
#[derive(Clone, Debug)]
pub struct AutoResolvedSchema {
    /// Parsed program schema.
    pub schema: ProgramSchema,
    /// Source that produced the schema.
    pub source: IdlSource,
    /// Whether this call added the source to the registry.
    pub newly_registered: bool,
}

/// Loads a schema from the registry, falling back to discovery without prompting.
///
/// Resolution order:
/// 1. An explicit IDL path, when given, is used and never registered.
/// 2. Registered sources, in the program's priority order.
/// 3. The canonical on-chain Program Metadata IDL, which is registered and saved.
/// 4. A single local Anchor or Codama IDL found from `workdir`, used without registering it.
///
/// Returns the registry error when nothing can be discovered, with the
/// discovery attempts described in its context.
pub async fn resolve_schema_auto(
    registry: &mut Registry,
    program: &str,
    rpc: &RpcClient,
    explicit_idl: Option<&Path>,
    workdir: Option<&Path>,
) -> Result<AutoResolvedSchema> {
    let initial_error = match registry
        .load_schema_with_source(program, explicit_idl, rpc)
        .await
    {
        Ok(loaded) => {
            return Ok(AutoResolvedSchema {
                schema: loaded.schema,
                source: loaded.source,
                newly_registered: false,
            });
        }
        Err(error) if explicit_idl.is_some() => return Err(error),
        Err(error) => error,
    };
    let address = registry.program_address(program)?;
    debug!(program, address, error = %initial_error, "registered sources failed; discovering IDL");
    match discover_canonical_idl(rpc, &address).await {
        Ok(onchain) => {
            let schema = parse_idl(&onchain.text, &address)?;
            let source = IdlSource::OnChain {
                name: "canonical-onchain".to_owned(),
                address: onchain.address,
            };
            registry.add_source(&address, source.clone(), None)?;
            registry
                .save()
                .await
                .context("discovered on-chain IDL but could not save the registry")?;
            return Ok(AutoResolvedSchema {
                schema,
                source,
                newly_registered: true,
            });
        }
        Err(error) => debug!(address, error = %error, "no canonical on-chain IDL"),
    }
    if let Some(workdir) = workdir {
        let candidates = discover_local_idls(workdir, &address)?;
        match candidates.as_slice() {
            [candidate] => {
                let text = tokio::fs::read_to_string(&candidate.idl_path).await?;
                let schema = parse_idl(&text, &address)?;
                return Ok(AutoResolvedSchema {
                    schema,
                    source: IdlSource::File {
                        name: candidate
                            .idl_path
                            .file_name()
                            .map(|name| name.to_string_lossy().into_owned())
                            .unwrap_or_default(),
                        path: candidate.idl_path.clone(),
                    },
                    newly_registered: false,
                });
            }
            [] => {}
            many => {
                let paths = many
                    .iter()
                    .map(|candidate| candidate.idl_path.display().to_string())
                    .collect::<Vec<_>>()
                    .join(", ");
                warn!(address, paths, "ambiguous local IDL candidates");
                return Err(initial_error.context(format!(
                    "multiple local IDLs match; register one explicitly: {paths}"
                )));
            }
        }
    }
    Err(initial_error.context(format!(
        "no canonical on-chain IDL exists for {address} and no local Anchor or Codama IDL was found; register one with a file or URL"
    )))
}
