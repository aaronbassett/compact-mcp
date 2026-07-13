use std::sync::Arc;
use std::time::Duration;

use compact_mcp_core::jobs::{BuildGate, TaskStore};
use compact_mcp_core::{CoreError, Toolchain, Workspace};
use rmcp::{
    ErrorData as McpError, RoleServer, ServerHandler,
    handler::server::router::tool::ToolRouter,
    model::{
        CallToolRequestParams, CallToolResult, CancelTaskParams, CancelTaskResult, ContentBlock,
        CreateTaskResult, GetTaskParams, GetTaskPayloadParams, GetTaskPayloadResult, GetTaskResult,
        Implementation, ListTasksResult, PaginatedRequestParams, ServerCapabilities, ServerInfo,
    },
    service::RequestContext,
    tool_handler,
};
use tokio::sync::OwnedSemaphorePermit;
use tokio_util::sync::CancellationToken;

tokio::task_local! {
    pub(crate) static TASK_CANCEL: tokio_util::sync::CancellationToken;
}

#[derive(Clone)]
pub struct CompactMcp {
    pub(crate) workspace: Workspace,
    pub(crate) toolchain: Toolchain,
    pub(crate) tasks: Arc<TaskStore>,
    pub(crate) gate: Arc<BuildGate>,
    pub(crate) compile_timeout_secs: u64,
    /// stdio: true. HTTP: false — we cannot identify requestors.
    pub(crate) advertise_task_list: bool,
    tool_router: ToolRouter<CompactMcp>,
}

impl CompactMcp {
    pub fn new(workspace: Workspace) -> Self {
        Self::with_toolchain(workspace, Toolchain::new("compact", None))
    }

    pub fn with_toolchain(workspace: Workspace, toolchain: Toolchain) -> Self {
        Self {
            workspace,
            toolchain,
            tasks: Arc::new(TaskStore::new(
                Duration::from_secs(900),
                Duration::from_secs(3600),
            )),
            gate: Arc::new(BuildGate::new(1, 8)),
            compile_timeout_secs: 900,
            advertise_task_list: true,
            tool_router: Self::analysis_router()
                + Self::toolchain_router()
                + Self::fmt_router()
                + Self::compile_router()
                + Self::artifacts_router(),
        }
    }

    /// Apply the CLI config. Called from `main`.
    pub fn with_config(mut self, c: &crate::config::Config) -> Self {
        // Rebuild the toolchain so `--compact-bin` / `--compiler-version` actually
        // take effect. Without this both flags are parsed but then dropped on the
        // floor: the server keeps the default `Toolchain::new("compact", None)`,
        // always shelling out to whatever `compact` is on `$PATH`, unpinned.
        // `compiler_version` reaches `compact compile` as the leading `+VERSION`
        // token via `Toolchain::compile`. Regression-guarded in this module's
        // tests. (#1)
        self.toolchain = Toolchain::new(c.compact_bin.as_str(), c.compiler_version.clone());
        self.tasks = Arc::new(TaskStore::new(
            Duration::from_secs(c.default_task_ttl),
            Duration::from_secs(c.max_task_ttl),
        ));
        self.gate = Arc::new(BuildGate::new(c.max_concurrent_builds, c.max_queued_builds));
        self.compile_timeout_secs = c.compile_timeout;
        self
    }

    /// Whether to advertise and answer `tasks/list`. Stdio (a single trusted
    /// client) leaves it on; the HTTP transport (Task 23) must set it `false`,
    /// because `tasks/list` has no per-caller scoping and the id is the only
    /// secret. The setter exists so that path has a seam to flip it.
    pub fn with_advertise_task_list(mut self, advertise: bool) -> Self {
        self.advertise_task_list = advertise;
        self
    }

    /// Like [`new`](Self::new) but for the HTTP transport, which cannot identify
    /// requestors and so must not advertise `tasks/list`.
    pub fn new_http(workspace: Workspace) -> Self {
        Self::new(workspace).with_advertise_task_list(false)
    }

    /// Registers `toolchain_update` / `toolchain_clean`. Off by default: over HTTP
    /// these are remote "install a binary" and "delete every compiler" primitives.
    pub fn with_toolchain_mutation(mut self, allow: bool) -> Self {
        if allow {
            self.tool_router += Self::mutation_router();
        }
        self
    }

