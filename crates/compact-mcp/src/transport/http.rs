use std::net::SocketAddr;

use axum::body::Body;
use axum::extract::{Request, State};
use axum::http::{StatusCode, header};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use http_body_util::Limited;
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

/// Reject an over-limit request body before the inner service can buffer it.
///
/// rmcp's `StreamableHttpService` reads the body with `Body::collect()` directly
/// (not an axum extractor), so `axum::extract::DefaultBodyLimit` — which only the
/// axum extractors honour — is a no-op here; that is exactly why `nest_service`
/// "bypasses axum's default". This layer enforces the cap two ways:
///
/// 1. A declared `Content-Length` over the cap is refused with 413 up front, so
///    an oversized upload is rejected before a single body byte is read.
/// 2. Every body (including chunked / length-omitted ones that step 1 can't see)
///    is wrapped in `Limited`, so the inner `collect()` errors out at the cap
///    instead of buffering an unbounded body into memory.
async fn enforce_body_limit(State(limit): State<usize>, req: Request, next: Next) -> Response {
    if let Some(len) = req
        .headers()
        .get(header::CONTENT_LENGTH)
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.parse::<u64>().ok())
        && len > limit as u64
    {
        return (StatusCode::PAYLOAD_TOO_LARGE, "request body too large").into_response();
    }
    let (parts, body) = req.into_parts();
    let req = Request::from_parts(parts, Body::new(Limited::new(body, limit)));
    next.run(req).await
}

/// Build the `/mcp` router exactly as [`run`] serves it: the DNS-rebinding
/// Origin/Host allow-list (the transport's real defence — there is no auth in v1)
/// AND the request-body cap layered on. Split out of [`run`] (an otherwise thin
/// bind-and-serve wrapper) so BOTH defences can be exercised through the real
/// handler without binding a socket or installing the process signal handler —
/// see `test_router`. The body-limit layer wraps the WHOLE router (including the
/// nested service), so it runs before rmcp ever touches the body (see
/// [`enforce_body_limit`]).
fn build_router(
    server: CompactMcp,
    bind: SocketAddr,
    allow_insecure: bool,
    ct: CancellationToken,
    max_body_bytes: usize,
) -> axum::Router {
    // DNS-rebinding protection is the PRIMARY control with no auth: without an
    // Origin check, any page the user visits can reach us on localhost. The
    // Origin allow-list stays on BOTH paths — non-browser MCP clients send no
    // Origin (and pass), while a cross-origin browser page is blocked, which is
    // what stops a rebinding page even on a public bind.
    let config = StreamableHttpServerConfig::default()
        .with_cancellation_token(ct)
        .with_allowed_origins([
            format!("http://127.0.0.1:{}", bind.port()),
            format!("http://localhost:{}", bind.port()),
        ]);
    let config = if allow_insecure && !bind.ip().is_loopback() {
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
    axum::Router::new()
        .nest_service("/mcp", service)
        .layer(axum::middleware::from_fn_with_state(
            max_body_bytes,
            enforce_body_limit,
        ))
}

pub async fn run(
    server: CompactMcp,
    bind: SocketAddr,
    allow_insecure: bool,
    max_body_bytes: usize,
) -> anyhow::Result<()> {
    let ct = CancellationToken::new();
    let router = build_router(
        server,
        bind,
        allow_insecure,
        ct.child_token(),
        max_body_bytes,
    );
    let listener = tokio::net::TcpListener::bind(bind).await?;
    tracing::info!("compact-mcp listening on http://{bind}/mcp (max body {max_body_bytes} bytes)");

    axum::serve(listener, router)
        .with_graceful_shutdown(async move {
            let _ = tokio::signal::ctrl_c().await;
            ct.cancel();
        })
        .await?;
    Ok(())
}

/// Testing hook: the exact router [`run`] serves, but detached from any bind or
/// signal handler, so integration tests can drive the Origin/Host allow-list AND
/// the body-size cap through the real HTTP handler. Behind the `testing` feature
/// so it never widens the shipped binary's surface (CI's `cargo check` without
/// `testing` proves the production build never depends on it). Uses the same
/// default body cap as production, so the testing router behaves exactly like the
/// shipped one.
#[cfg(feature = "testing")]
pub fn test_router(server: CompactMcp, bind: SocketAddr, allow_insecure: bool) -> axum::Router {
    // The production default: 4x the source cap (see `Config::max_http_body_bytes`).
    test_router_with_body_limit(
        server,
        bind,
        allow_insecure,
        compact_mcp_core::MAX_SOURCE_BYTES * 4,
    )
}

/// Like [`test_router`] but with an explicit body cap, so a body-limit test can
/// use a small deterministic limit instead of driving the multi-MiB production
/// default over a socket. Same feature gate and rationale as `test_router`.
#[cfg(feature = "testing")]
pub fn test_router_with_body_limit(
    server: CompactMcp,
    bind: SocketAddr,
    allow_insecure: bool,
    max_body_bytes: usize,
) -> axum::Router {
    build_router(
        server,
        bind,
        allow_insecure,
        CancellationToken::new(),
        max_body_bytes,
    )
}
