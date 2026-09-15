# wip

`wip` is a Rust CLI for inspecting Solana program accounts, applying offline
IDL-aware byte patches, and building unsigned transactions. It accepts Anchor
IDLs directly and a practical common subset of Codama-style IDLs.

## Quick start

```sh
wip idl add PROGRAM --file target/idl/example.json
wip account list State --program PROGRAM
wip account patch ADDRESS --program PROGRAM --set authority=NEW_ADDRESS
wip transaction build do_thing --program PROGRAM --account state=ADDRESS --arg amount=10
```

The CLI never signs or sends a transaction. `transaction build` emits a
serialized unsigned wire transaction, base64 by default.

## Configuration

The default registry is `~/.config/wip/registry.toml` (or the platform
equivalent). Use `--config PATH` to select another registry and `--idl PATH`
to use an IDL without persisting it.

`--url` accepts a full RPC URL or the `localhost`, `devnet`, `testnet`,
`mainnet`, and `mainnet-beta` cluster shorthands.

## Debug logging

Use `-v` for debug logs or repeat it (`-vv`) for trace logs. Logs are written
to stderr, leaving stdout safe for JSON and encoded-byte output. `RUST_LOG`
takes precedence and supports standard tracing filters, for example:

```sh
RUST_LOG=wip_core::rpc=debug,wip=debug wip account list State --program PROGRAM
```

Debug RPC logs include the complete JSON-RPC request payload and response.

If no schema is registered, `wip` searches the nearest Cargo workspace for
Anchor member projects whose `Anchor.toml` declares the requested program ID.
It checks `<member>/target/idl` first, then `<workspace>/target/idl`. In an
interactive terminal, it asks before registering a discovered IDL. With
`--no-input`, a single match is used only for that invocation and is never
persisted; ambiguous matches require `--idl` or an interactive selection.
