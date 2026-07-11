use std::sync::Arc;
use std::time::Duration;

use compact_mcp_core::jobs::{BuildGate, TaskStore};
use compact_mcp_core::{Toolchain, Workspace};
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

    /// The task's cancel token when running inside `enqueue_task`'s scope, else a
    /// fresh (never-cancelled) token for a plain synchronous tool call.
    pub(crate) fn current_cancel_token(&self) -> tokio_util::sync::CancellationToken {
        TASK_CANCEL.try_with(|t| t.clone()).unwrap_or_default()
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
}
