use compact_mcp_core::{Toolchain, Workspace};
use rmcp::{
    ErrorData as McpError, ServerHandler,
    handler::server::router::tool::ToolRouter,
    model::{CallToolResult, ContentBlock, Implementation, ServerCapabilities, ServerInfo},
    tool_handler,
};

#[derive(Clone)]
pub struct CompactMcp {
    pub(crate) workspace: Workspace,
    pub(crate) toolchain: Toolchain,
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
            tool_router: Self::analysis_router()
                + Self::toolchain_router()
                + Self::fmt_router()
                + Self::compile_router(),
        }
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
        ServerInfo::new(ServerCapabilities::builder().enable_tools().build())
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