    /// The task's cancel token when running inside `enqueue_task`'s scope, else a
    /// fresh (never-cancelled) token for a plain synchronous tool call.
    pub(crate) fn current_cancel_token(&self) -> tokio_util::sync::CancellationToken {
        TASK_CANCEL.try_with(|t| t.clone()).unwrap_or_default()
    }

    /// Acquire a permit from the shared [`BuildGate`] before spawning ANY
    /// `compact` subprocess, releasing it (RAII) when the returned permit drops
    /// on completion or cancel.
    ///
    /// Every compiler-spawning tool takes its permit from this ONE gate — the
    /// heavy `compile`/`witness_scaffold` builds AND the lighter `format`/
    /// `fixup`/`versions`/`toolchain_list`/`toolchain_check`/`toolchain_update`/
    /// `toolchain_clean` probes — so `max_concurrent_builds` is a real GLOBAL
    /// ceiling on concurrent `compact` processes rather than a per-`compile`
    /// limit. A call holds its single permit for the WHOLE operation (e.g.
    /// `versions`, which shells out up to five times, runs them serially under
    /// one permit — never five), so the gate can never be double-acquired within
    /// one handler.
    ///
    /// `biased`: check cancellation FIRST so a call cancelled WHILE QUEUED frees
    /// its slot at once — dropping the `acquire` future runs the gate's
    /// queue-counter guard — instead of holding it until its turn finally comes
    /// up. A fresh sync-call token never fires this branch.
    pub(crate) async fn acquire_gate(
        &self,
        ct: &CancellationToken,
    ) -> Result<OwnedSemaphorePermit, CoreError> {
        tokio::select! {
            biased;
            _ = ct.cancelled() => Err(CoreError::Cancelled),
            p = self.gate.acquire() => p,
        }
    }

    /// The shared task registry, for wiring up the retention GC loop from
    /// `main` (a separate binary crate, so the field itself stays `pub(crate)`).
    pub fn tasks(&self) -> Arc<TaskStore> {
        self.tasks.clone()
    }

    /// Resolve a `path`-or-`source` argument into `(source_text, file_label)`.
    /// Exactly one must be supplied.
    pub(crate) fn read_input(
        &self,
        input: &crate::tools::SourceInput,
    ) -> Result<(String, String), McpError> {
        match (&input.path, &input.source) {
            (Some(p), None) => {
                let resolved = self
                    .workspace
                    .resolve(p)
                    .map_err(|e| McpError::invalid_params(e.to_string(), None))?;
                // An empty (or `.`) `path` resolves to the workspace root
                // itself, which is a directory. Catch that before hitting
                // the filesystem so the caller gets "`path` is a directory"
                // instead of the far more confusing "Is a directory (os
                // error 21)" from `read_to_string`.
                if resolved.is_dir() {
                    return Err(McpError::invalid_params(
                        format!("`path` must be a file, not a directory: {p:?}"),
                        None,
                    ));
                }
                // Check the file's size before reading it into memory, so a huge file is
                // never fully buffered just to be rejected.
                let len = std::fs::metadata(&resolved)
                    .map_err(|e| McpError::invalid_params(e.to_string(), None))?
                    .len() as usize;
                if len > compact_mcp_core::MAX_SOURCE_BYTES {
                    return Err(McpError::invalid_params(
                        format!(
                            "input too large: {len} bytes (max {})",
                            compact_mcp_core::MAX_SOURCE_BYTES
                        ),
                        None,
                    ));
                }
                let text = std::fs::read_to_string(&resolved)
                    .map_err(|e| McpError::invalid_params(e.to_string(), None))?;
                Ok((text, p.clone()))
            }
            (None, Some(s)) => {
                if s.len() > compact_mcp_core::MAX_SOURCE_BYTES {
                    return Err(McpError::invalid_params(
                        format!(
                            "input too large: {} bytes (max {})",
                            s.len(),
                            compact_mcp_core::MAX_SOURCE_BYTES
                        ),
                        None,
                    ));
                }
                Ok((s.clone(), "<inline>".to_string()))
            }
            _ => Err(McpError::invalid_params(
                "supply exactly one of `path` or `source`".to_string(),
                None,
            )),
        }
    }

    pub(crate) fn json_result(value: serde_json::Value, is_error: bool) -> CallToolResult {
        let block = vec![ContentBlock::text(
            serde_json::to_string_pretty(&value).unwrap_or_else(|_| value.to_string()),
        )];
        if is_error {
            CallToolResult::error(block)
        } else {
            CallToolResult::success(block)
        }
    }
}

