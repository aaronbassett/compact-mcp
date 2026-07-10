use rmcp::{ErrorData as McpError, model::CallToolResult, tool, tool_router};
use serde_json::json;

use crate::server::CompactMcp;

/// `compact list` prints lines like:
///   `→ 0.31.1 - x86_macos, aarch64_macos, ...`
///   `  0.31.0 - x86_macos, ...`
/// The meaning of `→` is not documented upstream, so we surface it as `marked`
/// and always return the raw text alongside.
fn parse_list(stdout: &str) -> Vec<serde_json::Value> {
    stdout
        .lines()
        .filter_map(|line| {
            let marked = line.trim_start().starts_with('\u{2192}');
            let body = line.trim_start().trim_start_matches('\u{2192}').trim();
            let (version, platforms) = body.split_once(" - ")?;
            if !version.chars().next()?.is_ascii_digit() {
                return None;
            }
            Some(json!({
                "version": version.trim(),
                "platforms": platforms.split(',').map(|s| s.trim()).collect::<Vec<_>>(),
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
        let v = self
            .toolchain
            .versions()
            .await
            .map_err(|e| McpError::internal_error(e.to_string(), None))?;
        Ok(Self::json_result(serde_json::to_value(v).unwrap(), false))
    }

    #[tool(description = "List Compact compiler versions available to the toolchain.")]
    async fn toolchain_list(&self) -> Result<CallToolResult, McpError> {
        let out = self
            .toolchain
            .list()
            .await
            .map_err(|e| McpError::internal_error(e.to_string(), None))?;
        Ok(Self::json_result(
            json!({ "versions": parse_list(&out), "raw": out }),
            false,
        ))
    }

    #[tool(
        description = "Check whether a newer Compact compiler is available. Performs network I/O."
    )]
    async fn toolchain_check(&self) -> Result<CallToolResult, McpError> {
        let out = self
            .toolchain
            .check()
            .await
            .map_err(|e| McpError::internal_error(e.to_string(), None))?;
        Ok(Self::json_result(
            json!({ "up_to_date": out.contains("Up to date"), "raw": out.trim() }),
            false,
        ))
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
}
