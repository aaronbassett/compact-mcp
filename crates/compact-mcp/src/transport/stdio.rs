use rmcp::{ServiceExt, transport::stdio};

use crate::server::CompactMcp;

pub async fn run(server: CompactMcp) -> anyhow::Result<()> {
    let service = server.serve(stdio()).await?;
    service.waiting().await?;
    Ok(())
}
