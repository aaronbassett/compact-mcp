use compact_mcp_core::CoreError;
use compact_mcp_core::artifacts::{self, Artifacts};
use rmcp::{
    ErrorData as McpError, handler::server::wrapper::Parameters, model::CallToolResult, schemars,
    tool, tool_router,
};
use serde_json::json;

use crate::server::CompactMcp;

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
pub struct TargetDirArgs {
    /// A compiler output directory, relative to the workspace root.
    pub target_dir: String,
}

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
pub struct ZkirStatsArgs {
    /// A compiler output directory, relative to the workspace root.
    pub target_dir: String,
    /// Omit to report every circuit; name one to report just that circuit.
    pub circuit: Option<String>,
}

#[tool_router(router = artifacts_router, vis = "pub(crate)")]
impl CompactMcp {
    #[tool(
        description = "Inspect a compiler output directory: the parsed contract-info.json \
                          (circuits, witnesses, ledger, versions), whether proving keys are \
                          present, and the discovered file tree. NOTE: contract-info.json lists \
                          only witnesses that a circuit actually calls — an unused witness is \
                          absent. Use `symbols` to see every declared witness."
    )]
    async fn artifacts(
        &self,
        Parameters(args): Parameters<TargetDirArgs>,
    ) -> Result<CallToolResult, McpError> {
        // resolve + scan are runtime/domain operations. rmcp renders
        // `Err(McpError)` opaquely, so every `CoreError` here surfaces as a
        // successful call with `isError: true` carrying the message — matching
        // the other tools. There is no path/source XOR to reserve `McpError` for.
        match self.artifacts_impl(&args.target_dir) {
            Ok(a) => Ok(Self::json_result(serde_json::to_value(a).unwrap(), false)),
            Err(e) => Ok(Self::json_result(json!({ "error": e.to_string() }), true)),
        }
    }

    #[tool(
        description = "Instruction count and opcode histogram for compiled circuits. \
                          Only circuits with proof:true emit a .zkir; a proof:false circuit is \
                          reported under `absent`. Omit `circuit` to report all of them."
    )]
    async fn zkir_stats(
        &self,
        Parameters(args): Parameters<ZkirStatsArgs>,
    ) -> Result<CallToolResult, McpError> {
        match self.zkir_stats_impl(&args) {
            Ok(v) => Ok(Self::json_result(v, false)),
            Err(e) => Ok(Self::json_result(json!({ "error": e.to_string() }), true)),
        }
    }
}

impl CompactMcp {
    fn artifacts_impl(&self, target_dir: &str) -> Result<Artifacts, CoreError> {
        let dir = self.workspace.resolve(target_dir)?;
        artifacts::scan(&dir)
    }

    fn zkir_stats_impl(&self, args: &ZkirStatsArgs) -> Result<serde_json::Value, CoreError> {
        let dir = self.workspace.resolve(&args.target_dir)?;
        let scanned = artifacts::scan(&dir)?;

        // A named-but-unknown circuit is a caller-visible lookup failure, not a
        // protocol error: return it as a domain error (mapped to isError by the
        // handler) so the agent SEES which circuit names actually exist. When no
        // `circuit` is given we report every circuit, so an empty set there is a
        // legitimately circuit-less contract — not an error.
        if let Some(name) = args.circuit.as_deref()
            && !scanned.circuits.iter().any(|c| c.name == name)
        {
            let available: Vec<&str> = scanned.circuits.iter().map(|c| c.name.as_str()).collect();
            return Err(CoreError::InvalidArgs(format!(
                "no circuit named {name:?} in {}; available: {available:?}",
                args.target_dir
            )));
        }

        let wanted = scanned
            .circuits
            .iter()
            .filter(|c| args.circuit.as_deref().is_none_or(|n| n == c.name));

        let mut stats = Vec::new();
        let mut absent = Vec::new();
        for c in wanted {
            match &c.zkir {
                Some(rel) => {
                    let s = artifacts::zkir::stats(&c.name, &dir.join(rel))?;
                    stats.push(serde_json::to_value(s).unwrap());
                }
                // No `.zkir` on disk. A proof:false circuit never emits one; a
                // proof:true circuit missing it means the build is incomplete —
                // report both honestly rather than blaming proof:false for both.
                None => absent.push(json!({
                    "circuit": c.name,
                    "proof": c.proof,
                    "reason": if c.proof {
                        "proof:true but no .zkir found — build may be incomplete"
                    } else {
                        "proof:false — no zkir emitted"
                    },
                })),
            }
        }
        Ok(json!({ "circuits": stats, "absent": absent }))
    }
}
