//! Server state, tool routing, and shared response helpers.

use std::{
    collections::HashMap,
    path::PathBuf,
    sync::Arc,
    time::{Duration, Instant},
};

use anyhow::{Context, Result};
use rmcp::{
    ErrorData as McpError, RoleServer, ServerHandler,
    handler::server::{router::tool::ToolRouter, wrapper::Parameters},
    model::{
        CallToolResult, ContentBlock, Implementation, ListResourcesResult, PaginatedRequestParams,
        ReadResourceRequestParams, ReadResourceResponse, ReadResourceResult, Resource,
        ResourceContents, ServerCapabilities, ServerConfig,
    },
    service::RequestContext,
    tool, tool_handler, tool_router,
};
use serde_json::{Value, json};
use sift_core::{AutoResolvedSchema, DecodedAccounts, Registry, RpcClient, resolve_schema_auto};
use sift_types::IdlSource;
use tokio::sync::{Mutex, MutexGuard};
use tracing::debug;

mod account;
mod idl;
mod transaction;

pub use account::{AccountGetParams, AccountListParams, AccountPatchParams};
pub use idl::{IdlAddParams, IdlAliasParams, IdlPreferParams, IdlRemoveParams, IdlShowParams};
pub use transaction::TransactionBuildParams;

const REGISTRY_RESOURCE_URI: &str = "sift://registry";

/// How long decoded account listings are reused before being refetched.
const ACCOUNT_CACHE_TTL: Duration = Duration::from_secs(60);

const INSTRUCTIONS: &str = "\
sift decodes Solana program accounts with registered IDLs so you can read on-chain state as JSON \
instead of raw bytes.

Typical flow:
1. `idl_list` shows registered programs and their aliases.
2. `idl_show` (default detail=summary) lists account types, fields, and instructions; pass \
`definition` to expand one account, instruction, or type with its dependencies.
3. `account_list` fetches accounts of one type. Always pass `where`, `fields`, and `limit` to \
keep responses small. For counts, totals, or extremes use `aggregate` with `limit: 0` instead of \
paging through rows.
4. `account_get` decodes a single address; the owning program's IDL is used when `program` is omitted.

Programs without a registered IDL are registered automatically from the canonical on-chain \
Program Metadata IDL when one exists. Otherwise call `idl_add` with a file or URL.

