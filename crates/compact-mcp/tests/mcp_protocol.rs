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

#[tokio::test]
async fn ast_symbols_and_stats_flag_over_depth_refusal() {
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

    // A 2000-link postfix call chain: shallow brackets, but a tree ~2000 deep that would
    // SIGABRT on Drop if parsed. The guard refuses it — and ast/symbols/stats must SIGNAL that
    // refusal, not report the byte-identical shape of a genuinely empty file.
    let deep = format!(
        "pragma language_version >= 0.23;\nexport pure circuit f(x: Field): Field {{ return x{}; }}\n",
        "()".repeat(2000)
    );
    let deep_args = serde_json::json!({ "source": deep })
        .as_object()
        .unwrap()
        .clone();

    for tool in ["ast", "symbols", "stats"] {
        let res = client
            .call_tool(CallToolRequestParams::new(tool).with_arguments(deep_args.clone()))
            .await
            .unwrap();
        assert_eq!(
            res.is_error,
            Some(true),
            "{tool} must flag over-depth refusal as an error: {:?}",
            res.content
        );
        let text = format!("{:?}", res.content);
        assert!(
            text.contains("structural depth"),
            "{tool} refusal must carry the depth message: {text}"
        );
    }

    // No false positive: a normal small contract is NOT flagged by any of the three.
    let ok_args = serde_json::json!({ "source": "ledger round: Counter;" })
        .as_object()
        .unwrap()
        .clone();
    for tool in ["ast", "symbols", "stats"] {
        let res = client
            .call_tool(CallToolRequestParams::new(tool).with_arguments(ok_args.clone()))
            .await
            .unwrap();
        assert_ne!(
            res.is_error,
            Some(true),
            "{tool} wrongly flagged a normal contract: {:?}",
            res.content
        );
    }

    client.cancel().await.unwrap();
}

#[tokio::test]
async fn toolchain_tool_surfaces_a_missing_binary_as_an_is_error_result() {
    // Hermetic: no real toolchain needed. A bogus binary makes the subprocess
    // spawn fail fast with `ToolchainNotFound`, which the tool must surface as a
    // successful call with `isError: true` carrying the message — NOT as an
    // opaque protocol error that rmcp would render as "internal error".
    let dir = tempfile::tempdir().unwrap();
    let ws = compact_mcp_core::Workspace::new(dir.path()).unwrap();
    let tc = compact_mcp_core::Toolchain::new("compact-does-not-exist-xyz", None);
    let (client_t, server_t) = tokio::io::duplex(8192);
    tokio::spawn(async move {
        let _ = compact_mcp::server::CompactMcp::with_toolchain(ws, tc)
            .serve(server_t)
            .await
            .expect("server failed to start")
            .waiting()
            .await;
    });
    let client = ().serve(client_t).await.unwrap();

    let res = client
        .call_tool(CallToolRequestParams::new("toolchain_list"))
        .await
        .unwrap();
    assert_eq!(
        res.is_error,
        Some(true),
        "missing binary must be an is_error result: {:?}",
        res.content
    );
    let text = format!("{:?}", res.content);
    assert!(
        text.contains("not found"),
        "error result must carry the not-found message: {text}"
    );
    client.cancel().await.unwrap();
}

#[tokio::test]
#[cfg_attr(not(feature = "toolchain-tests"), ignore)]
async fn format_tool_returns_formatted_text_without_touching_the_file() {
    let dir = tempfile::tempdir().unwrap();
    let original = "pragma language_version >= 0.23;\nimport CompactStandardLibrary;\nexport ledger    a:Counter;\n";
    std::fs::write(dir.path().join("m.compact"), original).unwrap();
    let ws = compact_mcp_core::Workspace::new(dir.path()).unwrap();

    let (client_t, server_t) = tokio::io::duplex(8192);
    tokio::spawn(async move {
        let _ = compact_mcp::server::CompactMcp::new(ws)
            .serve(server_t)
            .await
            .expect("server failed to start")
            .waiting()
            .await;
    });
    let client = ().serve(client_t).await.unwrap();

    let res = client
        .call_tool(
            CallToolRequestParams::new("format").with_arguments(
                serde_json::json!({ "path": "m.compact" })
                    .as_object()
                    .unwrap()
                    .clone(),
            ),
        )
        .await
        .unwrap();
    assert_ne!(res.is_error, Some(true));

    let on_disk = std::fs::read_to_string(dir.path().join("m.compact")).unwrap();
    assert_eq!(
        on_disk, original,
        "write defaults to false; the file must be untouched"
    );

    client.cancel().await.unwrap();
}

