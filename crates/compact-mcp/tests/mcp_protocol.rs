use rmcp::{ServiceExt, model::CallToolRequestParams};

const CI_FIXTURE: &str = r#"{
  "compiler-version":"0.31.1","language-version":"0.23.0","runtime-version":"0.16.0",
  "circuits":[
    {"name":"increment","pure":false,"proof":true,"arguments":[],"result-type":{"type-name":"Tuple","types":[]}},
    {"name":"reveal","pure":false,"proof":false,"arguments":[],"result-type":{"type-name":"Field"}}
  ],
  "witnesses":[],"contracts":[],"ledger":[]
}"#;
const ZKIR_FIXTURE: &str = r#"{"version":{"major":2,"minor":0},"do_communications_commitment":true,"num_inputs":0,"instructions":[{"op":"load_imm","imm":"01"},{"op":"add"}]}"#;

/// Builds a fake skip-zk target dir named `build` under `root`: parsed
/// contract-info.json with two circuits (one `proof:true`, one `proof:false`)
/// and a `.zkir` for only the `proof:true` circuit — `reveal` intentionally
/// has no zkir file, matching a real skip-zk build's layout.
fn write_build_fixture(root: &std::path::Path) {
    let compiler_dir = root.join("build/compiler");
    std::fs::create_dir_all(&compiler_dir).unwrap();
    std::fs::write(compiler_dir.join("contract-info.json"), CI_FIXTURE).unwrap();
    let zkir_dir = root.join("build/zkir");
    std::fs::create_dir_all(&zkir_dir).unwrap();
    std::fs::write(zkir_dir.join("increment.zkir"), ZKIR_FIXTURE).unwrap();
}

#[tokio::test]
async fn artifacts_tool_scans_a_build_dir() {
    let dir = tempfile::tempdir().unwrap();
    write_build_fixture(dir.path());
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
            CallToolRequestParams::new("artifacts").with_arguments(
                serde_json::json!({ "target_dir": "build" })
                    .as_object()
                    .unwrap()
                    .clone(),
            ),
        )
        .await
        .unwrap();
    assert_ne!(res.is_error, Some(true), "{:?}", res.content);
    let text = format!("{:?}", res.content);
    assert!(text.contains("increment"), "{text}");
    assert!(text.contains("reveal"), "{text}");
    assert!(text.contains("proving_keys"), "{text}");

    client.cancel().await.unwrap();
}

#[tokio::test]
async fn artifacts_tool_missing_dir_is_is_error_not_protocol_error() {
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
            CallToolRequestParams::new("artifacts").with_arguments(
                serde_json::json!({ "target_dir": "nope" })
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
        "a missing target dir must be an is_error result, not a protocol error: {:?}",
        res.content
    );
    let text = format!("{:?}", res.content);
    assert!(text.contains("artifact missing"), "{text}");

    client.cancel().await.unwrap();
}

#[tokio::test]
async fn zkir_stats_tool_unknown_circuit_lists_available() {
    let dir = tempfile::tempdir().unwrap();
    write_build_fixture(dir.path());
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
            CallToolRequestParams::new("zkir_stats").with_arguments(
                serde_json::json!({ "target_dir": "build", "circuit": "ghost" })
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
        "an unknown circuit must be an is_error result: {:?}",
        res.content
    );
    let text = format!("{:?}", res.content);
    assert!(text.contains("available"), "{text}");
    assert!(
        text.contains("increment"),
        "the agent must SEE the real circuit names: {text}"
    );

    client.cancel().await.unwrap();
}

#[tokio::test]
async fn zkir_stats_tool_reports_stats_and_absent() {
    let dir = tempfile::tempdir().unwrap();
    write_build_fixture(dir.path());
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
            CallToolRequestParams::new("zkir_stats").with_arguments(
                serde_json::json!({ "target_dir": "build" })
                    .as_object()
                    .unwrap()
                    .clone(),
            ),
        )
        .await
        .unwrap();
    assert_ne!(res.is_error, Some(true), "{:?}", res.content);
    let text = format!("{:?}", res.content);
    assert!(text.contains("increment"), "{text}");
    assert!(
        text.contains("load_imm"),
        "expected opcode histogram from the parsed zkir: {text}"
    );
    assert!(text.contains("absent"), "{text}");
    assert!(
        text.contains("reveal"),
        "reveal is proof:false and must be reported absent: {text}"
    );

    client.cancel().await.unwrap();
}

#[tokio::test]
async fn zkir_stats_tool_reports_proof_true_missing_zkir_honestly() {
    // A partial/incomplete build: `increment` is proof:true but its .zkir was
    // never written. The tool must report it absent with an HONEST reason, not
    // the false "proof:false" one that would apply only to `reveal`.
    let dir = tempfile::tempdir().unwrap();
    let compiler = dir.path().join("build/compiler");
    std::fs::create_dir_all(&compiler).unwrap();
    std::fs::write(compiler.join("contract-info.json"), CI_FIXTURE).unwrap();
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
            CallToolRequestParams::new("zkir_stats").with_arguments(
                serde_json::json!({ "target_dir": "build" })
                    .as_object()
                    .unwrap()
                    .clone(),
            ),
        )
        .await
        .unwrap();
    assert_ne!(res.is_error, Some(true), "{:?}", res.content);
    let text = format!("{:?}", res.content);
    assert!(
        text.contains("build may be incomplete"),
        "a proof:true circuit with no .zkir must be reported honestly, not as proof:false: {text}"
    );

    client.cancel().await.unwrap();
}

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