Value conventions: integers wider than 32 bits are JSON strings; pubkeys are base58 strings; \
enums decode as {\"kind\": \"Variant\", \"fields\": [...]}, and `where` conditions on an enum path \
compare the variant name. Field paths use dots and brackets, for example `config.fee` or `votes[0]`.

Nothing here signs or sends transactions; `transaction_build` returns unsigned bytes and \
`account_patch` rewrites bytes locally.";

/// Runtime configuration shared by every tool invocation.
#[derive(Clone, Debug, Default)]
pub struct ServerOptions {
    /// Default RPC endpoint or cluster shorthand.
    pub url: Option<String>,
    /// Default RPC commitment.
    pub commitment: String,
    /// Directory searched for local Anchor or Codama IDLs when nothing is registered.
    pub workdir: Option<PathBuf>,
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
struct CacheKey {
    rpc_url: String,
    commitment: String,
    program: String,
    account_type: String,
    /// Serialized layout and type definitions, so a changed IDL never serves stale decodes.
    layout: String,
}

struct CachedAccounts {
    fetched_at: Instant,
    decoded: DecodedAccounts,
}

/// The sift MCP server.
#[derive(Clone)]
pub struct SiftServer {
    registry: Arc<Mutex<Registry>>,
    options: Arc<ServerOptions>,
    cache: Arc<Mutex<HashMap<CacheKey, CachedAccounts>>>,
    tool_router: ToolRouter<Self>,
}

/// A schema plus the human-readable location of the IDL that produced it.
pub(crate) struct ResolvedSchema {
    pub(crate) resolved: AutoResolvedSchema,
}

impl ResolvedSchema {
    pub(crate) fn location(&self) -> String {
        source_location(&self.resolved.source)
    }

    /// Adds `registered_idl` to a response when this call persisted a new source.
    pub(crate) fn annotate(&self, mut response: Value) -> Value {
        if self.resolved.newly_registered
            && let Some(object) = response.as_object_mut()
        {
            object.insert(
                "registered_idl".to_owned(),
                json!({
                    "source": self.location(),
                    "note": "No IDL was registered for this program; the canonical on-chain IDL was registered automatically.",
                }),
            );
        }
        response
    }
}

#[tool_router]
impl SiftServer {
    /// Creates a server over an already loaded registry.
    pub fn new(registry: Registry, options: ServerOptions) -> Self {
        Self {
            registry: Arc::new(Mutex::new(registry)),
            options: Arc::new(options),
            cache: Arc::new(Mutex::new(HashMap::new())),
            tool_router: Self::tool_router(),
        }
    }

    #[tool(
        name = "idl_list",
        description = "Lists registered programs with their addresses, aliases, and IDL sources. Call this first to learn which aliases are available.",
        annotations(read_only_hint = true, idempotent_hint = true, open_world_hint = false)
    )]
    async fn idl_list(&self) -> Result<CallToolResult, McpError> {
        respond(idl::list(self).await)
    }

    #[tool(
        name = "idl_add",
        description = "Registers an IDL source for a program at highest priority. Provide the program's base58 address and exactly one of `file` (local JSON path), `url` (HTTP(S) JSON), or `onchain: true` (canonical Program Metadata IDL). The IDL is validated before it is saved.",
        annotations(
            read_only_hint = false,
            destructive_hint = false,
            idempotent_hint = true
        )
    )]
    async fn idl_add(
        &self,
        Parameters(params): Parameters<IdlAddParams>,
    ) -> Result<CallToolResult, McpError> {
        respond(idl::add(self, params).await)
    }

    #[tool(
        name = "idl_alias",
        description = "Adds a friendly alias to a registered program. Aliases are accepted anywhere a program address is.",
        annotations(
            read_only_hint = false,
            destructive_hint = false,
            idempotent_hint = true
        )
    )]
    async fn idl_alias(
        &self,
        Parameters(params): Parameters<IdlAliasParams>,
    ) -> Result<CallToolResult, McpError> {
        respond(idl::alias(self, params).await)
    }

    #[tool(
        name = "idl_prefer",
        description = "Moves one of a program's registered IDL sources to highest priority. Source IDs come from `idl_list`.",
        annotations(
            read_only_hint = false,
            destructive_hint = false,
            idempotent_hint = true
        )
    )]
    async fn idl_prefer(
        &self,
        Parameters(params): Parameters<IdlPreferParams>,
    ) -> Result<CallToolResult, McpError> {
        respond(idl::prefer(self, params).await)
    }

    #[tool(
        name = "idl_remove",
        description = "Removes a program and all of its IDL sources and aliases from the registry.",
        annotations(
            read_only_hint = false,
            destructive_hint = true,
            idempotent_hint = true
        )
    )]
    async fn idl_remove(
        &self,
        Parameters(params): Parameters<IdlRemoveParams>,
    ) -> Result<CallToolResult, McpError> {
        respond(idl::remove(self, params).await)
    }

    #[tool(
        name = "idl_show",
        description = "Describes a program's IDL. `detail: \"summary\"` (default) returns account types with field names and short type signatures, instruction argument and account lists, and one-line signatures for named types. Pass `definition` to expand one account, instruction, or type in full together with every type it references. `detail: \"full\"` returns the entire normalized schema and is large; prefer `definition`.",
        annotations(read_only_hint = true, idempotent_hint = true)
    )]
    async fn idl_show(
        &self,
        Parameters(params): Parameters<IdlShowParams>,
    ) -> Result<CallToolResult, McpError> {
        respond(idl::show(self, params).await)
    }

    #[tool(
        name = "account_list",
        description = "Lists decoded accounts of one IDL account type owned by a program. Results are filtered server-side by `where`, ordered by `sort_by`, reduced to `fields`, then paged with `limit`/`offset`; `total` counts all accounts of the type and `matched` counts those passing `where`. Use `aggregate` (sum/min/max/avg/count over a field path) with `limit: 0` to compute totals or find extremes across all matches without transferring rows. Decoded data is cached for 60 seconds per program and type, so paging and refining filters is cheap; pass `refresh: true` to refetch.",
        annotations(read_only_hint = true, idempotent_hint = true)
    )]
    async fn account_list(
        &self,
        Parameters(params): Parameters<AccountListParams>,
    ) -> Result<CallToolResult, McpError> {
        respond(account::list(self, params).await)
    }

    #[tool(
        name = "account_get",
        description = "Fetches and decodes one account by address. The account type is detected from its discriminator unless `account_type` is given, and the owning program's IDL is used unless `program` is given. Pass `fields` to return only selected paths.",
        annotations(read_only_hint = true, idempotent_hint = true)
    )]
    async fn account_get(
        &self,
        Parameters(params): Parameters<AccountGetParams>,
    ) -> Result<CallToolResult, McpError> {
        respond(account::get(self, params).await)
    }

    #[tool(
        name = "account_patch",
        description = "Fetches an account, applies field replacements to its decoded JSON, and re-encodes the bytes locally. Nothing is written on-chain; the result is useful for test fixtures and simulations. `set` maps field paths to replacement JSON values.",
        annotations(read_only_hint = true, idempotent_hint = true)
    )]
    async fn account_patch(
        &self,
        Parameters(params): Parameters<AccountPatchParams>,
    ) -> Result<CallToolResult, McpError> {
        respond(account::patch(self, params).await)
    }

    #[tool(
        name = "transaction_build",
        description = "Builds an unsigned transaction for one IDL instruction using a fresh blockhash. Returns the fee payer, required signers, and base64 wire bytes. The transaction is never signed or sent. Use `idl_show` with `definition` set to the instruction name to learn its accounts and argument types.",
        annotations(read_only_hint = true, idempotent_hint = false)
    )]
    async fn transaction_build(
        &self,
        Parameters(params): Parameters<TransactionBuildParams>,
    ) -> Result<CallToolResult, McpError> {
        respond(transaction::build(self, params).await)
    }
}