#[tokio::test]
async fn format_tool_surfaces_a_missing_toolchain_as_an_is_error_result() {
    // Hermetic: no real `compact` binary needed. The parse gate lives in core and
    // runs before the subprocess is ever spawned, so a VALID contract sails
    // through the gate and reaches `Toolchain::run`, where the bogus binary makes
    // the spawn fail fast with `ToolchainNotFound`. That `CoreError` must surface
    // as a successful call with `isError: true` carrying the message — never as
    // an opaque protocol error (rmcp renders `Err(McpError)` as "internal error",
    // dropping the actionable text).
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join("x.compact"),
        "pragma language_version >= 0.23;\n",
    )
    .unwrap();
    let ws = compact_mcp_core::Workspace::new(dir.path()).unwrap();
    let tc = compact_mcp_core::Toolchain::new("compact-does-not-exist-xyz", None);

    let (client_t, server_t) = tokio::io::duplex(8192);
    tokio::spawn(async move {
        let _ = compact_mcp::server::CompactMcp::with_toolchain(ws, tc)
            .serve(server_t)
            .await
            .expect("server failed to start")
            .waiting()
            .await;
    });
    let client = ().serve(client_t).await.unwrap();

    let res = client
        .call_tool(
            CallToolRequestParams::new("format").with_arguments(
                serde_json::json!({ "path": "x.compact" })
                    .as_object()
                    .unwrap()
                    .clone(),
            ),
        )
        .await
        .unwrap();
    assert_eq!(
        res.is_error,
        Some(true),
        "missing toolchain must be an is_error result: {:?}",
        res.content
    );
    let text = format!("{:?}", res.content);
    assert!(
        text.contains("not found"),
        "error result must carry the not-found message: {text}"
    );

    client.cancel().await.unwrap();
}

#[tokio::test]
async fn format_tool_flags_a_parse_failure_as_is_error() {
    // Hermetic: the CORE parse gate runs BEFORE any `compact` subprocess is
    // spawned, so broken source returns `Ok(FormatOutcome { ok: false, .. })`
    // carrying compactp diagnostics without ever invoking the formatter. This
    // must therefore pass WITHOUT `--features toolchain-tests`. The tool maps
    // `ok: false` to `isError: true`.
    let dir = tempfile::tempdir().unwrap();
    let ws = compact_mcp_core::Workspace::new(dir.path()).unwrap();

    let (client_t, server_t) = tokio::io::duplex(8192);
    tokio::spawn(async move {
        let _ = compact_mcp::server::CompactMcp::new(ws)
            .serve(server_t)
            .await
            .expect("server failed to start")
            .waiting()
            .await;
    });
    let client = ().serve(client_t).await.unwrap();

    let res = client
        .call_tool(
            CallToolRequestParams::new("format").with_arguments(
                serde_json::json!({ "source": "export circuit oops(): [] { let x = }" })
                    .as_object()
                    .unwrap()
                    .clone(),
            ),
        )
        .await
        .unwrap();
    assert_eq!(
        res.is_error,
        Some(true),
        "a parse failure must be an is_error result: {:?}",
        res.content
    );
    let text = format!("{:?}", res.content);
    assert!(
        text.contains("diagnostics") && text.contains("compactp"),
        "parse-failure result must carry source-tagged diagnostics: {text}"
    );

    client.cancel().await.unwrap();
}

