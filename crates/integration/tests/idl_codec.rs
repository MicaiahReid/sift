//! Cross-crate account layout regression coverage.

use serde_json::json;
use sift_core::{
    BuildOptions, TransactionVersion, build_transaction, decode_account, encode_account, parse_idl,
};

#[test]
fn anchor_fixture_round_trips_an_account() {
    let idl = json!({
        "name": "example",
        "accounts": [{
            "name": "State",
            "discriminator": [1,2,3,4,5,6,7,8],
            "type": {"kind":"struct","fields":[{"name":"authority","type":"pubkey"},{"name":"count","type":"u64"}]}
        }],
        "instructions": []
    });
    let schema = parse_idl(&idl.to_string(), "11111111111111111111111111111111").unwrap();
    let value = json!({"authority":"11111111111111111111111111111111","count":"42"});
    let bytes = encode_account(&schema.accounts[0], &schema.types, &value).unwrap();
    assert_eq!(
        decode_account(&schema.accounts[0], &schema.types, &bytes).unwrap(),
        value
    );
}

#[test]
fn default_transaction_builder_emits_an_unsigned_v1_envelope() {
    let idl = json!({
        "name": "example",
        "accounts": [],
        "instructions": [{
            "name": "ping",
            "discriminator": [1,2,3,4,5,6,7,8],
            "accounts": [{"name":"payer", "signer":true, "writable":true}],
            "args": []
        }]
    });
    let address = "11111111111111111111111111111111";
    let schema = parse_idl(&idl.to_string(), address).unwrap();
    let mut options = BuildOptions::default();
    options
        .accounts
        .insert("payer".to_owned(), address.to_owned());
    let built = build_transaction(&schema, "ping", &options).unwrap();
    assert_eq!(built.version, TransactionVersion::V1);
    assert_eq!(built.signers, vec![address]);
    assert_eq!(built.bytes.first(), Some(&0x81));
}
