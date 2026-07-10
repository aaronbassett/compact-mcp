use rmcp::{
    ErrorData as McpError, handler::server::wrapper::Parameters, model::CallToolResult, schemars,
    tool, tool_router,
};

use crate::server::CompactMcp;
use crate::tools::SourceInput;

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
pub struct DiagnosticsArgs {
    #[serde(flatten)]
    pub input: SourceInput,
    /// Cap the diagnostics returned. `error_count` still reports the true total.
    pub max_diagnostics: Option<usize>,
}

#[tool_router(router = analysis_router, vis = "pub(crate)")]
impl CompactMcp {
    #[tool(
        description = "Parse a Compact file and report every syntax diagnostic. \
                          Does not invoke the compiler. Milliseconds."
    )]
    async fn diagnostics(
        &self,
        Parameters(args): Parameters<DiagnosticsArgs>,
    ) -> Result<CallToolResult, McpError> {
        let (text, file) = self.read_input(&args.input)?;
        let out = compact_mcp_core::analyze::diagnostics(&text, &file, args.max_diagnostics);
        let is_error = !out.success;
        Ok(Self::json_result(
            serde_json::to_value(out).unwrap(),
            is_error,
        ))
    }

    #[tool(description = "Dump the typed AST item list of a Compact file.")]
    async fn ast(
        &self,
        Parameters(input): Parameters<SourceInput>,
    ) -> Result<CallToolResult, McpError> {
        let (text, _) = self.read_input(&input)?;
        Ok(Self::json_result(
            compact_mcp_core::analyze::ast_json(&text),
            false,
        ))
    }

    #[tool(
        description = "List every declaration (ledger, circuit, witness, struct, enum) \
                          with its exported/sealed/pure status."
    )]
    async fn symbols(
        &self,
        Parameters(input): Parameters<SourceInput>,
    ) -> Result<CallToolResult, McpError> {
        let (text, _) = self.read_input(&input)?;
        let s = compact_mcp_core::analyze::symbols(&text);
        Ok(Self::json_result(serde_json::to_value(s).unwrap(), false))
    }

    #[tool(description = "Token, node, error and recovery counts, plus parse time.")]
    async fn stats(
        &self,
        Parameters(input): Parameters<SourceInput>,
    ) -> Result<CallToolResult, McpError> {
        let (text, _) = self.read_input(&input)?;
        let s = compact_mcp_core::analyze::stats(&text);
        Ok(Self::json_result(serde_json::to_value(s).unwrap(), false))
    }
}
