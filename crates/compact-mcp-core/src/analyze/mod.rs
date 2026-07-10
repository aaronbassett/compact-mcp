use std::time::Instant;

use compactp_parser::ParseOptions;
use compactp_syntax::{SyntaxKind, SyntaxNode};
use serde::Serialize;

use crate::Diagnostic;

pub mod symbols;
pub use symbols::{Symbol, SymbolKind, ast_json, symbols};

/// Largest source we will parse or read, in bytes (2 MiB). Real Compact contracts are far
/// smaller; this bounds absolute work and memory for a single tool call.
pub const MAX_SOURCE_BYTES: usize = 2 * 1024 * 1024;

/// Parser options we use everywhere. `max_depth` is pinned explicitly (not left to the
/// upstream default) so a future `compactp` change cannot silently remove our recursion bound.
fn parse_opts() -> ParseOptions {
    ParseOptions {
        max_depth: 256,
        ..ParseOptions::default()
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct ParseOutcome {
    pub success: bool,
    /// The count BEFORE `max` is applied. Never erase the signal.
    pub error_count: usize,
    pub truncated: bool,
    pub diagnostics: Vec<Diagnostic>,
}

#[derive(Debug, Clone, Serialize)]
pub struct Stats {
    pub file_size_bytes: usize,
    pub token_count: usize,
    pub node_count: usize,
    pub error_count: usize,
    pub recovery_count: usize,
    pub parse_time_ms: f64,
}

pub(crate) fn parse_root(source: &str) -> (SyntaxNode, Vec<compactp_diagnostics::Diagnostic>) {
    let result = compactp_parser::parse_with(source, parse_opts());
    (SyntaxNode::new_root(result.green), result.errors)
}

pub fn diagnostics(source: &str, file: &str, max: Option<usize>) -> ParseOutcome {
    let result = compactp_parser::parse_with(source, parse_opts());
    let error_count = result.errors.len();

    let take = max.unwrap_or(usize::MAX);
    let truncated = error_count > take;

    let diagnostics = result
        .errors
        .iter()
        .take(take)
        .map(|d| Diagnostic::from_compactp(d, source, file))
        .collect();

    ParseOutcome {
        success: error_count == 0,
        error_count,
        truncated,
        diagnostics,
    }
}

pub fn stats(source: &str) -> Stats {
    let token_count = compactp_lexer::lex(source).len();

    let start = Instant::now();
    let result = compactp_parser::parse_with(source, parse_opts());
    let parse_time = start.elapsed();

    let root = SyntaxNode::new_root(result.green);

    Stats {
        file_size_bytes: source.len(),
        token_count,
        node_count: count_nodes(&root),
        error_count: result.errors.len(),
        recovery_count: root
            .descendants()
            .filter(|n| n.kind() == SyntaxKind::ERROR)
            .count(),
        parse_time_ms: parse_time.as_secs_f64() * 1000.0,
    }
}

/// Total node count. Iterative (explicit stack) so a pathologically deep tree cannot
/// overflow the call stack, independent of the parser's own depth guard.
fn count_nodes(root: &SyntaxNode) -> usize {
    let mut count = 0usize;
    let mut stack = vec![root.clone()];
    while let Some(node) = stack.pop() {
        count += 1;
        stack.extend(node.children());
    }
    count
}

#[cfg(test)]
mod tests {
    use super::*;

    const GOOD: &str = "pragma language_version >= 0.23;\nledger count: Counter;\n";
    const BAD: &str = "ledger count Field;";

    #[test]
    fn clean_source_succeeds_with_no_diagnostics() {
        let out = diagnostics(GOOD, "a.compact", None);
        assert!(out.success);
        assert_eq!(out.error_count, 0);
        assert!(out.diagnostics.is_empty());
        assert!(!out.truncated);
    }

    #[test]
    fn broken_source_reports_errors_tagged_compactp() {
        let out = diagnostics(BAD, "a.compact", None);
        assert!(!out.success);
        assert!(out.error_count >= 1);
        assert!(
            out.diagnostics
                .iter()
                .all(|d| d.source == crate::Source::Compactp)
        );
    }

    #[test]
    fn max_truncates_but_error_count_survives() {
        let out = diagnostics(BAD, "a.compact", Some(0));
        assert_eq!(out.diagnostics.len(), 0);
        assert!(
            out.error_count >= 1,
            "error_count is the count BEFORE truncation"
        );
        assert!(out.truncated);
    }

    #[test]
    fn stats_counts_tokens_and_nodes() {
        let s = stats(GOOD);
        assert_eq!(s.file_size_bytes, GOOD.len());
        assert!(s.token_count > 0);
        assert!(s.node_count > 0);
        assert_eq!(s.error_count, 0);
        assert_eq!(s.recovery_count, 0);
    }

    #[test]
    fn count_nodes_is_iterative_and_bounded_on_deep_input() {
        // Deeply-nested input: the parser's max_depth guard keeps the tree shallow, and
        // count_nodes is iterative regardless — this must return, not overflow.
        let deep = format!(
            "circuit c(): [] {{ x := {}1{}; }}",
            "(".repeat(40_000),
            ")".repeat(40_000)
        );
        let s = stats(&deep);
        assert!(s.node_count > 0);
        assert!(
            s.error_count > 0,
            "over-deep input should report recovery errors"
        );
    }
}
