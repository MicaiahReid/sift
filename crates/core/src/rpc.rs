//! Minimal JSON-RPC client for the read-only calls this CLI needs.

use anyhow::{Context, Result, bail};
use serde_json::{Value, json};
use tracing::{debug, warn};

/// Read-only Solana JSON-RPC client with explicit commitment selection.
#[derive(Clone, Debug)]
pub struct RpcClient {
    /// Cluster endpoint used for each request.
    pub url: String,
    /// Commitment sent with account lookup methods.
    pub commitment: String,
    client: reqwest::Client,
}

/// Account bytes together with the owning program address.
#[derive(Clone, Debug)]
pub struct RpcAccount {
    /// Account owner reported by the cluster.
    pub owner: String,
    /// Decoded account data.
    pub data: Vec<u8>,
}

impl RpcClient {
    /// Creates a client with the public mainnet endpoint as the final default.
    pub fn new(url: Option<String>, commitment: String) -> Result<Self> {
        let url = url
            .as_deref()
            .map(expand_url_shorthand)
            .or_else(solana_cli_rpc_url)
            .unwrap_or_else(|| "https://api.mainnet-beta.solana.com".to_owned());
        debug!(endpoint = %url, commitment, "initialized RPC client");
        Ok(Self {
            url,
            commitment,
            client: reqwest::Client::builder().build()?,
        })
    }

    async fn call(&self, method: &str, params: Value) -> Result<Value> {
        debug!(endpoint = %self.url, method, payload = ?params, "sending RPC request");
        let mut response: Value = self
            .client
            .post(&self.url)
            .json(&json!({"jsonrpc":"2.0","id":1,"method":method,"params":params}))
            .send()
            .await?
            .error_for_status()?
            .json()
            .await?;
        debug!(endpoint = %self.url, method, response = ?response, "received RPC response");
        if let Some(error) = response.get("error") {
            warn!(endpoint = %self.url, method, error = ?error, "RPC returned an error");
            bail!("RPC {method} failed: {error}")
        }
        response
            .get_mut("result")
            .map(Value::take)
            .ok_or_else(|| anyhow::anyhow!("RPC {method} response lacks result"))
    }

    /// Gets one account's base64 bytes, or `None` when it does not exist.
    pub async fn account(&self, address: &str) -> Result<Option<RpcAccount>> {
        let result = self
            .call(
                "getAccountInfo",
                json!([address, {"encoding":"base64", "commitment":self.commitment}]),
            )
            .await?;
        let Some(value) = result.get("value").filter(|value| !value.is_null()) else {
            return Ok(None);
        };
        let owner = value
            .get("owner")
            .and_then(Value::as_str)
            .ok_or_else(|| anyhow::anyhow!("account has no owner"))?
            .to_owned();
        let data = decode_data(value)?.ok_or_else(|| anyhow::anyhow!("account has no data"))?;
        Ok(Some(RpcAccount { owner, data }))
    }

    /// Gets one account's base64 bytes, or `None` when it does not exist.
    pub async fn account_data(&self, address: &str) -> Result<Option<Vec<u8>>> {
        Ok(self.account(address).await?.map(|account| account.data))
    }

    /// Lists matching program account addresses without downloading account data.
    pub async fn program_account_addresses(
        &self,
        program: &str,
        filters: Value,
    ) -> Result<Vec<String>> {
        let result = self
            .call(
                "getProgramAccounts",
                json!([program, {
                    "encoding": "base64", "commitment": self.commitment, "filters": filters,
                    "dataSlice": {"offset": 0, "length": 0}
                }]),
            )
            .await?;
        result
            .as_array()
            .ok_or_else(|| anyhow::anyhow!("getProgramAccounts result is not an array"))?
            .iter()
            .map(|entry| {
                entry
                    .get("pubkey")
                    .and_then(Value::as_str)
                    .map(ToOwned::to_owned)
                    .ok_or_else(|| anyhow::anyhow!("program account lacks pubkey"))
            })
            .collect()
    }

