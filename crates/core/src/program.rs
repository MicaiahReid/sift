//! Deployed program inspection: loader state, upgrade authority, on-chain IDL, and verified builds.

use std::sync::Arc;

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use tokio::{sync::Semaphore, task::JoinSet};
use tracing::{debug, warn};

use crate::{RpcClient, canonical_idl_address};

/// The upgradeable BPF loader.
pub const BPF_LOADER_UPGRADEABLE_ID: &str = "BPFLoaderUpgradeab1e11111111111111111111111";
/// The immutable BPF loader (v2).
pub const BPF_LOADER_ID: &str = "BPFLoader2111111111111111111111111111111111";
/// The deprecated immutable BPF loader (v1).
pub const BPF_LOADER_DEPRECATED_ID: &str = "BPFLoader1111111111111111111111111111111111";
/// Loader v4.
pub const LOADER_V4_ID: &str = "LoaderV411111111111111111111111111111111111";
/// The owner of builtin programs.
pub const NATIVE_LOADER_ID: &str = "NativeLoader1111111111111111111111111111111";
/// Genesis hash of mainnet-beta, the only cluster OtterSec verifies builds for.
const MAINNET_GENESIS_HASH: &str = "5eykt4UsFv8P8NJdTREpY1vzqKqZKvdpKuc147dw2N9d";
const VERIFY_API: &str = "https://verify.osec.io/status";
// Keeps `program list` under public RPC rate limits; each inspection makes three or four calls.
const MAX_CONCURRENT_INSPECTIONS: usize = 8;
// Upgradeable ProgramData header: u32 tag, u64 slot, Option<Pubkey> authority.
const PROGRAM_DATA_HEADER_LEN: usize = 45;

/// What the cluster reports about one deployed program.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct ProgramInfo {
    /// Program base58 address.
    pub address: String,
    /// Loader that owns the program account.
    pub loader: String,
    /// Upgradeable-loader ProgramData account, when the program has one.
    pub programdata_address: Option<String>,
    /// Address allowed to upgrade the program; `None` when it is immutable.
    pub authority: Option<String>,
    /// Slot in which the current program binary was deployed, when the loader records it.
    pub last_deployed_slot: Option<u64>,
    /// Canonical Program Metadata IDL account, when one exists.
    pub idl_address: Option<String>,
    /// OtterSec verified-build record, when the deployed binary matches a public source build.
    pub verified_build: Option<VerifiedBuild>,
}

/// A verified build whose reproducible output matches the deployed program binary.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct VerifiedBuild {
    /// Source repository URL the build was produced from.
    pub repo_url: String,
    /// Source commit the build was produced from.
    pub commit: String,
    /// SHA-256 of the deployed executable, which equals the build's hash.
    pub executable_hash: String,
    /// When the verifier last confirmed the match.
    pub last_verified_at: Option<String>,
}

#[derive(Debug, PartialEq, Eq)]
struct LoaderState {
    programdata_address: Option<String>,
    authority: Option<String>,
    last_deployed_slot: Option<u64>,
}

/// Reads a program's loader state, canonical IDL address, and verified-build status.
///
/// Verified builds are only looked up when `client` points at mainnet-beta. A
/// failed IDL or verification lookup is logged and reported as absent; a
/// missing account or one that is not a program is an error.
pub async fn inspect_program(client: &RpcClient, address: &str) -> Result<ProgramInfo> {
    let mainnet = on_mainnet(client).await;
    inspect(client, address, mainnet).await
}

/// Inspects several programs concurrently, returning one result per address in input order.
///
/// Each result behaves like [`inspect_program`]; one program failing does not
/// affect the others.
pub async fn inspect_programs(
    client: &RpcClient,
    addresses: &[String],
) -> Vec<Result<ProgramInfo>> {
    let mainnet = on_mainnet(client).await;
    let permits = Arc::new(Semaphore::new(MAX_CONCURRENT_INSPECTIONS));
    let mut tasks = JoinSet::new();
    for (index, address) in addresses.iter().cloned().enumerate() {
        let client = client.clone();
        let permits = permits.clone();
        tasks.spawn(async move {
            let _permit = permits.acquire_owned().await;
            (index, inspect(&client, &address, mainnet).await)
        });
    }
    let mut results = (0..addresses.len()).map(|_| None).collect::<Vec<_>>();
    while let Some(joined) = tasks.join_next().await {
        match joined {
            Ok((index, result)) => results[index] = Some(result),
            Err(error) => warn!(error = %error, "program inspection task failed"),
        }
    }
    results
        .into_iter()
        .map(|result| {
            result.unwrap_or_else(|| Err(anyhow::anyhow!("program inspection was aborted")))
        })
        .collect()
}

async fn on_mainnet(client: &RpcClient) -> bool {
    match client.genesis_hash().await {
        Ok(genesis) => genesis == MAINNET_GENESIS_HASH,
        Err(error) => {
            warn!(error = %error, "could not identify the cluster; skipping verified-build lookups");
            false
        }
    }
}

