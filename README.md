# sift

`sift` is a Rust CLI for inspecting Solana program accounts, applying offline
IDL-aware byte patches, and building unsigned transactions. It accepts Anchor
IDLs directly and a practical common subset of Codama-style IDLs.

## Quick start

```sh
sift idl add PROGRAM --file target/idl/example.json
sift --url mainnet idl add PROGRAM --onchain
sift idl prefer PROGRAM local-1
sift idl alias PROGRAM example
sift idl alias example another-alias
sift idl show example
sift account list State --program PROGRAM
sift account patch ADDRESS --program PROGRAM --set authority=NEW_ADDRESS
sift transaction build do_thing --program PROGRAM --account state=ADDRESS --arg amount=10
```

The CLI never signs or sends a transaction. `transaction build` emits a
serialized unsigned wire transaction, base64 by default.

The `account`, `transaction`, and `idl` command groups also accept their plural
forms: `accounts`, `transactions`, and `idls`.

`idl list` and `idl show` open an interactive browser in a terminal. Use Enter
to drill in, Esc to go back, `r` to view JSON, `c` to copy, and `q` to quit.
Pass `--no-tui` or pipe the output to print JSON.

`account list` opens the browser while matching addresses are discovered. It
lists those addresses as soon as discovery finishes, then fills in their details
in batches of 100. After the first batch, press `n` or scroll past row 90 of
the loaded range to fetch the next 100. Pending rows show “Waiting for details”;
loaded rows can be opened immediately. Press `n` to retry a failed request. You can
browse or quit while requests are in flight. JSON viewing and copying include
all discovered addresses, with a `status` for pending or unavailable accounts.
With `--no-tui` or piped output, the command prints the complete decoded JSON.

## MCP server for AI agents

`sift mcp` serves the same registry and account tooling to AI agents over the
Model Context Protocol on stdio. Global options such as `--url`, `--commitment`,
and `--config` set the server defaults; most tools also accept a per-call
`rpc_url`. Register it with an agent, for example in Claude Code:

```json
{
  "mcpServers": {
    "sift": { "command": "sift", "args": ["mcp", "--url", "mainnet"] }
  }
}
```

Tools:

| Tool | Purpose |
|---|---|
| `idl_list` | Registered programs, aliases, and IDL sources. |
| `idl_add` / `idl_alias` / `idl_prefer` / `idl_remove` | Registry management, matching the CLI. |
| `idl_show` | Compact schema summary, or one definition expanded with its referenced types. |
| `account_list` | Decoded accounts of one type with server-side `where` filters, `fields` projection, `sort_by`, `aggregate` (sum/min/max/avg/count), and `limit`/`offset` paging. Decoded results are cached for 60 seconds. |
| `account_get` | Decode one address; the owner's IDL is used when `program` is omitted. |
| `account_patch` | Offline byte patching, identical to `sift account patch`. |
| `transaction_build` | Unsigned transaction for one instruction. |

Programs without a registered IDL are registered automatically from the
canonical on-chain Program Metadata IDL when one exists; the response notes
when that happens. The registry resource `sift://registry` exposes the raw
registry JSON.

## Configuration

The default registry is `~/.config/sift/registry.toml` (or the platform
equivalent). Use `--config PATH` to select another registry and `--idl PATH`
to use an IDL without persisting it.

`--url` accepts a full RPC URL or the `localhost`, `devnet`, `testnet`,
`mainnet`, and `mainnet-beta` cluster shorthands. CLI and MCP requests default
to `https://api.mainnet-beta.solana.com`, independently of the Solana CLI
configuration. Use `--url` to override it, or `rpc_url` for an individual MCP call.

Use `sift idl add PROGRAM --idl-url URL` to register an HTTP(S) IDL document;
`--url` is reserved for the Solana RPC endpoint.

## Debug logging

Use `-v` for debug logs or repeat it (`-vv`) for trace logs. Logs are written
to stderr, leaving stdout safe for JSON and encoded-byte output. `RUST_LOG`
takes precedence and supports standard tracing filters, for example:

```sh
RUST_LOG=sift_core::rpc=debug,sift=debug sift account list State --program PROGRAM
```

Debug RPC logs include the complete JSON-RPC request payload and response.

If no schema is registered, `sift` searches the nearest Cargo workspace for
Anchor member projects whose `Anchor.toml` declares the requested program ID.
It checks `<member>/target/idl` first, then `<workspace>/target/idl`. In an
interactive terminal, it asks before registering a discovered IDL. With
`--no-input`, a single match is used only for that invocation and is never
persisted; ambiguous matches require `--idl` or an interactive selection.

Canonical on-chain Program Metadata IDLs can be registered with `--onchain`.
Sift validates and pins the canonical `idl` metadata PDA, then revalidates it
when loading. Discovered local and on-chain sources are independently offered
for registration; source IDs and the per-program `priority` list are shown by
`sift idl list`. `sift idl prefer PROGRAM SOURCE_ID` changes the preferred
source while retaining lower-priority fallbacks.
