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
    let server = match config.transport {
        config::Transport::Stdio => server::CompactMcp::new(workspace),
        config::Transport::Http => server::CompactMcp::new_http(workspace),
    }
    .with_config(&config)
    .with_toolchain_mutation(config.allow_toolchain_mutation);

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
        config::Transport::Http => {
            let addr: std::net::SocketAddr = config.bind.parse()?;
            transport::http::bind_guard(&addr, config.allow_insecure_bind)?;
            transport::http::run(
                server,
                addr,
                config.allow_insecure_bind,
                config.max_http_body_bytes,
            )
            .await
        }
    }
}
