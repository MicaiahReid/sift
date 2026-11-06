# sift

`sift` turns raw program accounts into readable data. Point it at a program,
give it an IDL (or let it find one on-chain or in your workspace), and browse
every account of a given type, decoded field by field, in an interactive
terminal UI. The same tooling is available to AI agents through a built-in MCP
server.

What it does today:

- **Decodes accounts from any IDL.** Works with Codama IDLs and Anchor IDLs
  (legacy and 0.30+), and supports the full Codama type system.
- **Finds IDLs for you.** Pulls the canonical IDL published on-chain through
  Program Metadata, or discovers it in your local Anchor or Codama project.
- **Lists accounts at any scale.** Discovers every account of a type and
  streams in details as you scroll, so large programs stay responsive.
- **Inspects deployed programs.** Shows the upgrade authority, ProgramData
  address, on-chain IDL, and verified build status in one view.
- **Patches account data offline.** Change a field by name (`authority=...`)
  and get back the re-encoded bytes. Useful for test fixtures and local
  validators. Nothing is written on-chain.
- **Builds unsigned transactions.** Name an instruction, pass its accounts and
  arguments, and get a serialized transaction ready for your signer of choice.
  `sift` never holds keys, signs, or sends.
- **Gives AI agents the same tools.** `sift mcp` lets Claude Code, Codex, or
  any MCP client query, filter, and aggregate decoded accounts.

## Install

You'll need a recent Rust toolchain (1.85 or newer) and
[`just`](https://github.com/casey/just).

```sh
# Install just if you don't have it
brew install just        # or: cargo install just

git clone https://github.com/MicaiahReid/sift.git
cd sift
just install
```

`just install` builds the CLI and puts `sift` on your `PATH` through
`cargo install`. Run `sift --help` to confirm it worked.

## Quick start

Register a program's IDL once, give it a friendly name, and start exploring:

```sh
# Grab the IDL the program published on-chain...
sift idl add PROGRAM_ADDRESS --onchain --alias myprogram

# ...or use one from your own project
sift idl add PROGRAM_ADDRESS --file target/idl/myprogram.json --alias myprogram

# Browse the program's accounts, instructions, and types
sift idl show myprogram

# Browse every account of one type
sift account list State --program myprogram

# Check who controls the program and whether its build is verified
sift program show myprogram
```

Working inside an Anchor or Codama project? You can often skip `idl add`
entirely. `sift` finds the matching IDL in your workspace and offers to
register it.

## What you can do

### Browse accounts

```sh
sift account list State --program myprogram
```

Opens an interactive browser listing every `State` account the program owns.
Addresses appear as soon as they're found, and details load in batches as you
scroll. Press Enter to open an account, `r` to view raw JSON, `c` to copy, and
`q` to quit.

Every browsing command prints JSON instead when you pass `--no-tui` or pipe the
output, so `sift` slots into scripts and `jq` pipelines.

### Inspect programs

```sh
sift program show myprogram   # one program
sift program list             # every program you've registered
```

See the program's loader, upgrade authority, ProgramData address, on-chain IDL
location, and, on mainnet, its verified build status from OtterSec.

### Patch an account

```sh
sift account patch ACCOUNT_ADDRESS --program myprogram --set authority=NEW_ADDRESS
```

Fetches the account, changes the fields you name, and prints the re-encoded
bytes (base64 by default). This is handy for cloning mainnet state into a local
validator with a few tweaks.

### Build a transaction

```sh
sift transaction build do_thing --program myprogram \
  --account state=STATE_ADDRESS \
  --arg amount=10
```

Prints an unsigned, serialized transaction for one instruction. Hand it to the
wallet or signer you already trust.

### Manage IDLs

```sh
sift idl list                         # everything registered
sift idl alias PROGRAM_ADDRESS mine   # add another name
sift idl prefer myprogram local-1     # pick which source wins
sift idl remove myprogram
```

A program can have several IDL sources (a local file, a URL, the on-chain
copy). `sift` keeps them all, uses your preferred one, and falls back to the
others.

## Use sift with AI agents (MCP)

`sift mcp` runs a [Model Context Protocol](https://modelcontextprotocol.io)
server, so an agent can answer questions like *"which vaults have more than
1,000 SOL deposited?"* or *"who is the upgrade authority on this program?"*
directly from on-chain data.

Install `sift` first (see [Install](#install)), then add it to your agent.

**Claude Code**

```sh
claude mcp add sift --scope user -- sift mcp --url mainnet
```

**Codex**

```sh
codex mcp add sift -- sift mcp --url mainnet
```

**Claude Desktop, Cursor, and other MCP clients**

Add this to the client's MCP configuration file (for example
`claude_desktop_config.json` or `~/.cursor/mcp.json`):

```json
{
  "mcpServers": {
    "sift": {
      "command": "sift",
      "args": ["mcp", "--url", "mainnet"]
    }
  }
}
```

Swap `mainnet` for `devnet`, `localhost`, or your own RPC URL. If your client
can't find the `sift` binary, use its full path (`which sift`).

Once it's connected, agents can:

- list, register, and alias IDLs
- read a program's schema
- inspect a deployed program
- fetch and decode a single account, or list accounts of a type with
  filtering, sorting, field selection, and aggregates (sum, min, max, average,
  count)
- patch account bytes and build unsigned transactions

If an agent asks about a program you haven't registered, `sift` registers its
on-chain IDL automatically when one exists.

## Configuration

- **RPC endpoint.** `sift` uses public mainnet by default. Pass `--url` with a
  full RPC URL or a shorthand (`localhost`, `devnet`, `testnet`, `mainnet`).
- **Registry.** Registered IDLs live in `~/.config/sift/registry.toml` on macOS
  and Linux, and under `%APPDATA%` on Windows. Pass `--config PATH` to use a
  different one.
- **One-off IDLs.** Pass `--idl PATH` to use an IDL for a single command
  without saving it.
- **Debug logs.** Add `-v` (or `-vv`) to see what `sift` is doing, including
  full RPC requests. Logs go to stderr, so JSON output stays clean.

## What's next

These aren't supported yet:

- **Automatic PDA resolution** when building transactions. Today, accounts
  derived from seeds must be passed explicitly.
- **Multi-instruction transactions.** `transaction build` currently builds one
  instruction at a time.
- **Loading patched accounts into a local validator** directly, instead of
  copying the bytes by hand.
- **Non-canonical on-chain IDLs**, such as Program Metadata IDLs published by
  someone other than the program's upgrade authority.
- **YAML and TOML on-chain IDLs.** Only JSON is read today.
- **Anchor IDLs with generic types.**

## Development

```sh
just check   # type-check the workspace
just test    # run the test suite
just lint    # clippy with warnings as errors
just fmt     # format the code
```
