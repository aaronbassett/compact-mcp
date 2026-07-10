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
    let server = server::CompactMcp::new(workspace).with_config(&config);

    let gc = server.tasks();
    tokio::spawn(async move {
        let mut tick = tokio::time::interval(std::time::Duration::from_secs(60));
        loop {
            tick.tick().await;
            let n = gc.gc(std::time::SystemTime::now());
            if n > 0 {
                tracing::debug!("evicted {n} expired task(s)");
            }
        }
    });

    match config.transport {
        config::Transport::Stdio => transport::stdio::run(server).await,
        config::Transport::Http => anyhow::bail!("http transport lands in Task 23"),
    }
}