    /// Fetches up to 100 accounts in address order, retaining missing accounts as `None`.
    pub async fn multiple_accounts(&self, addresses: &[String]) -> Result<Vec<Option<RpcAccount>>> {
        if addresses.len() > 100 {
            bail!("getMultipleAccounts accepts at most 100 addresses")
        }
        if addresses.is_empty() {
            return Ok(Vec::new());
        }
        let result = self
            .call(
                "getMultipleAccounts",
                json!([addresses, {
                    "encoding": "base64", "commitment": self.commitment
                }]),
            )
            .await?;
        let values = result
            .get("value")
            .and_then(Value::as_array)
            .ok_or_else(|| anyhow::anyhow!("getMultipleAccounts response lacks accounts"))?;
        if values.len() != addresses.len() {
            bail!("getMultipleAccounts returned an unexpected number of accounts")
        }
        values
            .iter()
            .map(|value| {
                if value.is_null() {
                    return Ok(None);
                }
                let owner = value
                    .get("owner")
                    .and_then(Value::as_str)
                    .ok_or_else(|| anyhow::anyhow!("account has no owner"))?
                    .to_owned();
                let data =
                    decode_data(value)?.ok_or_else(|| anyhow::anyhow!("account has no data"))?;
                Ok(Some(RpcAccount { owner, data }))
            })
            .collect()
    }

    /// Gets all program-owned accounts as `(address, bytes)` pairs.
    pub async fn program_accounts(
        &self,
        program: &str,
        filters: Value,
    ) -> Result<Vec<(String, Vec<u8>)>> {
        let result = self.call("getProgramAccounts", json!([program, {"encoding":"base64", "commitment":self.commitment, "filters":filters}])).await?;
        result
            .as_array()
            .ok_or_else(|| anyhow::anyhow!("getProgramAccounts result is not an array"))?
            .iter()
            .map(|entry| {
                let key = entry
                    .get("pubkey")
                    .and_then(Value::as_str)
                    .ok_or_else(|| anyhow::anyhow!("program account lacks pubkey"))?
                    .to_owned();
                Ok((
                    key,
                    decode_data(
                        entry
                            .get("account")
                            .ok_or_else(|| anyhow::anyhow!("program account lacks data"))?,
                    )?
                    .ok_or_else(|| anyhow::anyhow!("program account has no data"))?,
                ))
            })
            .collect()
    }

    /// Obtains the cluster's current lifetime token for a transaction message.
    pub async fn latest_blockhash(&self) -> Result<String> {
        self.call(
            "getLatestBlockhash",
            json!([{ "commitment": self.commitment }]),
        )
        .await?
        .pointer("/value/blockhash")
        .and_then(Value::as_str)
        .map(ToOwned::to_owned)
        .ok_or_else(|| anyhow::anyhow!("getLatestBlockhash response lacks blockhash"))
    }
}

/// Expands Solana cluster aliases accepted by the `--url` CLI option.
pub fn expand_url_shorthand(value: &str) -> String {
    match value.trim().to_ascii_lowercase().as_str() {
        "localhost" => "http://127.0.0.1:8899".to_owned(),
        "devnet" => "https://api.devnet.solana.com".to_owned(),
        "testnet" => "https://api.testnet.solana.com".to_owned(),
        "mainnet" | "mainnet-beta" => "https://api.mainnet-beta.solana.com".to_owned(),
        _ => value.to_owned(),
    }
}

/// Reads `json_rpc_url` from the Solana CLI's conventional config when present.
fn solana_cli_rpc_url() -> Option<String> {
    let config = directories::BaseDirs::new()?
        .home_dir()
        .join(".config/solana/cli/config.yml");
    let text = std::fs::read_to_string(config).ok()?;
    text.lines().find_map(|line| {
        line.trim()
            .strip_prefix("json_rpc_url:")
            .map(|value| value.trim().trim_matches(['\'', '"']).to_owned())
            .filter(|value| !value.is_empty())
    })
}

