//! End-to-end coverage of the MCP tools over an in-memory transport with a
//! canned JSON-RPC endpoint standing in for a Solana cluster.

use std::{io::Write, path::Path};

use rmcp::{
    ServiceExt,
    model::{CallToolRequestParams, CallToolResult},
    service::{RoleClient, RunningService},
};
use serde_json::{Map, Value, json};
use sift_core::{Registry, encode_account, parse_idl};
use sift_mcp::{ServerOptions, SiftServer};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpListener,
};

const PROGRAM: &str = "11111111111111111111111111111111";

fn idl_json() -> Value {
    json!({
        "name": "demo",
        "address": PROGRAM,
        "accounts": [{
            "name": "State",
            "discriminator": [1,2,3,4,5,6,7,8],
            "type": {"kind":"struct","fields":[
                {"name":"authority","type":"pubkey"},
                {"name":"count","type":"u64"},
                {"name":"status","type":{"defined":{"name":"Status"}}}
            ]}
        }],
        "types": [{
            "name": "Status",
            "type": {"kind":"enum","variants":[{"name":"Active"},{"name":"Closed"}]}
        }],
        "instructions": [{
            "name": "ping",
            "discriminator": [9,9,9,9,9,9,9,9],
            "accounts": [{"name":"payer","signer":true,"writable":true}],
            "args": [{"name":"amount","type":"u64"}]
        }]
    })
}

fn address(seed: u8) -> String {
    bs58::encode([seed; 32]).into_string()
}

/// Encoded `State` accounts: two active, one closed.
fn fixture_accounts() -> Vec<(String, Vec<u8>)> {
    let schema = parse_idl(&idl_json().to_string(), PROGRAM).unwrap();
    [(1, "5", "Active"), (2, "40", "Active"), (3, "7", "Closed")]
        .into_iter()
        .map(|(seed, count, status)| {
            let value =
                json!({"authority": address(9), "count": count, "status": {"kind": status}});
            let bytes = encode_account(&schema.accounts[0], &schema.types, &value).unwrap();
            (address(seed), bytes)
        })
        .collect()
}