impl SiftServer {
    /// Locks the registry after refreshing it from disk.
    ///
    /// The CLI may edit the same file while the server runs, so every tool
    /// works from the current on-disk state rather than the startup snapshot.
    pub(crate) async fn registry(&self) -> Result<MutexGuard<'_, Registry>> {
        let mut guard = self.registry.lock().await;
        let path = guard.path.clone();
        *guard = Registry::load(Some(path))
            .await
            .context("reloading the IDL registry")?;
        Ok(guard)
    }

    pub(crate) fn options(&self) -> &ServerOptions {
        &self.options
    }

    /// Creates an RPC client, letting a per-call URL or commitment override the server defaults.
    pub(crate) fn rpc(&self, url: Option<&str>, commitment: Option<&str>) -> Result<RpcClient> {
        let url = url.map(str::to_owned).or_else(|| self.options.url.clone());
        let commitment = commitment
            .map(str::to_owned)
            .unwrap_or_else(|| self.options.commitment.clone());
        RpcClient::new(url, commitment)
    }

    /// Resolves a schema, registering the canonical on-chain IDL when nothing is registered.
    pub(crate) async fn schema(&self, program: &str, rpc: &RpcClient) -> Result<ResolvedSchema> {
        let mut registry = self.registry().await?;
        let resolved = resolve_schema_auto(
            &mut registry,
            program,
            rpc,
            None,
            self.options.workdir.as_deref(),
        )
        .await?;
        debug!(program, source = ?resolved.source, newly_registered = resolved.newly_registered, "resolved schema for MCP tool");
        Ok(ResolvedSchema { resolved })
    }

    /// Returns cached decoded accounts when fresh, otherwise fetches and caches them.
    pub(crate) async fn decoded_accounts(
        &self,
        rpc: &RpcClient,
        schema: &sift_types::ProgramSchema,
        layout: &sift_types::AccountSchema,
        refresh: bool,
    ) -> Result<(DecodedAccounts, Duration)> {
        let key = CacheKey {
            rpc_url: rpc.url.clone(),
            commitment: rpc.commitment.clone(),
            program: schema.address.clone(),
            account_type: layout.name.clone(),
            layout: serde_json::to_string(&(layout, &schema.types))?,
        };
        if !refresh {
            let cache = self.cache.lock().await;
            if let Some(entry) = cache.get(&key)
                && entry.fetched_at.elapsed() < ACCOUNT_CACHE_TTL
            {
                return Ok((entry.decoded.clone(), entry.fetched_at.elapsed()));
            }
        }
        let decoded = sift_core::fetch_decoded_accounts(rpc, schema, layout)
            .await
            .context("fetching program accounts")?;
        let mut cache = self.cache.lock().await;
        cache.retain(|_, entry| entry.fetched_at.elapsed() < ACCOUNT_CACHE_TTL);
        cache.insert(
            key,
            CachedAccounts {
                fetched_at: Instant::now(),
                decoded: decoded.clone(),
            },
        );
        Ok((decoded, Duration::ZERO))
    }
}