fn decode_data(value: &Value) -> Result<Option<Vec<u8>>> {
    let Some(data) = value.get("data").and_then(Value::as_array) else {
        return Ok(None);
    };
    let encoded = data
        .first()
        .and_then(Value::as_str)
        .ok_or_else(|| anyhow::anyhow!("account has invalid base64 data"))?;
    use base64::Engine;
    Ok(Some(
        base64::engine::general_purpose::STANDARD
            .decode(encoded)
            .context("account data is not base64")?,
    ))
}

#[cfg(test)]
mod tests {
    use super::{RpcClient, expand_url_shorthand};
    use serde_json::{Value, json};
    use std::{
        io::{Read, Write},
        net::TcpListener,
        time::Duration,
    };

    #[tokio::test]
    async fn discovers_addresses_without_data_and_preserves_missing_account_positions() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let endpoint = format!("http://{}", listener.local_addr().unwrap());
        let server = std::thread::spawn(move || {
            for (method, result) in [
                (
                    "getProgramAccounts",
                    json!([{"pubkey":"first"},{"pubkey":"closed"}]),
                ),
                (
                    "getMultipleAccounts",
                    json!({"value":[{"owner":"program","data":["AQI=","base64"]}, null]}),
                ),
            ] {
                let (mut connection, _) = listener.accept().unwrap();
                connection
                    .set_read_timeout(Some(Duration::from_secs(5)))
                    .unwrap();
                let mut request = Vec::new();
                let mut buffer = [0_u8; 4096];
                let body = loop {
                    let read = connection.read(&mut buffer).unwrap();
                    assert!(read > 0);
                    request.extend_from_slice(&buffer[..read]);
                    if let Some(end) = request.windows(4).position(|bytes| bytes == b"\r\n\r\n") {
                        let headers = String::from_utf8_lossy(&request[..end]);
                        let length: usize = headers
                            .lines()
                            .find_map(|line| {
                                let (key, value) = line.split_once(':')?;
                                key.eq_ignore_ascii_case("content-length")
                                    .then(|| value.trim().parse().unwrap())
                            })
                            .unwrap();
                        if request.len() >= end + 4 + length {
                            break serde_json::from_slice::<Value>(
                                &request[end + 4..end + 4 + length],
                            )
                            .unwrap();
                        }
                    }
                };
                assert_eq!(body["method"], method);
                assert_eq!(body["params"][1]["commitment"], "confirmed");
                if method == "getProgramAccounts" {
                    assert_eq!(
                        body["params"][1]["dataSlice"],
                        json!({"offset":0,"length":0})
                    );
                    assert_eq!(body["params"][1]["filters"], json!([{"dataSize":42}]));
                } else {
                    assert_eq!(body["params"][0], json!(["first", "closed"]));
                }
                let response = json!({"jsonrpc":"2.0","id":1,"result":result}).to_string();
                write!(connection, "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nConnection: close\r\nContent-Length: {}\r\n\r\n{response}", response.len()).unwrap();
            }
        });
        let rpc = RpcClient::new(Some(endpoint), "confirmed".into()).unwrap();
        let addresses = rpc
            .program_account_addresses("program", json!([{"dataSize":42}]))
            .await
            .unwrap();
        let accounts = rpc.multiple_accounts(&addresses).await.unwrap();
        assert_eq!(accounts[0].as_ref().unwrap().data, vec![1, 2]);
        assert!(accounts[1].is_none());
        server.join().unwrap();
    }

    #[test]
    fn expands_cluster_url_shorthands() {
        assert_eq!(expand_url_shorthand("localhost"), "http://127.0.0.1:8899");
        assert_eq!(
            expand_url_shorthand("devnet"),
            "https://api.devnet.solana.com"
        );
        assert_eq!(
            expand_url_shorthand("testnet"),
            "https://api.testnet.solana.com"
        );
        assert_eq!(
            expand_url_shorthand("mainnet"),
            "https://api.mainnet-beta.solana.com"
        );
        assert_eq!(
            expand_url_shorthand("mainnet-beta"),
            "https://api.mainnet-beta.solana.com"
        );
        assert_eq!(
            expand_url_shorthand("http://rpc.example.test"),
            "http://rpc.example.test"
        );
    }
}
