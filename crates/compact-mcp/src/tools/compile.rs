use std::time::Duration;

use compact_mcp_core::CoreError;
use compact_mcp_core::toolchain::compile::{CompileOutcome, CompileRequest};
use rmcp::{
    ErrorData as McpError, handler::server::wrapper::Parameters, model::CallToolResult, schemars,
    tool, tool_router,
};

use crate::server::CompactMcp;
use crate::tools::SourceInput;

fn default_true() -> bool {
    true
}

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
pub struct CompileArgs {
    #[serde(flatten)]
    pub input: SourceInput,
    /// Output directory, relative to the workspace root. Defaults to `build`.
    pub target_dir: Option<String>,
    /// Skip PLONK proving-key generation. **Defaults to true.** A skip_zk build
    /// still emits contract-info.json, index.d.ts and .zkir — only the keys are
    /// missing. Set false for a deployable build; prefer invoking as a task.
    #[serde(default = "default_true")]
    pub skip_zk: bool,
    #[serde(default)]
    pub no_communications_commitment: bool,
    pub source_root: Option<String>,
}

#[tool_router(router = compile_router, vis = "pub(crate)")]
impl CompactMcp {
    #[tool(
        description = "Compile a Compact contract with compactc. Defaults to skip_zk:true \
                       (seconds; emits TypeScript bindings, contract-info.json and .zkir but \
                       no proving keys). Set skip_zk:false to generate PLONK proving keys — \
                       cost scales with circuit size; strongly prefer invoking as a task. \
                       compactc reports only the FIRST error and stops; use `diagnostics` \
                       to see every syntax error at once.",
        execution(task_support = "optional")
    )]
    async fn compile(
        &self,
        Parameters(args): Parameters<CompileArgs>,
    ) -> Result<CallToolResult, McpError> {
        let (text, _label) = self.read_input(&args.input)?;

        // A `CoreError` from resolving the workspace, scratch files, or the
        // compiler subprocess itself is a runtime/domain failure, not a
        // request-shape problem. rmcp renders `Err(McpError)` opaquely — the
        // caller would see "internal error", never our message — so every
        // `CoreError` here surfaces as a successful call with `isError: true`,
        // matching the other toolchain tools. `McpError` stays reserved for the
        // `path`/`source` XOR check above.
        match self.compile_impl(&args, text).await {
            Ok((out, target_dir)) => {
                let is_error = !out.ok;
                let mut value = serde_json::to_value(out).unwrap();
                value["target_dir"] = serde_json::json!(target_dir.to_string_lossy());
                Ok(Self::json_result(value, is_error))
            }
            Err(e) => Ok(Self::json_result(
                serde_json::json!({ "error": e.to_string() }),
                true,
            )),
        }
    }
}

impl CompactMcp {
    /// The fallible core of `compile`, pulled out of the `#[tool]` handler so it
    /// can return `CoreError` directly and be mapped once at the call site.
    async fn compile_impl(
        &self,
        args: &CompileArgs,
        text: String,
    ) -> Result<(CompileOutcome, std::path::PathBuf), CoreError> {
        // `compactc` needs a real path. Inline source goes to a scoped temp file.
        // CRITICAL: `_scope` must stay alive until this function returns — bound
        // to a local here so the temp file exists for the whole compilation.
        let _scope;
        let source = match (&args.input.path, &args.input.source) {
            (Some(p), _) => self.workspace.resolve(p)?,
            _ => {
                let scope = self.workspace.temp_scope("compile")?;
                let p = scope.write_file("input.compact", &text)?;
                _scope = scope;
                p
            }
        };

        let target_dir = self
            .workspace
            .resolve(args.target_dir.as_deref().unwrap_or("build"))?;

        let req = CompileRequest {
            source,
            target_dir: target_dir.clone(),
            skip_zk: args.skip_zk,
            no_communications_commitment: args.no_communications_commitment,
            source_root: args.source_root.clone(),
        };

        // Serialize builds through the gate; a full queue is a CoreError the
        // handler surfaces as isError (never an opaque McpError). Hold the permit
        // for the whole compile. Use the task cancel token when running as a task
        // (a fresh, never-cancelled token otherwise) and the configured timeout.
        let ct = self.current_cancel_token();
        // Race the queue wait against cancellation: a task cancelled WHILE QUEUED
        // must free its slot at once (dropping the acquire future runs the gate's
        // queue-counter guard and releases it) instead of holding it until its
        // turn finally comes up. A fresh sync-call token never fires this branch.
        let _permit = tokio::select! {
            biased;
            _ = ct.cancelled() => return Err(CoreError::Cancelled),
            p = self.gate.acquire() => p?,
        };
        let out = self
            .toolchain
            .compile(&req, ct, Duration::from_secs(self.compile_timeout_secs))
            .await?;

        Ok((out, target_dir))
    }
}