#[tokio::test]
#[cfg_attr(not(feature = "toolchain-tests"), ignore)]
async fn format_tool_write_true_rewrites_the_file() {
    // Locks the one mutating code path at the tool boundary: `write: true` must
    // rewrite the file in place through the `format` handler.
    let dir = tempfile::tempdir().unwrap();
    let messy = "pragma language_version >= 0.23;\nimport CompactStandardLibrary;\nexport ledger    a:Counter;\n";
    std::fs::write(dir.path().join("m.compact"), messy).unwrap();
    let ws = compact_mcp_core::Workspace::new(dir.path()).unwrap();

    let (client_t, server_t) = tokio::io::duplex(8192);
    tokio::spawn(async move {
        let _ = compact_mcp::server::CompactMcp::new(ws)
            .serve(server_t)
            .await
            .expect("server failed to start")
            .waiting()
            .await;
    });
    let client = ().serve(client_t).await.unwrap();

    let res = client
        .call_tool(
            CallToolRequestParams::new("format").with_arguments(
                serde_json::json!({ "path": "m.compact", "write": true })
                    .as_object()
                    .unwrap()
                    .clone(),
            ),
        )
        .await
        .unwrap();
    assert_ne!(res.is_error, Some(true));

    let on_disk = std::fs::read_to_string(dir.path().join("m.compact")).unwrap();
    assert!(
        on_disk.contains("export ledger a: Counter;"),
        "write:true must rewrite the file to canonical form; got {on_disk:?}"
    );

    client.cancel().await.unwrap();
}

#[tokio::test]
async fn fixup_tool_surfaces_a_missing_toolchain_as_an_is_error_result() {
    // Hermetic mirror of the format missing-toolchain test, for the OTHER tool in
    // the pair — the one that mutates on `write: true`. A VALID contract sails
    // through the core parse gate and reaches `Toolchain::run`, where the bogus
    // binary fails fast with `ToolchainNotFound`. The `fixup` handler's
    // `Err(CoreError)` arm must surface it as `isError: true` carrying the
    // message, proving the tool name and error mapping are wired.
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join("x.compact"),
        "pragma language_version >= 0.23;\n",
    )
    .unwrap();
    let ws = compact_mcp_core::Workspace::new(dir.path()).unwrap();
    let tc = compact_mcp_core::Toolchain::new("compact-does-not-exist-xyz", None);

    let (client_t, server_t) = tokio::io::duplex(8192);
    tokio::spawn(async move {
        let _ = compact_mcp::server::CompactMcp::with_toolchain(ws, tc)
            .serve(server_t)
            .await
            .expect("server failed to start")
            .waiting()
            .await;
    });
    let client = ().serve(client_t).await.unwrap();

    let res = client
        .call_tool(
            CallToolRequestParams::new("fixup").with_arguments(
                serde_json::json!({ "path": "x.compact" })
                    .as_object()
                    .unwrap()
                    .clone(),
            ),
        )
        .await
        .unwrap();
    assert_eq!(
        res.is_error,
        Some(true),
        "missing toolchain must be an is_error result: {:?}",
        res.content
    );
    let text = format!("{:?}", res.content);
    assert!(
        text.contains("not found"),
        "error result must carry the not-found message: {text}"
    );

    client.cancel().await.unwrap();
}

#[tokio::test]
#[cfg_attr(not(feature = "toolchain-tests"), ignore)]
async fn fixup_tool_returns_result_without_touching_the_file() {
    // `fixup` with write defaulting to false is a no-op on already-canonical
    // input and must never mutate disk. Confirms the `fixup` tool name, the
    // `FixupArgs.path` deserialization, and the non-destructive default all wire
    // through the protocol layer.
    let dir = tempfile::tempdir().unwrap();
    let clean = "pragma language_version >= 0.23;\n\nimport CompactStandardLibrary;\n\nexport ledger a: Counter;\n";
    std::fs::write(dir.path().join("m.compact"), clean).unwrap();
    let ws = compact_mcp_core::Workspace::new(dir.path()).unwrap();

    let (client_t, server_t) = tokio::io::duplex(8192);
    tokio::spawn(async move {
        let _ = compact_mcp::server::CompactMcp::new(ws)
            .serve(server_t)
            .await
            .expect("server failed to start")
            .waiting()
            .await;
    });
    let client = ().serve(client_t).await.unwrap();

    let res = client
        .call_tool(
            CallToolRequestParams::new("fixup").with_arguments(
                serde_json::json!({ "path": "m.compact" })
                    .as_object()
                    .unwrap()
                    .clone(),
            ),
        )
        .await
        .unwrap();
    assert_ne!(res.is_error, Some(true));

    let on_disk = std::fs::read_to_string(dir.path().join("m.compact")).unwrap();
    assert_eq!(
        on_disk, clean,
        "write defaults to false; fixup must leave the file byte-identical"
    );

    client.cancel().await.unwrap();
}

