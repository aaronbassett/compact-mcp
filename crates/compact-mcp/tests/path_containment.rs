//! MCP-boundary path-containment regression tests.
//!
//! `Workspace::resolve`'s containment LOGIC is exhaustively unit-tested in
//! `compact-mcp-core`. What was NOT tested is that each path-accepting tool
//! HANDLER actually routes its argument through `resolve` before touching the
//! filesystem. A reviewer proved the gap by hand: mutating the call site in
//! `server.rs::read_input` to skip `resolve()` left the entire suite green, so a
//! handler that forgot the call would ship a containment escape with no failing
//! test.
//!
//! These tests close that gap. They drive REAL `tools/call` requests with paths
//! that escape the workspace root and assert every one is rejected — exercising
//! the wiring, not `resolve` in isolation.
//!
//! ## Why the escape targets must EXIST
//!
//! The reviewer's mutation slipped through precisely because the escaping paths
//! in the existing tests pointed at nothing: with `resolve` bypassed, the read
//! still failed (file-not-found), so the tests stayed green for the wrong
//! reason. To make a bypass observable, every escape here points at a real file
//! or directory that lives OUTSIDE the root. With `resolve` wired the call is
//! rejected before the filesystem is touched; with `resolve` skipped the
//! out-of-root content is served instead — a difference the assertions detect.
//!
//! ## Hermetic
//!
//! Every covered handler performs its containment check BEFORE spawning any
//! `compact` subprocess, so these run in the fast CI job with no toolchain
//! installed — they need no `toolchain-tests` feature.

use rmcp::{
    RoleClient, ServiceExt,
    model::{CallToolRequestParams, CallToolResult},
    service::RunningService,
};

/// A unique identifier that appears ONLY in the out-of-root secret file. If it
/// ever surfaces in a tool result, containment was bypassed and the file leaked.
const MARKER: &str = "leaked_marker_a7f3";

/// A syntactically valid contract so a resolve-bypass would parse cleanly and
/// return success (not an incidental parse error) — making the leak unambiguous.
fn secret_source() -> String {
    format!(
        "pragma language_version >= 0.23;\nimport CompactStandardLibrary;\nexport ledger {MARKER}: Counter;\n"
    )
}

/// A minimal compiler-output `contract-info.json`, so the out-of-root secret
/// DIRECTORY is a scannable build. A resolve-bypass in an artifact tool would
/// then surface its circuit names (`increment`), proving the leak concretely.
const CI_FIXTURE: &str = r#"{
  "compiler-version":"0.31.1","language-version":"0.23.0","runtime-version":"0.16.0",
  "circuits":[
    {"name":"increment","pure":false,"proof":true,"arguments":[],"result-type":{"type-name":"Tuple","types":[]}}
  ],
  "witnesses":[],"contracts":[],"ledger":[]
}"#;

/// Placeholder source for the `compile` target-dir case: `read_input` only
/// size-checks inline source, so the containment check under test is the
/// separate `target_dir` resolve, reached only once the source is accepted.
const VALID_SOURCE: &str = "pragma language_version >= 0.23;\n";

/// A workspace root with secrets planted just OUTSIDE it, reachable only by
/// escaping the root. Laid out as:
///
/// ```text
/// base/
/// ├── secret.compact        (contains MARKER)          <- file escape target
/// ├── secret_dir/compiler/contract-info.json           <- dir  escape target
/// └── root/                 (the workspace root)
///     ├── link.compact  ->  base/secret.compact         (unix symlink)
///     └── link_dir       ->  base/secret_dir            (unix symlink)
/// ```
struct Escape {
    // Held only to keep the temp tree alive for the test's lifetime.
    _base: tempfile::TempDir,
    root: std::path::PathBuf,
    secret_file_abs: String,
    secret_dir_abs: String,
}

