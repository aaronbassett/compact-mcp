use std::time::Duration;

use compact_mcp_core::CoreError;
use compact_mcp_core::toolchain::compile::{CompileOutcome, CompileRequest};
use rmcp::{
    ErrorData as McpError, RoleServer,
    handler::server::wrapper::Parameters,
    model::{CallToolResult, ProgressNotificationParam},
    schemars,
    service::RequestContext,
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
        ctx: RequestContext<RoleServer>,
        Parameters(args): Parameters<CompileArgs>,
    ) -> Result<CallToolResult, McpError> {
        let (text, _label) = self.read_input(&args.input)?;

        // Honest heartbeat: compactc reports no stages, so send elapsed seconds as
        // `progress` with NO `total` — a fabricated percentage would be a lie. Only
        // when the caller attached a progressToken. `interval`'s FIRST tick fires
        // immediately, so even a sub-second build emits at least one notification.
        let heartbeat = ctx.meta.get_progress_token().map(|token| {
            let peer = ctx.peer.clone();
            let stage = if args.skip_zk {
                "compiling"
            } else {
                "compiling and generating proving keys"
            };
            tokio::spawn(async move {
                let start = std::time::Instant::now();
                let mut tick = tokio::time::interval(Duration::from_secs(2));
                loop {
                    tick.tick().await;
                    let param = ProgressNotificationParam::new(
                        token.clone(),
                        start.elapsed().as_secs_f64(),
                    )
                    .with_message(stage);
                    // Peer gone / transport closed -> stop pinging.
                    if peer.notify_progress(param).await.is_err() {
                        break;
                    }
                }
            })
        });

        // A `CoreError` from resolving the workspace, scratch files, or the
        // compiler subprocess itself is a runtime/domain failure, not a
        // request-shape problem. rmcp renders `Err(McpError)` opaquely — the
        // caller would see "internal error", never our message — so every
        // `CoreError` here surfaces as a successful call with `isError: true`,
        // matching the other toolchain tools. `McpError` stays reserved for the
        // `path`/`source` XOR check above.
        let outcome = match self.compile_impl(&args, text).await {
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
        };

        if let Some(h) = heartbeat {
            h.abort();
        }
        outcome
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

        // The workspace gate above validates only the ENTRY path. The compiler
        // then dereferences `import`/`include` targets inside the source itself,
        // following `../` and absolute paths out of the root (a content leak of
        // out-of-root `.compact` files via diagnostics). Close that in-process
        // BEFORE spawning the compiler: reject any directive target — in this
        // source or any in-root file it transitively includes — that escapes the
        // root. Runs before the build gate, so it needs no toolchain.
        compact_mcp_core::assert_imports_contained(&self.workspace, &source, &text)?;

        let target_dir = self
            .workspace
            .resolve(args.target_dir.as_deref().unwrap_or("build"))?;

        // `--sourceRoot` is a cosmetic source-map field, not an import search
        // root (verified against the compiler), so it is not itself a traversal
        // vector — but route it through the workspace like `target_dir` anyway, so
        // no client-supplied path reaches the compiler un-contained (#7).
        let source_root = args
            .source_root
            .as_deref()
            .map(|r| self.workspace.resolve(r))
            .transpose()?
            .map(|p| p.to_string_lossy().into_owned());

        let req = CompileRequest {
            source,
            target_dir: target_dir.clone(),
            skip_zk: args.skip_zk,
            no_communications_commitment: args.no_communications_commitment,
            source_root,
        };

        // Serialize builds through the gate; a full queue is a CoreError the
        // handler surfaces as isError (never an opaque McpError). Hold the permit
        // for the whole compile. Use the task cancel token when running as a task
        // (a fresh, never-cancelled token otherwise) and the configured timeout.
        // `acquire_gate` races the queue wait against cancellation so a task
        // cancelled WHILE QUEUED frees its slot at once — see its doc comment.
        let ct = self.current_cancel_token();
        let _permit = self.acquire_gate(&ct).await?;
        let out = self
            .toolchain
            .compile(&req, ct, Duration::from_secs(self.compile_timeout_secs))
            .await?;

        Ok((out, target_dir))
    }
}