#[tokio::test]
async fn compile_tool_surfaces_a_missing_toolchain_as_an_is_error_result() {
    // Hermetic: no real toolchain needed. `compile` does not parse-gate like
    // `format`/`fixup` — it goes straight to spawning the subprocess, so a
    // bogus binary fails fast with `ToolchainNotFound` before any compilation
    // is attempted. That `CoreError` must surface as a successful call with
    // `isError: true` carrying the message, never as an opaque protocol error.
    let dir = tempfile::tempdir().unwrap();
    std::fs::copy(
        "tests/fixtures/counter.compact",
        dir.path().join("c.compact"),
    )
    .unwrap();
    let ws = compact_mcp_core::Workspace::new(dir.path()).unwrap();
    let tc = compact_mcp_core::Toolchain::new("compact-does-not-exist-xyz", None);

    let (client_t, server_t) = tokio::io::duplex(8192);
    tokio::spawn(async move {
        let _ = compact_mcp::server::CompactMcp::with_toolchain(ws, tc)
            .serve(server_t)
            .await
            .expect("server failed to start")
            .waiting()
            .await;
    });
    let client = ().serve(client_t).await.unwrap();

    let res = client
        .call_tool(
            CallToolRequestParams::new("compile").with_arguments(
                serde_json::json!({ "path": "c.compact" })
                    .as_object()
                    .unwrap()
                    .clone(),
            ),
        )
        .await
        .unwrap();
    assert_eq!(
        res.is_error,
        Some(true),
        "missing toolchain must be an is_error result: {:?}",
        res.content
    );
    let text = format!("{:?}", res.content);
    assert!(
        text.contains("not found"),
        "error result must carry the not-found message: {text}"
    );

    client.cancel().await.unwrap();
}

#[tokio::test]
#[cfg_attr(not(feature = "toolchain-tests"), ignore)]
async fn a_failing_compile_returns_diagnostics_not_a_protocol_error() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::copy(
        "tests/fixtures/broken.compact",
        dir.path().join("b.compact"),
    )
    .unwrap();
    let ws = compact_mcp_core::Workspace::new(dir.path()).unwrap();

    let (client_t, server_t) = tokio::io::duplex(8192);
    tokio::spawn(async move {
        let _ = compact_mcp::server::CompactMcp::new(ws)
            .serve(server_t)
            .await
            .expect("server failed to start")
            .waiting()
            .await;
    });
    let client = ().serve(client_t).await.unwrap();

    let res = client
        .call_tool(
            CallToolRequestParams::new("compile").with_arguments(
                serde_json::json!({ "path": "b.compact" })
                    .as_object()
                    .unwrap()
                    .clone(),
            ),
        )
        .await
        .expect("the CALL succeeds; the CONTRACT fails");

    assert_eq!(res.is_error, Some(true));
    let text = format!("{:?}", res.content);
    assert!(text.contains("unbound identifier"), "{text}");
    assert!(text.contains("compactc"), "must be source-tagged: {text}");

    client.cancel().await.unwrap();
}

#[tokio::test]
#[cfg_attr(not(feature = "toolchain-tests"), ignore)]
async fn versions_tool_reports_both_parsers_and_a_skew_verdict() {
    let dir = tempfile::tempdir().unwrap();
    let ws = compact_mcp_core::Workspace::new(dir.path()).unwrap();
    let (client_t, server_t) = tokio::io::duplex(8192);
    tokio::spawn(async move {
        let _ = compact_mcp::server::CompactMcp::new(ws)
            .serve(server_t)
            .await
            .expect("server failed to start")
            .waiting()
            .await;
    });
    let client = ().serve(client_t).await.unwrap();

    let res = client
        .call_tool(rmcp::model::CallToolRequestParams::new("versions"))
        .await
        .unwrap();
    assert_ne!(res.is_error, Some(true));
    let text = format!("{:?}", res.content);
    for key in ["compiler", "language", "compactp", "skew"] {
        assert!(text.contains(key), "versions output missing {key}: {text}");
    }
    client.cancel().await.unwrap();
}
