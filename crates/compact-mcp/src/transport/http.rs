use std::net::SocketAddr;

use rmcp::transport::streamable_http_server::{
    StreamableHttpServerConfig, StreamableHttpService, session::local::LocalSessionManager,
};
use tokio_util::sync::CancellationToken;

use crate::server::CompactMcp;

/// v1 ships no authorization. A non-loopback bind therefore exposes "compile any
/// file in the workspace" to the network, so it must be opted into explicitly.
pub fn bind_guard(addr: &SocketAddr, allow_insecure: bool) -> anyhow::Result<()> {
    if addr.ip().is_loopback() || allow_insecure {
        return Ok(());
    }
    anyhow::bail!(
        "refusing to bind {addr}: compact-mcp has no authorization. \
         Pass --allow-insecure-bind if this is a trusted private network."
    )
}

pub async fn run(server: CompactMcp, bind: SocketAddr, allow_insecure: bool) -> anyhow::Result<()> {
    let ct = CancellationToken::new();

    // DNS-rebinding protection is the PRIMARY control with no auth: without an
    // Origin check, any page the user visits can reach us on localhost. The
    // Origin allow-list stays on BOTH paths — non-browser MCP clients send no
    // Origin (and pass), while a cross-origin browser page is blocked, which is
    // what stops a rebinding page even on a public bind.
    let config = StreamableHttpServerConfig::default()
        .with_cancellation_token(ct.child_token())
        .with_allowed_origins([
            format!("http://127.0.0.1:{}", bind.port()),
            format!("http://localhost:{}", bind.port()),
        ]);
    let config = if allow_insecure {
        // A deliberately public bind (0.0.0.0) is reached by remote clients under
        // whatever address they dialed, NOT the bind literal — so a loopback-only
        // Host allow-list would 403 every legitimate remote request. The operator
        // has already accepted no-auth public exposure, and DNS rebinding is a
        // localhost threat that a public server doesn't face the same way, so drop
        // the Host check here (the Origin check above still guards browsers).
        config.disable_allowed_hosts()
    } else {
        // Loopback bind: pin the EXACT bound port — tighter than rmcp's
        // port-agnostic loopback default — so nothing local can rebind onto us
        // via another port.
        config.with_allowed_hosts([bind.to_string(), format!("localhost:{}", bind.port())])
    };

    let service = StreamableHttpService::new(
        move || Ok(server.clone()),
        LocalSessionManager::default().into(),
        config,
    );

    // The auth layer for v2 slots in here and nothing else moves:
    //   let service = ServiceBuilder::new().layer(AuthLayer::new(v)).service(service);
    let router = axum::Router::new().nest_service("/mcp", service);
    let listener = tokio::net::TcpListener::bind(bind).await?;
    tracing::info!("compact-mcp listening on http://{bind}/mcp");

    axum::serve(listener, router)
        .with_graceful_shutdown(async move {
            let _ = tokio::signal::ctrl_c().await;
            ct.cancel();
        })
        .await?;
    Ok(())
}