#[tool_handler(router = self.tool_router)]
impl ServerHandler for CompactMcp {
    fn get_info(&self) -> ServerInfo {
        // `Implementation::from_build_env()` reads `CARGO_CRATE_NAME`/
        // `CARGO_PKG_VERSION` via `env!`, which resolves at the *macro
        // invocation site* — inside the `rmcp` crate itself, not ours. Using
        // it here would report `rmcp`/`2.2.0` instead of this binary's
        // identity, so build `Implementation` from our own crate's env vars.
        let mut caps = ServerCapabilities::builder().enable_tools().build();
        let mut tasks = rmcp::model::TasksCapability::server_default();
        // HTTP cannot identify requestors, so `tasks/list` (which has no
        // per-caller scoping) is withheld on that transport; stdio's single
        // trusted client keeps it.
        if !self.advertise_task_list {
            tasks.list = None;
        }
        caps.tasks = Some(tasks);
        ServerInfo::new(caps)
            .with_server_info(Implementation::new(
                env!("CARGO_PKG_NAME"),
                env!("CARGO_PKG_VERSION"),
            ))
            .with_instructions(
                "Compact toolchain for Midnight. `diagnostics` parses without invoking the \
                 compiler and reports every error; `compile` invokes compactc, which stops at \
                 the FIRST error. Diagnostics are tagged with `source` so you can tell which \
                 tool spoke."
                    .to_string(),
            )
    }

    async fn enqueue_task(
        &self,
        request: CallToolRequestParams,
        context: RequestContext<RoleServer>,
    ) -> Result<CreateTaskResult, McpError> {
        self.enqueue_task_impl(request, context).await
    }

    async fn get_task_info(
        &self,
        request: GetTaskParams,
        _context: RequestContext<RoleServer>,
    ) -> Result<GetTaskResult, McpError> {
        self.get_task_info_impl(request).await
    }

    async fn get_task_result(
        &self,
        request: GetTaskPayloadParams,
        _context: RequestContext<RoleServer>,
    ) -> Result<GetTaskPayloadResult, McpError> {
        self.get_task_result_impl(request).await
    }

    async fn cancel_task(
        &self,
        request: CancelTaskParams,
        _context: RequestContext<RoleServer>,
    ) -> Result<CancelTaskResult, McpError> {
        self.cancel_task_impl(request).await
    }