impl Escape {
    fn new() -> Self {
        let base = tempfile::tempdir().unwrap();
        let root = base.path().join("root");
        std::fs::create_dir(&root).unwrap();

        let secret_file = base.path().join("secret.compact");
        std::fs::write(&secret_file, secret_source()).unwrap();

        // `scan(target_dir)` reads `target_dir/compiler/contract-info.json`, so
        // the build lives directly under the secret dir.
        let secret_dir = base.path().join("secret_dir");
        let compiler = secret_dir.join("compiler");
        std::fs::create_dir_all(&compiler).unwrap();
        std::fs::write(compiler.join("contract-info.json"), CI_FIXTURE).unwrap();

        // Symlinks INSIDE the root whose targets are OUTSIDE it. `resolve`
        // canonicalizes through the link and rejects the out-of-root target.
        #[cfg(unix)]
        {
            std::os::unix::fs::symlink(&secret_file, root.join("link.compact")).unwrap();
            std::os::unix::fs::symlink(&secret_dir, root.join("link_dir")).unwrap();
        }

        Self {
            secret_file_abs: secret_file.to_string_lossy().into_owned(),
            secret_dir_abs: secret_dir.to_string_lossy().into_owned(),
            root,
            _base: base,
        }
    }

    /// `path` values that escape to the out-of-root secret FILE.
    fn file_vectors(&self) -> Vec<(&'static str, String)> {
        let mut v = vec![
            ("dotdot traversal", "../secret.compact".to_string()),
            ("absolute path", self.secret_file_abs.clone()),
        ];
        #[cfg(unix)]
        v.push(("symlink escape", "link.compact".to_string()));
        v
    }

    /// `target_dir` values that escape to the out-of-root secret DIRECTORY.
    fn dir_vectors(&self) -> Vec<(&'static str, String)> {
        let mut v = vec![
            ("dotdot traversal", "../secret_dir".to_string()),
            ("absolute path", self.secret_dir_abs.clone()),
        ];
        #[cfg(unix)]
        v.push(("symlink escape", "link_dir".to_string()));
        v
    }
}

/// Serve a `CompactMcp` over the workspace `root` and return an in-process
/// client. Mirrors the duplex harness the other integration tests use, kept
/// feature-free so it also compiles under a bare `cargo test`.
async fn client_for(root: &std::path::Path) -> RunningService<RoleClient, ()> {
    let ws = compact_mcp_core::Workspace::new(root).unwrap();
    let (client_t, server_t) = tokio::io::duplex(1 << 20);
    tokio::spawn(async move {
        let _ = compact_mcp::server::CompactMcp::new(ws)
            .serve(server_t)
            .await
            .expect("server failed to start")
            .waiting()
            .await;
    });
    ().serve(client_t).await.unwrap()
}

fn call(tool: &str, args: serde_json::Value) -> CallToolRequestParams {
    CallToolRequestParams::new(tool.to_string()).with_arguments(args.as_object().unwrap().clone())
}

/// Assert an escaping-path tool call was rejected on containment grounds, and
/// that no out-of-root content leaked.
///
/// Handlers signal a containment rejection in one of two shapes, and this
/// accepts both:
///  * A protocol error (`Err`) — used by handlers whose `path` flows through
///    `read_input`, which maps `PathEscape` to `McpError::invalid_params`.
///  * A successful call with `is_error: true` (`Ok`) — used by handlers that
///    map the `CoreError` into the result body.
///
/// In BOTH shapes the rejection MUST cite the path-escape (`"escapes workspace
/// root"`). That specificity is what makes the test bite: a bypassed handler
/// fails with an unrelated error (`"not found"`, `"artifact missing"`) or, worse,
/// SUCCEEDS and serves the secret — never with the escape message.
fn assert_rejected<E: std::fmt::Debug>(
    tool: &str,
    label: &str,
    leaked: &str,
    result: Result<CallToolResult, E>,
) {
    const ESCAPE: &str = "escapes workspace root";
    match result {
        Err(e) => {
            let msg = format!("{e:?}");
            assert!(
                msg.contains(ESCAPE),
                "{tool} ({label}): a rejection must cite path containment, got: {msg}"
            );
        }
        Ok(res) => {
            let text = format!("{:?}", res.content);
            assert!(
                !text.contains(leaked),
                "{tool} ({label}): out-of-root content leaked through the boundary: {text}"
            );
            assert_eq!(
                res.is_error,
                Some(true),
                "{tool} ({label}): an escaping path must be rejected, not served: {text}"
            );
            assert!(
                text.contains(ESCAPE),
                "{tool} ({label}): rejection must cite path containment, not another error: {text}"
            );
        }
    }
}

