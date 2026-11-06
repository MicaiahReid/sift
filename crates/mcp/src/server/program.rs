//! Deployed program inspection tools.

use anyhow::Result;
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::{Value, json};
use sift_core::inspect_program;

use super::SiftServer;

/// Input for `program_show`.
#[derive(Debug, Deserialize, JsonSchema)]
pub struct ProgramShowParams {
    /// Program alias or base58 address. The program does not need to be registered.
    pub program: String,
    /// RPC URL or cluster shorthand (`mainnet`, `devnet`, `testnet`, `localhost`) overriding the server default.
    #[serde(default)]
    pub rpc_url: Option<String>,
}

pub(super) async fn show(server: &SiftServer, params: ProgramShowParams) -> Result<Value> {
    let (address, aliases) = {
        let registry = server.registry().await?;
        let address = registry.program_address(&params.program)?;
        let aliases = registry
            .config
            .programs
            .get(&address)
            .map(|entry| entry.aliases.iter().cloned().collect::<Vec<_>>())
            .unwrap_or_default();
        (address, aliases)
    };
    let rpc = server.rpc(params.rpc_url.as_deref(), None)?;
    let mut info = serde_json::to_value(inspect_program(&rpc, &address).await?)?;
    info["aliases"] = json!(aliases);
    Ok(info)
}