    async fn list_tasks(
        &self,
        request: Option<PaginatedRequestParams>,
        _context: RequestContext<RoleServer>,
    ) -> Result<ListTasksResult, McpError> {
        self.list_tasks_impl(request).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tools::SourceInput;

    fn server() -> (tempfile::TempDir, CompactMcp) {
        let d = tempfile::tempdir().unwrap();
        let ws = compact_mcp_core::Workspace::new(d.path()).unwrap();
        (d, CompactMcp::new(ws))
    }

    fn si(path: Option<&str>, source: Option<&str>) -> SourceInput {
        SourceInput {
            path: path.map(Into::into),
            source: source.map(Into::into),
        }
    }

    #[tokio::test]
    async fn http_refuses_list_tasks_at_the_method_level_not_just_the_advertisement() {
        // Defense in depth: withholding `tasks/list` from the HTTP capability is
        // only half of it — the METHOD itself must refuse too, so a client that
        // calls it anyway (ignoring the advertisement) can't enumerate task ids
        // on a transport that can't identify requestors. Guards against a future
        // refactor silently re-enabling it. stdio still answers it.
        let d = tempfile::tempdir().unwrap();
        let ws = compact_mcp_core::Workspace::new(d.path()).unwrap();

        assert!(
            CompactMcp::new(ws.clone())
                .list_tasks_impl(None)
                .await
                .is_ok()
        );

        let err = CompactMcp::new_http(ws)
            .list_tasks_impl(None)
            .await
            .unwrap_err();
        assert_eq!(err.code, rmcp::model::ErrorCode::METHOD_NOT_FOUND);
    }

    #[test]
    fn rejects_both_path_and_source() {
        let (_d, s) = server();
        assert!(s.read_input(&si(Some("a.compact"), Some("x"))).is_err());
    }

    #[test]
    fn rejects_neither_path_nor_source() {
        let (_d, s) = server();
        assert!(s.read_input(&si(None, None)).is_err());
    }

    #[test]
    fn rejects_directory_path() {
        let (d, s) = server();
        std::fs::create_dir(d.path().join("sub")).unwrap();
        assert!(s.read_input(&si(Some("sub"), None)).is_err());
    }

    #[test]
    fn rejects_oversized_inline_source() {
        let (_d, s) = server();
        let big = "a".repeat(compact_mcp_core::MAX_SOURCE_BYTES + 1);
        let err = s.read_input(&si(None, Some(&big))).unwrap_err();
        assert!(format!("{err:?}").contains("too large"));
    }

    #[test]
    fn rejects_oversized_file() {
        let (d, s) = server();
        std::fs::write(
            d.path().join("big.compact"),
            vec![b'a'; compact_mcp_core::MAX_SOURCE_BYTES + 1],
        )
        .unwrap();
        let err = s.read_input(&si(Some("big.compact"), None)).unwrap_err();
        assert!(format!("{err:?}").contains("too large"));
    }

    #[test]
    fn accepts_normal_inline_source() {
        let (_d, s) = server();
        let (text, label) = s.read_input(&si(None, Some("ledger x: Counter;"))).unwrap();
        assert_eq!(text, "ledger x: Counter;");
        assert_eq!(label, "<inline>");
    }

    /// Regression guard for #1: `--compact-bin` / `--compiler-version` were parsed
    /// and validated but never rebuilt into the server's `Toolchain`, so both were
    /// silent no-ops — the server always ran whatever `compact` was on `$PATH`,
    /// unpinned.
    ///
    /// This drives a real `compile` through a server built via the full
    /// `Config::parse_from` -> `with_config` boundary and asserts on the captured
    /// argv: the configured stub binary is the one executed (`argv[0]`), and the
    /// `+VERSION` pin reaches `compact compile`. If the wiring is removed,
    /// `with_config` leaves the default `Toolchain::new("compact", None)`, the stub
    /// never runs, its argv log is never written, and this test fails.
    #[cfg(unix)]
    #[tokio::test]
    async fn with_config_wires_compact_bin_and_compiler_version_into_the_compile_argv() {
        use std::io::Write;
        use std::os::unix::fs::PermissionsExt;

        use clap::Parser;
        use compact_mcp_core::toolchain::compile::CompileRequest;
        use tokio_util::sync::CancellationToken;

        let dir = tempfile::tempdir().unwrap();

        // A stub `compact` that appends its own path (`$0`) and every argument,
        // one per line, to `<self>.argv`. Self-contained: no path is baked in, so
        // it records exactly the binary the toolchain chose to exec.
        let stub = dir.path().join("stub-compact");
        {
            let mut f = std::fs::File::create(&stub).unwrap();
            writeln!(f, "#!/bin/sh").unwrap();
            writeln!(f, "printf '%s\\n' \"$0\" \"$@\" >> \"$0.argv\"").unwrap();
            writeln!(f, "exit 0").unwrap();
        }
        std::fs::set_permissions(&stub, std::fs::Permissions::from_mode(0o755)).unwrap();
        std::fs::write(dir.path().join("c.compact"), "ledger x: Counter;").unwrap();

        let ws = compact_mcp_core::Workspace::new(dir.path()).unwrap();
        let cfg = crate::config::Config::parse_from([
            "compact-mcp",
            "--compact-bin",
            stub.to_str().unwrap(),
            "--compiler-version",
            "0.42.0",
        ]);
        let server = CompactMcp::new(ws).with_config(&cfg);

        let req = CompileRequest {
            source: dir.path().join("c.compact"),
            target_dir: dir.path().join("out"),
            skip_zk: true,
            no_communications_commitment: false,
            source_root: None,
        };
        server
            .toolchain
            .compile(&req, CancellationToken::new(), Duration::from_secs(30))
            .await
            .expect("stub compile should exit 0");

        let argv = std::fs::read_to_string(dir.path().join("stub-compact.argv")).expect(
            "the configured --compact-bin stub was never executed: `with_config` \
             dropped the toolchain wiring",
        );
        let lines: Vec<&str> = argv.lines().collect();
        assert_eq!(
            lines.first().copied(),
            stub.to_str(),
            "argv[0] must be the configured --compact-bin, got {argv:?}",
        );
        assert!(
            lines.contains(&"compile"),
            "compile subcommand missing from argv: {argv:?}",
        );
        assert!(
            lines.contains(&"+0.42.0"),
            "--compiler-version did not reach `compact compile` as +VERSION: {argv:?}",
        );
    }
}
