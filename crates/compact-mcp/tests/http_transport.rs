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

/// The unauthenticated HTTP transport must refuse an oversized POST body up front
/// rather than letting rmcp buffer it whole via `Body::collect()`. These tests
/// drive the REAL router (via `test_router_with_body_limit`, with a small explicit
/// cap for speed/determinism) over a live loopback socket and cover BOTH defences
/// in `enforce_body_limit`: the Content-Length 413 fast-path AND the `Limited`
/// streaming wrap for chunked / length-omitted bodies.
#[cfg(feature = "testing")]
mod http_body_limit {
    use std::net::SocketAddr;

    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpStream;

    /// Small explicit cap: big enough to be a real limit, small enough that the
    /// over-cap cases send a trivial payload.
    const LIMIT: usize = 1024;

    async fn spawn_server() -> (tempfile::TempDir, SocketAddr) {
        let dir = tempfile::tempdir().unwrap();
        let ws = compact_mcp_core::Workspace::new(dir.path()).unwrap();
        let server = compact_mcp::server::CompactMcp::new_http(ws);

        let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0))
            .await
            .unwrap();
        let addr = listener.local_addr().unwrap();
        let router =
            compact_mcp::transport::http::test_router_with_body_limit(server, addr, false, LIMIT);
        tokio::spawn(async move {
            let _ = axum::serve(listener, router).await;
        });
        (dir, addr)
    }

    fn status_of(resp: &[u8]) -> u16 {
        let text = String::from_utf8_lossy(resp);
        text.lines()
            .next()
            .and_then(|line| line.split_whitespace().nth(1))
            .and_then(|code| code.parse().ok())
            .unwrap_or_else(|| panic!("no HTTP status line in response: {text:?}"))
    }

    /// POST to `/mcp` declaring `content_length`, actually sending `body`, and
    /// return the response status. When the server rejects on the DECLARED length
    /// before reading the body (the 413 fast-path), `body` may be shorter than
    /// `content_length` — the response arrives before the unsent bytes matter, so
    /// we prove the over-limit rejection without transmitting a large payload.
    async fn post_content_length(
        addr: SocketAddr,
        host: &str,
        content_length: usize,
        body: &[u8],
    ) -> u16 {
        let mut stream = TcpStream::connect(addr).await.unwrap();
        let head = format!(
            "POST /mcp HTTP/1.1\r\nHost: {host}\r\nContent-Type: application/json\r\n\
             Content-Length: {content_length}\r\nConnection: close\r\n\r\n"
        );
        // Tolerate a broken pipe: the server may 413 and close before it reads.
        let _ = stream.write_all(head.as_bytes()).await;
        let _ = stream.write_all(body).await;
        let _ = stream.flush().await;
        let mut resp = Vec::new();
        let _ = stream.read_to_end(&mut resp).await;
        status_of(&resp)
    }

    /// POST an over-cap body with NO Content-Length (`Transfer-Encoding: chunked`),
    /// carrying the full MCP handshake headers (allow-listed `Host`, the dual
    /// `Accept`, `application/json`) so rmcp clears its pre-body checks and actually
    /// READS the body via `expect_json` → `Body::collect()`. That is the poll that
    /// makes the `Limited` streaming wrap trip.
    async fn post_chunked(addr: SocketAddr, host: &str, payload_len: usize) -> u16 {
        let mut stream = TcpStream::connect(addr).await.unwrap();
        let head = format!(
            "POST /mcp HTTP/1.1\r\nHost: {host}\r\n\
             Accept: application/json, text/event-stream\r\n\
             Content-Type: application/json\r\n\
             Transfer-Encoding: chunked\r\nConnection: close\r\n\r\n"
        );
        let _ = stream.write_all(head.as_bytes()).await;
        // A single chunk of `payload_len` bytes, then the terminating 0-chunk.
        let _ = stream
            .write_all(format!("{payload_len:x}\r\n").as_bytes())
            .await;
        let _ = stream.write_all(&vec![b'a'; payload_len]).await;
        let _ = stream.write_all(b"\r\n0\r\n\r\n").await;
        let _ = stream.flush().await;
        let mut resp = Vec::new();
        let _ = stream.read_to_end(&mut resp).await;
        status_of(&resp)
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn an_over_limit_content_length_body_is_rejected_before_it_is_buffered() {
        let (_dir, addr) = spawn_server().await;
        let host = addr.to_string(); // allow-listed, so a passing request reaches rmcp.

        // Over the cap → rejected with 413 on the declared Content-Length, before a
        // single body byte is read (so we send none).
        let rejected = post_content_length(addr, &host, LIMIT + 1, b"").await;
        assert_eq!(
            rejected, 413,
            "an over-limit body must be rejected with 413 before it is buffered"
        );

        // Within the cap → the layer lets it through; rmcp answers (some non-413
        // status, e.g. 4xx for the missing MCP handshake headers). The point is
        // only that the body limit did NOT reject it.
        let passed = post_content_length(addr, &host, 16, &[b'a'; 16]).await;
        assert_ne!(
            passed, 413,
            "a within-limit body must not be rejected by the cap"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn an_over_limit_chunked_body_trips_the_streaming_wrap() {
        let (_dir, addr) = spawn_server().await;
        let host = addr.to_string();

        // A chunked body has no Content-Length, so the 413 fast-path can't see it —
        // the `Limited` wrap is the ONLY thing bounding it. rmcp reads the body via
        // `Body::collect()`, which errors at the cap and yields 500, so the whole
        // (8x-over-cap) body is never buffered.
        let status = post_chunked(addr, &host, LIMIT * 8).await;
        assert_eq!(
            status, 500,
            "an over-limit chunked body must trip the Limited wrap (500), not be buffered whole"
        );
    }
}
