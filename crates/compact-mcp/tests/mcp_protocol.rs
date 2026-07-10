use rmcp::{ServiceExt, model::CallToolRequestParams};

#[tokio::test]
async fn lists_the_four_analysis_tools_and_diagnoses_a_broken_contract() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("bad.compact"), "ledger count Field;").unwrap();
    let ws = compact_mcp_core::Workspace::new(dir.path()).unwrap();

    // rmcp gives us an in-memory duplex; no process, no sockets. The service
    // must be awaited to completion (`.waiting()`), not just constructed —
    // dropping the `RunningService` handle tears the transport down.
    let (client_t, server_t) = tokio::io::duplex(4096);
    tokio::spawn(async move {
        let _ = compact_mcp::server::CompactMcp::new(ws)
            .serve(server_t)
            .await
            .expect("server failed to start")
            .waiting()
            .await;
    });
    let client = ().serve(client_t).await.unwrap();

    let tools = client.list_all_tools().await.unwrap();
    let names: Vec<_> = tools.iter().map(|t| t.name.as_ref()).collect();
    for expected in ["diagnostics", "ast", "symbols", "stats"] {
        assert!(
            names.contains(&expected),
            "missing tool {expected}; got {names:?}"
        );
    }

    let res = client
        .call_tool(
            CallToolRequestParams::new("diagnostics").with_arguments(
                serde_json::json!({ "path": "bad.compact" })
                    .as_object()
                    .unwrap()
                    .clone(),
            ),
        )
        .await
        .unwrap();

    // A contract that does not parse is a SUCCESSFUL call with isError: true.
    assert_eq!(res.is_error, Some(true));
    let text = format!("{:?}", res.content);
    assert!(
        text.contains("compactp"),
        "diagnostic must be source-tagged: {text}"
    );

    client.cancel().await.unwrap();
}

#[tokio::test]
async fn ast_symbols_and_stats_all_succeed_on_valid_source() {
    let dir = tempfile::tempdir().unwrap();
    let ws = compact_mcp_core::Workspace::new(dir.path()).unwrap();

    let (client_t, server_t) = tokio::io::duplex(4096);
    tokio::spawn(async move {
        let _ = compact_mcp::server::CompactMcp::new(ws)
            .serve(server_t)
            .await
            .expect("server failed to start")
            .waiting()
            .await;
    });
    let client = ().serve(client_t).await.unwrap();

    let source = "ledger round: Counter;\nexport circuit inc(): [] { round.increment(1); }";
    let args = serde_json::json!({ "source": source })
        .as_object()
        .unwrap()
        .clone();

    for (tool, expected) in [
        ("ast", "SourceFile"),
        ("symbols", "round"),
        ("stats", "token_count"),
    ] {
        let res = client
            .call_tool(CallToolRequestParams::new(tool).with_arguments(args.clone()))
            .await
            .unwrap();
        assert_ne!(
            res.is_error,
            Some(true),
            "{tool} call reported an error: {:?}",
            res.content
        );
        let text = format!("{:?}", res.content);
        assert!(
            text.contains(expected),
            "{tool} result missing {expected:?}: {text}"
        );
    }

    client.cancel().await.unwrap();
}