/// The four analysis tools take `path`/`source` and route it through
/// `server.rs::read_input` — the EXACT call site the reviewer mutated. These are
/// the sole guard for those handlers (unlike `compile`/`witness_scaffold`, which
/// re-resolve downstream), so bypassing `read_input`'s `resolve()` opens all four
/// and this test is the direct regression guard for that mutation.
#[tokio::test]
async fn analysis_tools_reject_escaping_paths() {
    let f = Escape::new();
    let client = client_for(&f.root).await;

    for tool in ["diagnostics", "ast", "symbols", "stats"] {
        for (label, path) in f.file_vectors() {
            let res = client
                .call_tool(call(tool, serde_json::json!({ "path": path })))
                .await;
            assert_rejected(tool, label, MARKER, res);
        }
    }

    client.cancel().await.unwrap();
}

/// `format` and `fixup` resolve their `path` inside `Toolchain::rewrite` (core),
/// BEFORE the parse gate and any formatter subprocess. Guards that call site.
#[tokio::test]
async fn format_and_fixup_reject_escaping_paths() {
    let f = Escape::new();
    let client = client_for(&f.root).await;

    for tool in ["format", "fixup"] {
        for (label, path) in f.file_vectors() {
            let res = client
                .call_tool(call(tool, serde_json::json!({ "path": path })))
                .await;
            assert_rejected(tool, label, MARKER, res);
        }
    }

    client.cancel().await.unwrap();
}

/// `artifacts` and `zkir_stats` resolve `target_dir` in their `*_impl` before
/// scanning the filesystem. A bypass would scan the out-of-root build and leak
/// its circuit names.
#[tokio::test]
async fn artifact_tools_reject_escaping_target_dirs() {
    let f = Escape::new();
    let client = client_for(&f.root).await;

    for tool in ["artifacts", "zkir_stats"] {
        for (label, dir) in f.dir_vectors() {
            let res = client
                .call_tool(call(tool, serde_json::json!({ "target_dir": dir })))
                .await;
            // `increment` is the circuit a bypassed scan would surface.
            assert_rejected(tool, label, "increment", res);
        }
    }

    client.cancel().await.unwrap();
}

/// `compile` accepts BOTH an escaping `path` (rejected by `read_input` and again
/// by `compile_impl`) and an escaping `target_dir` (rejected by `compile_impl`
/// alone). The `target_dir` resolve runs before the build gate and subprocess,
/// so with valid inline source this stays hermetic.
#[tokio::test]
async fn compile_rejects_escaping_path_and_target_dir() {
    let f = Escape::new();
    let client = client_for(&f.root).await;

    for (label, path) in f.file_vectors() {
        let res = client
            .call_tool(call("compile", serde_json::json!({ "path": path })))
            .await;
        assert_rejected("compile (path)", label, MARKER, res);
    }

    for (label, dir) in f.dir_vectors() {
        let res = client
            .call_tool(call(
                "compile",
                serde_json::json!({ "source": VALID_SOURCE, "target_dir": dir }),
            ))
            .await;
        assert_rejected("compile (target_dir)", label, "increment", res);
    }

    client.cancel().await.unwrap();
}

/// `witness_scaffold` resolves its `path` via `read_input` (and again in its
/// `_impl`) before the `--skip-zk` compile, so the escape is caught with no
/// toolchain present.
#[tokio::test]
async fn witness_scaffold_rejects_escaping_paths() {
    let f = Escape::new();
    let client = client_for(&f.root).await;

    for (label, path) in f.file_vectors() {
        let res = client
            .call_tool(call(
                "witness_scaffold",
                serde_json::json!({ "path": path }),
            ))
            .await;
        assert_rejected("witness_scaffold", label, MARKER, res);
    }

    client.cancel().await.unwrap();
}