/// Serves canned JSON-RPC answers for every request until the task is dropped.
async fn fake_rpc() -> String {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let endpoint = format!("http://{}", listener.local_addr().unwrap());
    tokio::spawn(async move {
        loop {
            let (mut connection, _) = listener.accept().await.unwrap();
            tokio::spawn(async move {
                let mut request = Vec::new();
                let mut buffer = [0_u8; 4096];
                let body = loop {
                    let read = connection.read(&mut buffer).await.unwrap();
                    if read == 0 {
                        return;
                    }
                    request.extend_from_slice(&buffer[..read]);
                    if let Some(end) = request.windows(4).position(|bytes| bytes == b"\r\n\r\n") {
                        let headers = String::from_utf8_lossy(&request[..end]).to_string();
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
                let encode = |bytes: &[u8]| {
                    use base64::Engine;
                    json!([
                        base64::engine::general_purpose::STANDARD.encode(bytes),
                        "base64"
                    ])
                };
                let result = match body["method"].as_str().unwrap() {
                    "getProgramAccounts" => Value::Array(
                        fixture_accounts()
                            .iter()
                            .map(|(address, bytes)| json!({"pubkey": address, "account": {"owner": PROGRAM, "data": encode(bytes)}}))
                            .collect(),
                    ),
                    "getAccountInfo" => {
                        let wanted = body["params"][0].as_str().unwrap();
                        let found = fixture_accounts()
                            .into_iter()
                            .find(|(address, _)| address == wanted)
                            .map(|(_, bytes)| json!({"owner": PROGRAM, "data": encode(&bytes)}))
                            .unwrap_or(Value::Null);
                        json!({"value": found})
                    }
                    "getLatestBlockhash" => json!({"value": {"blockhash": address(7)}}),
                    other => panic!("unexpected RPC method {other}"),
                };
                let response = json!({"jsonrpc":"2.0","id":1,"result":result}).to_string();
                let mut out = Vec::new();
                write!(out, "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nConnection: close\r\nContent-Length: {}\r\n\r\n{response}", response.len()).unwrap();
                connection.write_all(&out).await.unwrap();
                connection.shutdown().await.ok();
            });
        }
    });
    endpoint
}

async fn start(registry_path: &Path, rpc_url: String) -> RunningService<RoleClient, ()> {
    let registry = Registry::load(Some(registry_path.to_path_buf()))
        .await
        .unwrap();
    let server = SiftServer::new(
        registry,
        ServerOptions {
            url: Some(rpc_url),
            commitment: "confirmed".into(),
            workdir: None,
        },
    );
    let (server_io, client_io) = tokio::io::duplex(1 << 20);
    tokio::spawn(async move {
        let service = server.serve(tokio::io::split(server_io)).await.unwrap();
        service.waiting().await.ok();
    });
    ().serve(tokio::io::split(client_io)).await.unwrap()
}

async fn call(
    client: &RunningService<RoleClient, ()>,
    name: &'static str,
    args: Value,
) -> (bool, Value) {
    let arguments: Map<String, Value> = serde_json::from_value(args).unwrap();
    let CallToolResult {
        content, is_error, ..
    } = client
        .call_tool(CallToolRequestParams::new(name).with_arguments(arguments))
        .await
        .unwrap();
    let text = content[0].as_text().unwrap().text.clone();
    (
        is_error.unwrap_or(false),
        serde_json::from_str(&text).unwrap(),
    )
}

#[tokio::test]
async fn agents_can_register_an_idl_and_query_filtered_accounts() {
    let dir = tempfile::tempdir().unwrap();
    let idl_path = dir.path().join("demo.json");
    std::fs::write(&idl_path, idl_json().to_string()).unwrap();
    let registry_path = dir.path().join("registry.toml");
    let rpc_url = fake_rpc().await;
    let client = start(&registry_path, rpc_url).await;

    let tools = client.list_all_tools().await.unwrap();
    let names = tools
        .iter()
        .map(|tool| tool.name.as_ref())
        .collect::<Vec<_>>();
    assert!(names.contains(&"account_list") && names.contains(&"idl_add"));

    let (failed, added) = call(
        &client,
        "idl_add",
        json!({"program": PROGRAM, "file": idl_path, "alias": "demo"}),
    )
    .await;
    assert!(!failed, "{added}");
    assert_eq!(added["schema"]["accounts"], json!(["State"]));
    assert!(registry_path.is_file(), "registry was saved");

    let (_, listed) = call(&client, "idl_list", json!({})).await;
    assert_eq!(listed["programs"][0]["aliases"], json!(["demo"]));

    let (_, summary) = call(&client, "idl_show", json!({"program": "demo"})).await;
    assert_eq!(
        summary["accounts"][0]["fields"][2],
        json!({"name": "status", "type": "Status"})
    );
    assert_eq!(summary["types"]["Status"], "Active | Closed");

    let (_, definition) = call(
        &client,
        "idl_show",
        json!({"program": "demo", "definition": "state"}),
    )
    .await;
    assert_eq!(definition["kind"], "account");
    assert!(definition["types"]["Status"].is_object());

    let (failed, page) = call(
        &client,
        "account_list",
        json!({
            "program": "demo",
            "account_type": "state",
            "where": [{"path": "status", "op": "eq", "value": "Active"}],
            "fields": ["count"],
            "sort_by": "count",
            "descending": true,
            "limit": 1
        }),
    )
    .await;
    assert!(!failed, "{page}");
    assert_eq!(page["total"], 3);
    assert_eq!(page["matched"], 2);
    assert_eq!(page["returned"], 1);
    assert_eq!(page["next_offset"], 1);
    assert_eq!(page["accounts"][0]["address"], address(2));
    assert_eq!(page["accounts"][0]["data"], json!({"count": "40"}));

    let (_, counted) = call(
        &client,
        "account_list",
        json!({"program": "demo", "account_type": "State", "limit": 0, "fields": ["authority"],
               "aggregate": [{"op": "sum", "path": "count"}, {"op": "max", "path": "count"}]}),
    )
    .await;
    assert_eq!(counted["matched"], 3);
    assert_eq!(counted["returned"], 0);
    assert_eq!(counted["aggregates"]["sum(count)"], "52");
    assert_eq!(
        counted["aggregates"]["max(count)"],
        json!({"value": "40", "address": address(2), "fields": {"authority": address(9)}})
    );
    assert!(
        counted["cache_age_secs"].is_number(),
        "second call used the cache"
    );

    let (failed, single) = call(&client, "account_get", json!({"address": address(3)})).await;
    assert!(!failed, "{single}");
    assert_eq!(single["account_type"], "State");
    assert_eq!(single["data"]["status"], json!({"kind": "Closed"}));

    let (failed, patched) = call(
        &client,
        "account_patch",
        json!({"address": address(1), "program": "demo", "set": {"count": "99"}, "encoding": "hex"}),
    )
    .await;
    assert!(!failed, "{patched}");
    assert_eq!(patched["data"]["count"], "99");
    assert!(
        patched["bytes"]
            .as_str()
            .unwrap()
            .starts_with("0102030405060708")
    );

    let (failed, built) = call(
        &client,
        "transaction_build",
        json!({"program": "demo", "instruction": "ping", "accounts": {"payer": address(5)}, "args": {"amount": "1"}}),
    )
    .await;
    assert!(!failed, "{built}");
    assert_eq!(built["fee_payer"], address(5));
    assert_eq!(built["version"], "v1");

    client.cancel().await.unwrap();
}

#[tokio::test]
async fn input_mistakes_are_reported_as_tool_errors_with_guidance() {
    let dir = tempfile::tempdir().unwrap();
    let registry_path = dir.path().join("registry.toml");
    let rpc_url = fake_rpc().await;
    let client = start(&registry_path, rpc_url).await;

    let (failed, response) = call(
        &client,
        "idl_alias",
        json!({"program": "nope", "alias": "x"}),
    )
    .await;
    assert!(failed);
    assert!(response["error"].as_str().unwrap().contains("nope"));

    let (failed, response) = call(&client, "idl_add", json!({"program": PROGRAM})).await;
    assert!(failed);
    assert!(
        response["error"]
            .as_str()
            .unwrap()
            .contains("exactly one of")
    );

    let idl_path = dir.path().join("demo.json");
    std::fs::write(&idl_path, idl_json().to_string()).unwrap();
    call(
        &client,
        "idl_add",
        json!({"program": PROGRAM, "file": idl_path}),
    )
    .await;
    let (failed, response) = call(
        &client,
        "account_list",
        json!({"program": PROGRAM, "account_type": "Missing"}),
    )
    .await;
    assert!(failed);
    let message = response["error"].as_str().unwrap();
    assert!(
        message.contains("Missing") && message.contains("State"),
        "{message}"
    );

    client.cancel().await.unwrap();
}