async fn inspect(client: &RpcClient, address: &str, mainnet: bool) -> Result<ProgramInfo> {
    let account = client
        .account_slice(address, 0, 48)
        .await?
        .ok_or_else(|| anyhow::anyhow!("program account {address} does not exist"))?;
    let mut state = loader_state(&account.owner, &account.data)
        .with_context(|| format!("{address} is not a deployed program"))?;
    if let Some(programdata) = &state.programdata_address {
        let data = client
            .account_slice(programdata, 0, PROGRAM_DATA_HEADER_LEN)
            .await?
            .ok_or_else(|| anyhow::anyhow!("ProgramData account {programdata} does not exist"))?
            .data;
        let (slot, authority) = parse_program_data(&data)?;
        state.last_deployed_slot = Some(slot);
        state.authority = authority;
    }
    let (idl_address, verified_build) =
        tokio::join!(canonical_idl_address(client, address), async {
            if mainnet {
                verified_build(address).await
            } else {
                Ok(None)
            }
        });
    let idl_address = idl_address.unwrap_or_else(|error| {
        warn!(address, error = %error, "could not look up the canonical on-chain IDL");
        None
    });
    let verified_build = verified_build.unwrap_or_else(|error| {
        warn!(address, error = %error, "could not look up verified-build status");
        None
    });
    Ok(ProgramInfo {
        address: address.to_owned(),
        loader: account.owner,
        programdata_address: state.programdata_address,
        authority: state.authority,
        last_deployed_slot: state.last_deployed_slot,
        idl_address,
        verified_build,
    })
}

fn loader_state(owner: &str, data: &[u8]) -> Result<LoaderState> {
    let mut state = LoaderState {
        programdata_address: None,
        authority: None,
        last_deployed_slot: None,
    };
    match owner {
        BPF_LOADER_UPGRADEABLE_ID => {
            if read_u32(data, 0)? != 2 {
                bail!("upgradeable loader account is not a Program account")
            }
            state.programdata_address = Some(read_address(data, 4)?);
        }
        LOADER_V4_ID => {
            state.last_deployed_slot = Some(read_u64(data, 0)?);
            // Once finalized, this field names the next program version, not an authority.
            match read_u64(data, 40)? {
                2 => {}
                _ => state.authority = Some(read_address(data, 8)?),
            }
        }
        BPF_LOADER_ID | BPF_LOADER_DEPRECATED_ID | NATIVE_LOADER_ID => {}
        other => bail!("account is owned by {other}, not a program loader"),
    }
    Ok(state)
}

fn parse_program_data(data: &[u8]) -> Result<(u64, Option<String>)> {
    if read_u32(data, 0)? != 3 {
        bail!("ProgramData account has an unexpected tag")
    }
    let slot = read_u64(data, 4)?;
    let authority = match data.get(12) {
        Some(0) => None,
        Some(1) => Some(read_address(data, 13)?),
        _ => bail!("ProgramData authority is malformed"),
    };
    Ok((slot, authority))
}

async fn verified_build(address: &str) -> Result<Option<VerifiedBuild>> {
    #[derive(Deserialize)]
    struct Status {
        is_verified: bool,
        #[serde(flatten)]
        build: VerifiedBuild,
    }
    let status: Status = reqwest::Client::new()
        .get(format!("{VERIFY_API}/{address}"))
        .send()
        .await?
        .error_for_status()?
        .json()
        .await
        .context("verified-build response is malformed")?;
    debug!(
        address,
        is_verified = status.is_verified,
        "received verified-build status"
    );
    Ok(status.is_verified.then_some(status.build))
}

fn read_u32(data: &[u8], offset: usize) -> Result<u32> {
    Ok(u32::from_le_bytes(read_array(data, offset)?))
}

fn read_u64(data: &[u8], offset: usize) -> Result<u64> {
    Ok(u64::from_le_bytes(read_array(data, offset)?))
}

fn read_address(data: &[u8], offset: usize) -> Result<String> {
    Ok(bs58::encode(read_array::<32>(data, offset)?).into_string())
}

fn read_array<const N: usize>(data: &[u8], offset: usize) -> Result<[u8; N]> {
    data.get(offset..offset + N)
        .and_then(|bytes| bytes.try_into().ok())
        .ok_or_else(|| anyhow::anyhow!("loader account data is too short"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_programdata_and_authority_from_upgradeable_accounts() {
        let mut program = 2_u32.to_le_bytes().to_vec();
        program.extend([7_u8; 32]);
        let state = loader_state(BPF_LOADER_UPGRADEABLE_ID, &program).unwrap();
        assert_eq!(
            state.programdata_address,
            Some(bs58::encode([7_u8; 32]).into_string())
        );

        let mut programdata = 3_u32.to_le_bytes().to_vec();
        programdata.extend(42_u64.to_le_bytes());
        programdata.push(1);
        programdata.extend([9_u8; 32]);
        assert_eq!(
            parse_program_data(&programdata).unwrap(),
            (42, Some(bs58::encode([9_u8; 32]).into_string()))
        );
        programdata.truncate(12);
        programdata.push(0);
        assert_eq!(parse_program_data(&programdata).unwrap(), (42, None));
    }

    #[test]
    fn finalized_loader_v4_programs_have_no_authority() {
        let mut program = 5_u64.to_le_bytes().to_vec();
        program.extend([1_u8; 32]);
        program.extend(2_u64.to_le_bytes());
        let state = loader_state(LOADER_V4_ID, &program).unwrap();
        assert_eq!(state.authority, None);
        assert_eq!(state.last_deployed_slot, Some(5));
    }
}
