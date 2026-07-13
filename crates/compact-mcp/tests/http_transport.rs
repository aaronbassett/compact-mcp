use compact_mcp::transport::http::bind_guard;

#[test]
fn loopback_binds_without_a_flag() {
    assert!(bind_guard(&"127.0.0.1:8080".parse().unwrap(), false).is_ok());
    assert!(bind_guard(&"[::1]:8080".parse().unwrap(), false).is_ok());
}

#[test]
fn a_public_bind_is_refused_without_the_flag() {
    // There is no authorization in v1. Binding 0.0.0.0 exposes "compile any file".
    let err = bind_guard(&"0.0.0.0:8080".parse().unwrap(), false).unwrap_err();
    assert!(err.to_string().contains("--allow-insecure-bind"));
    assert!(bind_guard(&"0.0.0.0:8080".parse().unwrap(), true).is_ok());
}

#[tokio::test]
async fn http_does_not_advertise_tasks_list_but_stdio_does() {
    let dir = tempfile::tempdir().unwrap();
    let ws = compact_mcp_core::Workspace::new(dir.path()).unwrap();

    let stdio = compact_mcp::server::CompactMcp::new(ws.clone());
    let http = compact_mcp::server::CompactMcp::new_http(ws);

    use rmcp::ServerHandler;
    let s = stdio.get_info().capabilities.tasks.unwrap();
    let h = http.get_info().capabilities.tasks.unwrap();

    assert!(
        s.list.is_some(),
        "stdio has one identifiable local requestor"
    );
    assert!(
        h.list.is_none(),
        "http cannot identify requestors; must not advertise tasks.list"
    );
    assert!(
        h.cancel.is_some(),
        "cancel stays available on both transports"
    );
}

/// The Origin/Host allow-list is the HTTP transport's real DNS-rebinding defence
/// (there is no auth in v1). These tests drive the REAL handler — the exact router
/// `run` serves, via `test_router` — over a live loopback socket with hand-rolled
/// `Host`/`Origin` headers, and assert a disallowed value is rejected with 403
/// while an allowed one passes the guard.
///
/// Precise assertion: rmcp emits `403 FORBIDDEN` *only* from its DNS-rebinding
/// guard, so `status == 403` means "guard rejected" and `status != 403` means
/// "guard let it through" (the handler then answers 405/406 for our minimal GET —
/// that non-403 status is exactly what we assert for the allowed case).
#[cfg(feature = "testing")]
mod dns_rebinding_allow_list {
    use std::net::SocketAddr;

    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpStream;

    /// Spawn the production router on an ephemeral loopback port. The returned temp
    /// workspace guard must be kept alive for the server's lifetime; the returned
    /// address is what the allow-list was pinned to.
    async fn spawn_server() -> (tempfile::TempDir, SocketAddr) {
        let dir = tempfile::tempdir().unwrap();
        let ws = compact_mcp_core::Workspace::new(dir.path()).unwrap();
        let server = compact_mcp::server::CompactMcp::new_http(ws);

        let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0))
            .await
            .unwrap();
        let addr = listener.local_addr().unwrap();
        // Build the allow-list against the ACTUAL bound port, exactly as `run` would
        // for this address.
        let router = compact_mcp::transport::http::test_router(server, addr, false);
        tokio::spawn(async move {
            let _ = axum::serve(listener, router).await;
        });
        (dir, addr)
    }

    /// Send a bare `GET /mcp` with the given `Host` and optional `Origin`, and
    /// return the response status code. A GET without `Accept: text/event-stream`
    /// yields a small, bounded response (405/406) once it clears the guard, so we
    /// never have to drain an SSE stream.
    async fn get_status(addr: SocketAddr, host: &str, origin: Option<&str>) -> u16 {
        let mut stream = TcpStream::connect(addr).await.unwrap();
        let mut req = format!("GET /mcp HTTP/1.1\r\nHost: {host}\r\n");
        if let Some(origin) = origin {
            req.push_str(&format!("Origin: {origin}\r\n"));
        }
        req.push_str("Accept: application/json\r\nConnection: close\r\n\r\n");
        stream.write_all(req.as_bytes()).await.unwrap();

        // These responses are tiny; read until the status line's CRLF (or EOF).
        let mut buf = Vec::new();
        let mut chunk = [0u8; 512];
        loop {
            let n = stream.read(&mut chunk).await.unwrap();
            if n == 0 {
                break;
            }
            buf.extend_from_slice(&chunk[..n]);
            if buf.windows(2).any(|w| w == b"\r\n") || buf.len() > 8192 {
                break;
            }
        }
        let line = String::from_utf8_lossy(&buf);
        // Status line: "HTTP/1.1 <code> <reason>".
        line.split_whitespace()
            .nth(1)
            .and_then(|code| code.parse::<u16>().ok())
            .unwrap_or_else(|| panic!("no HTTP status line in response: {line:?}"))
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_disallowed_origin_is_rejected_but_an_allowed_one_passes() {
        let (_dir, addr) = spawn_server().await;
        let allowed_host = addr.to_string(); // 127.0.0.1:<port> — on the Host allow-list.

        // A cross-origin browser page (the DNS-rebinding vector) is blocked...
        let rejected = get_status(addr, &allowed_host, Some("http://evil.example")).await;
        assert_eq!(
            rejected, 403,
            "a disallowed Origin must be rejected by the DNS-rebinding guard"
        );

        // ...while the loopback origin the server actually allows passes the guard.
        let allowed_origin = format!("http://{allowed_host}");
        let passed = get_status(addr, &allowed_host, Some(&allowed_origin)).await;
        assert_ne!(
            passed, 403,
            "the allow-listed Origin must pass the guard (got a 403)"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_disallowed_host_is_rejected_but_an_allowed_one_passes() {
        let (_dir, addr) = spawn_server().await;

        // A rebinding `Host` (an attacker DNS name that resolves to 127.0.0.1) is
        // blocked...
        let rejected = get_status(addr, "evil.example", None).await;
        assert_eq!(
            rejected, 403,
            "a disallowed Host must be rejected by the DNS-rebinding guard"
        );

        // ...while the exact bound authority the server pinned passes the guard.
        let passed = get_status(addr, &addr.to_string(), None).await;
        assert_ne!(
            passed, 403,
            "the allow-listed Host must pass the guard (got a 403)"
        );
    }
}