#[tool_handler(router = self.tool_router)]
impl ServerHandler for SiftServer {
    fn get_info(&self) -> ServerConfig {
        ServerConfig::new(
            ServerCapabilities::builder()
                .enable_tools()
                .enable_resources()
                .build(),
        )
        .with_server_info(Implementation::new("sift", env!("CARGO_PKG_VERSION")))
        .with_instructions(INSTRUCTIONS)
    }

    async fn list_resources(
        &self,
        _request: Option<PaginatedRequestParams>,
        _context: RequestContext<RoleServer>,
    ) -> Result<ListResourcesResult, McpError> {
        let resource = Resource::new(REGISTRY_RESOURCE_URI, "IDL registry")
            .with_description("The persistent sift IDL registry: programs, aliases, and sources.")
            .with_mime_type("application/json");
        Ok(ListResourcesResult {
            resources: vec![resource],
            ..Default::default()
        })
    }

    async fn read_resource(
        &self,
        request: ReadResourceRequestParams,
        _context: RequestContext<RoleServer>,
    ) -> Result<ReadResourceResponse, McpError> {
        if request.uri != REGISTRY_RESOURCE_URI {
            return Err(McpError::resource_not_found(
                format!("unknown resource {}", request.uri),
                Some(json!({"available": [REGISTRY_RESOURCE_URI]})),
            ));
        }
        let registry = self
            .registry()
            .await
            .map_err(|error| McpError::internal_error(format!("{error:#}"), None))?;
        let text = serde_json::to_string(&registry.json()).unwrap_or_default();
        Ok(ReadResourceResult::new(vec![
            ResourceContents::text(text, request.uri).with_mime_type("application/json"),
        ])
        .into())
    }
}

/// Renders a tool outcome as compact JSON text.
///
/// Failures are returned as tool-level errors so the calling agent can read the
/// message and correct its input; protocol errors are reserved for transport faults.
fn respond(result: Result<Value>) -> Result<CallToolResult, McpError> {
    match result {
        Ok(value) => Ok(CallToolResult::success(vec![ContentBlock::text(
            serde_json::to_string(&value).unwrap_or_default(),
        )])),
        Err(error) => {
            debug!(error = ?error, "MCP tool reported an error");
            let body = json!({ "error": format!("{error:#}") });
            Ok(CallToolResult::error(vec![ContentBlock::text(
                body.to_string(),
            )]))
        }
    }
}

/// Formats an IDL source as a short location string.
pub(crate) fn source_location(source: &IdlSource) -> String {
    match source {
        IdlSource::File { path, .. } => path.display().to_string(),
        IdlSource::Url { url, .. } => url.to_string(),
        IdlSource::OnChain { address, .. } => format!("on-chain:{address}"),
    }
}

/// Describes an IDL source as JSON.
pub(crate) fn source_json(id: &str, source: &IdlSource, active: bool) -> Value {
    let kind = match source {
        IdlSource::File { .. } => "file",
        IdlSource::Url { .. } => "url",
        IdlSource::OnChain { .. } => "on-chain",
    };
    json!({
        "id": id,
        "kind": kind,
        "location": source_location(source),
        "active": active,
    })
}
