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
