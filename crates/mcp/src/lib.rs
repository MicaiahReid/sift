//! Model Context Protocol server exposing sift's IDL registry, account
//! decoding, offline patching, and unsigned transaction building to AI agents.
//!
//! The server is a thin layer over `sift-core`: every tool validates its
//! input, delegates to core, and renders compact JSON so agents spend as few
//! tokens as possible on on-chain data.

use anyhow::{Context, Result};
use rmcp::{ServiceExt, transport::stdio};

mod server;

pub use server::{ServerOptions, SiftServer};

/// Serves `server` over standard input and output until the client disconnects.
///
/// Standard output carries the protocol, so callers must keep their own
/// diagnostics on standard error.
pub async fn run_stdio(server: SiftServer) -> Result<()> {
    let service = server
        .serve(stdio())
        .await
        .context("MCP handshake failed")?;
    service
        .waiting()
        .await
        .context("MCP session ended abnormally")?;
    Ok(())
}
