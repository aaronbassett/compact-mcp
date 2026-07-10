pub mod analysis;

use rmcp::schemars;

/// Exactly one of `path` or `source` must be supplied.
#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
pub struct SourceInput {
    /// Path to a `.compact` file, relative to the workspace root.
    pub path: Option<String>,
    /// Inline Compact source. Use this to check an unsaved buffer.
    pub source: Option<String>,
}
