use compact_mcp_core::toolchain::fmt::FmtInput;
use rmcp::{
    ErrorData as McpError, handler::server::wrapper::Parameters, model::CallToolResult, schemars,
    tool, tool_router,
};

use crate::server::CompactMcp;
use crate::tools::SourceInput;

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
pub struct FormatArgs {
    #[serde(flatten)]
    pub input: SourceInput,
    /// Rewrite the file in place. Requires `path`. Defaults to false, in which
    /// case the formatted text is returned and nothing on disk changes.
    #[serde(default)]
    pub write: bool,
}

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
pub struct FixupArgs {
    pub path: String,
    #[serde(default)]
    pub write: bool,
}

#[tool_router(router = fmt_router, vis = "pub(crate)")]
impl CompactMcp {
    #[tool(
        description = "Format Compact source. Returns the formatted text by default and \
                          leaves the file untouched; pass write:true to rewrite in place. \
                          If the source does not parse, returns parse diagnostics instead."
    )]
    async fn format(
        &self,
        Parameters(args): Parameters<FormatArgs>,
    ) -> Result<CallToolResult, McpError> {
        let input = match (&args.input.path, &args.input.source) {
            (Some(p), None) => FmtInput::Path(p.clone()),
            (None, Some(s)) => FmtInput::Source(s.clone()),
            _ => {
                return Err(McpError::invalid_params(
                    "supply exactly one of `path` or `source`".to_string(),
                    None,
                ));
            }
        };
        // A `CoreError` here (e.g. `compact` not on PATH, or a `ToolchainFailed`
        // from the formatter subprocess) is a runtime/domain failure, not a
        // request-shape problem. rmcp renders `Err(McpError)` opaquely — the
        // caller would see "internal error", never our message — so we surface
        // it as a successful call with `isError: true`, matching the toolchain
        // tools. `McpError` stays reserved for bad request shapes (the XOR check
        // above).
        match self
            .toolchain
            .format(&self.workspace, input, args.write)
            .await
        {
            Ok(out) => {
                let is_error = !out.ok;
                Ok(Self::json_result(
                    serde_json::to_value(out).unwrap(),
                    is_error,
                ))
            }
            Err(e) => Ok(Self::json_result(
                serde_json::json!({ "error": e.to_string() }),
                true,
            )),
        }
    }

    #[tool(
        description = "Apply Compact fixup transformations. Returns the fixed text by \
                          default; pass write:true to rewrite in place. \
                          If the source does not parse, returns parse diagnostics instead."
    )]
    async fn fixup(
        &self,
        Parameters(args): Parameters<FixupArgs>,
    ) -> Result<CallToolResult, McpError> {
        match self
            .toolchain
            .fixup(&self.workspace, &args.path, args.write)
            .await
        {
            Ok(out) => {
                let is_error = !out.ok;
                Ok(Self::json_result(
                    serde_json::to_value(out).unwrap(),
                    is_error,
                ))
            }
            Err(e) => Ok(Self::json_result(
                serde_json::json!({ "error": e.to_string() }),
                true,
            )),
        }
    }
}
