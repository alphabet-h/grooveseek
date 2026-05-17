//! stdio transport runner. Previously inlined in
//! `server::run_server`. Extracted here so HTTP can coexist.

use anyhow::Result;

use crate::server::{KbServer, KbServerShared};

/// Serve MCP over stdio. One client at a time. Returns when the client
/// disconnects (stdin closed).
pub async fn run_stdio(shared: &KbServerShared) -> Result<()> {
    let server = KbServer::from_shared(shared);
    eprintln!("kb-mcp server ready (stdio transport)");

    let transport = rmcp::transport::io::stdio();
    let service = rmcp::serve_server(server, transport).await?;
    service.waiting().await?;
    Ok(())
}
