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

/// Maximum structural bracket nesting we will hand to the parser. The parser guards its own
/// recursion at 256 but returns a tree nested to the FULL input depth, and that tree's
/// recursive Drop overflows the stack past ~8000 levels (2 MiB stack). 512 sits well above any
/// real contract (and above the parser's own 256 limit) yet far below the crash zone.
pub const MAX_NESTING_DEPTH: usize = 512;

/// Cheap, iterative scan of maximum `([{ … }])` nesting. Approximate (counts brackets inside
/// strings/comments too) — fine for a DoS guard; real code never nests hundreds deep. Used to
/// refuse over-deep input BEFORE `parse` builds a tree whose recursive Drop would abort.
pub fn nesting_depth(source: &str) -> usize {
    let (mut depth, mut max) = (0usize, 0usize);
    for b in source.bytes() {
        match b {
            b'(' | b'[' | b'{' => {
                depth += 1;
                if depth > max {
                    max = depth;
                }
            }
            b')' | b']' | b'}' => depth = depth.saturating_sub(1),
            _ => {}
        }
    }
    max
}

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
    if nesting_depth(source) > MAX_NESTING_DEPTH {
        // Parse the empty string: a valid, shallow SOURCE_FILE root with no items.
        let empty = compactp_parser::parse_with("", parse_opts());
        return (SyntaxNode::new_root(empty.green), Vec::new());
    }
    let result = compactp_parser::parse_with(source, parse_opts());
    (SyntaxNode::new_root(result.green), result.errors)
}

pub fn diagnostics(source: &str, file: &str, max: Option<usize>) -> ParseOutcome {
    if nesting_depth(source) > MAX_NESTING_DEPTH {
        let d = Diagnostic {
            severity: crate::Severity::Error,
            source: crate::Source::Compactp,
            message: format!(
                "input nesting depth exceeds maximum ({MAX_NESTING_DEPTH}); refusing to parse"
            ),
            file: Some(file.to_string()),
            span: None,
            code: None,
            raw: true,
        };
        return ParseOutcome {
            success: false,
            error_count: 1,
            truncated: false,
            diagnostics: vec![d],
        };
    }
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

    if nesting_depth(source) > MAX_NESTING_DEPTH {
        return Stats {
            file_size_bytes: source.len(),
            token_count,
            node_count: 0,
            error_count: 1,
            recovery_count: 0,
            parse_time_ms: 0.0,
        };
    }

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

    fn deeply_nested(depth: usize) -> String {
        format!(
            "pragma language_version >= 0.23;\nexport pure circuit f(x: Field): Field {{ return {}x{}; }}\n",
            "(".repeat(depth),
            ")".repeat(depth)
        )
    }

    #[test]
    fn nesting_depth_counts_max_bracket_nesting() {
        assert_eq!(nesting_depth("a"), 0);
        assert_eq!(nesting_depth("([{}])"), 3);
        assert_eq!(nesting_depth("()()()"), 1);
        assert_eq!(nesting_depth(&"(".repeat(1000)), 1000);
    }

    #[test]
    fn diagnostics_refuses_over_deep_input_instead_of_crashing() {
        // depth 20000 would abort the process via the tree's recursive Drop if we parsed it.
        let out = diagnostics(&deeply_nested(20_000), "deep.compact", None);
        assert!(!out.success);
        assert_eq!(out.error_count, 1);
        assert!(out.diagnostics[0].message.contains("nesting depth"));
    }

    #[test]
    fn stats_and_symbols_survive_over_deep_input() {
        // Neither may build/drop the deep tree. stats still lexes; symbols is empty.
        let src = deeply_nested(20_000);
        let s = stats(&src);
        assert!(s.token_count > 0 && s.node_count == 0);
        assert!(symbols(&src).is_empty());
    }

    #[test]
    fn moderately_nested_input_still_parses_normally() {
        // Well under the cap: real behaviour is unchanged (this parses, few/no errors).
        let out = diagnostics(&deeply_nested(10), "ok.compact", None);
        assert!(
            out.diagnostics
                .iter()
                .all(|d| !d.message.contains("nesting depth"))
        );
    }

    #[test]
    fn count_nodes_walks_a_tree_deeper_than_the_parsers_own_limit() {
        // Depth 400 sits above the parser's own 256-deep recursion guard but under our
        // MAX_NESTING_DEPTH of 512, so this still parses for real (unlike the tests above)
        // and exercises count_nodes walking a tree deeper than the parser's own limit.
        let s = stats(&deeply_nested(400));
        assert!(s.node_count > 0);
    }
}
