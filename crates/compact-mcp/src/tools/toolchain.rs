use rmcp::{
    ErrorData as McpError, handler::server::wrapper::Parameters, model::CallToolResult, schemars,
    tool, tool_router,
};
use serde_json::json;

use crate::server::CompactMcp;

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
pub struct UpdateArgs {
    /// A specific compiler version, e.g. "0.31.1". Omit for the latest.
    pub version: Option<String>,
}

/// `compact list` prints lines like:
///   `→ 0.31.1 - x86_macos, aarch64_macos, ...`
///   `  0.31.0 - x86_macos, ...`
/// The meaning of `→` is not documented upstream, so we surface it as `marked`
/// and always return the raw text alongside.
///
/// We validate the version token with `semver` rather than a bare charset check:
/// that both KEEPS a real version line that has no ` - <platforms>` suffix
/// (e.g. `→ 0.31.1`) and REJECTS a non-version line whose leading token is all
/// digits (e.g. a `2024 - Copyright ...` banner). The `raw` field always carries
/// every line verbatim, so nothing this parser drops is truly lost.
fn parse_list(stdout: &str) -> Vec<serde_json::Value> {
    stdout
        .lines()
        .filter_map(|line| {
            let marked = line.trim_start().starts_with('\u{2192}');
            let body = line.trim_start().trim_start_matches('\u{2192}').trim();
            let (version, platforms) = match body.split_once(" - ") {
                Some((v, p)) => (v.trim(), p.split(',').map(|s| s.trim()).collect::<Vec<_>>()),
                None => (body, Vec::new()),
            };
            if semver::Version::parse(version).is_err() {
                return None; // not a real version line (header, copyright, drift) — raw still carries it
            }
            Some(json!({
                "version": version,
                "platforms": platforms,
                "marked": marked,
            }))
        })
        .collect()
}

#[tool_router(router = toolchain_router, vis = "pub(crate)")]
impl CompactMcp {
    #[tool(
        description = "Report compactc, language, ledger, runtime, compact-CLI and compactp \
                          versions, plus a skew verdict on whether the linked parser matches \
                          the installed compiler."
    )]
    async fn versions(&self) -> Result<CallToolResult, McpError> {
        // A `CoreError` here (e.g. `compact` not on PATH) is a runtime/domain
        // failure, not a request-shape problem. rmcp renders `Err(McpError)`
        // opaquely — the caller would see "internal error", never our message —
        // so we surface it as a successful call with `isError: true`, matching
        // the analysis tools. `McpError` stays reserved for bad request shapes.
        match self.toolchain.versions().await {
            Ok(v) => Ok(Self::json_result(serde_json::to_value(v).unwrap(), false)),
            Err(e) => Ok(Self::json_result(json!({ "error": e.to_string() }), true)),
        }
    }

    #[tool(description = "List Compact compiler versions available to the toolchain.")]
    async fn toolchain_list(&self) -> Result<CallToolResult, McpError> {
        match self.toolchain.list().await {
            Ok(out) => Ok(Self::json_result(
                json!({ "versions": parse_list(&out), "raw": out }),
                false,
            )),
            Err(e) => Ok(Self::json_result(json!({ "error": e.to_string() }), true)),
        }
    }

    #[tool(
        description = "Check whether a newer Compact compiler is available. Performs network I/O."
    )]
    async fn toolchain_check(&self) -> Result<CallToolResult, McpError> {
        match self.toolchain.check().await {
            Ok(out) => Ok(Self::json_result(
                json!({ "up_to_date": out.contains("Up to date"), "raw": out }),
                false,
            )),
            Err(e) => Ok(Self::json_result(json!({ "error": e.to_string() }), true)),
        }
    }
}

#[tool_router(router = mutation_router, vis = "pub(crate)")]
impl CompactMcp {
    #[tool(
        description = "Install or update the Compact compiler. Downloads a binary and \
                          writes to the toolchain directory.",
        annotations(destructive_hint = true, idempotent_hint = false)
    )]
    async fn toolchain_update(
        &self,
        Parameters(args): Parameters<UpdateArgs>,
    ) -> Result<CallToolResult, McpError> {
        match self.toolchain.update(args.version.as_deref()).await {
            Ok(raw) => Ok(Self::json_result(json!({ "raw": raw.trim() }), false)),
            Err(e) => Ok(Self::json_result(json!({ "error": e.to_string() }), true)),
        }
    }

    #[tool(
        description = "Remove ALL installed Compact compiler versions. Destructive and \
                          irreversible; a subsequent compile must re-download a compiler.",
        annotations(destructive_hint = true, idempotent_hint = true)
    )]
    async fn toolchain_clean(&self) -> Result<CallToolResult, McpError> {
        match self.toolchain.clean().await {
            Ok(raw) => Ok(Self::json_result(json!({ "raw": raw.trim() }), false)),
            Err(e) => Ok(Self::json_result(json!({ "error": e.to_string() }), true)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_the_marked_current_version() {
        let s = "compact: available versions\n\n\u{2192} 0.31.1 - x86_macos, aarch64_macos\n  0.31.0 - x86_linux\n";
        let v = parse_list(s);
        assert_eq!(v.len(), 2);
        assert_eq!(v[0]["version"], "0.31.1");
        assert_eq!(v[0]["marked"], true);
        assert_eq!(v[0]["platforms"][1], "aarch64_macos");
        assert_eq!(v[1]["marked"], false);
    }

    #[test]
    fn a_digit_leading_non_version_line_is_not_fabricated() {
        // `2024` has an all-digit leading token, so a bare charset check would
        // wrongly emit it. `semver` rejects it — no fake version appears.
        let v = parse_list("2024 - Copyright Notice\n");
        assert!(v.is_empty(), "fabricated a version from a banner: {v:?}");
    }

    #[test]
    fn a_real_version_with_no_platform_suffix_is_kept() {
        // A `→ 0.31.1` line with no ` - <platforms>` suffix must NOT vanish:
        // it is emitted with empty platforms and the `marked` flag preserved.
        let v = parse_list("\u{2192} 0.31.1\n");
        assert_eq!(v.len(), 1);
        assert_eq!(v[0]["version"], "0.31.1");
        assert_eq!(v[0]["marked"], true);
        assert_eq!(v[0]["platforms"], json!([]));
    }
}
