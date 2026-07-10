use clap::Parser;
use compact_mcp::{config, server, transport};
use compact_mcp_core::Workspace;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // stdio transport owns stdout; every log line MUST go to stderr.
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .with_writer(std::io::stderr)
        .with_ansi(false)
        .init();

    let config = config::Config::parse();
    let workspace = Workspace::new(config.resolved_workspace_root()?)?;
    let server = server::CompactMcp::new(workspace);

    match config.transport {
        config::Transport::Stdio => transport::stdio::run(server).await,
        config::Transport::Http => anyhow::bail!("http transport lands in Task 23"),
    }
}
