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

/// Maximum structural tree depth we will hand to the parser. The parser guards its own recursion
/// at 256 but returns a tree nested to the FULL input depth (bracket nesting AND left-nested
/// operator/access chains, which it builds iteratively without tripping its own guard). That
/// tree's recursive Drop overflows the stack past ~4500 levels (2 MiB stack). 512 sits well above
/// any real contract (and above the parser's own 256 limit) yet far below the crash zone, and it
/// also bounds the parser's O(n²) cost on long operator chains.
pub const MAX_STRUCTURAL_DEPTH: usize = 512;

/// Conservative proxy for the depth of the syntax tree `parse` would build, scanning bytes
/// only. Charges for bracket nesting AND for operator/field-access chains (`a+a+…`, `a.b.c…`),
/// which build left-nested trees the bracket count cannot see. A bracketed group's operators
/// are un-counted when it closes (the subtree becomes a single child); `;` ends a statement's
/// chain. Over-estimates (counts operator bytes inside strings/comments, `.` in `0.23`, and
/// each byte of multi-byte operators) — fine for a DoS guard; real code stays far under the cap.
/// Refusing over-deep input keeps the tree from ever being built (its recursive Drop aborts the
/// process) and bounds the parser's O(n²) cost on long operator chains.
pub fn structural_depth(source: &str) -> usize {
    let mut levels: Vec<usize> = vec![0]; // operator count per open bracket level
    let mut sum_ops: usize = 0; // total operators across all open levels
    let mut peak: usize = 1;
    for b in source.bytes() {
        match b {
            b'(' | b'[' | b'{' => levels.push(0),
            b')' | b']' | b'}' => {
                if let Some(n) = levels.pop() {
                    sum_ops = sum_ops.saturating_sub(n);
                }
                if levels.is_empty() {
                    levels.push(0);
                } // tolerate unbalanced input
            }
            b';' => {
                if let Some(last) = levels.last_mut() {
                    sum_ops = sum_ops.saturating_sub(*last);
                    *last = 0;
                }
            }
            b'+' | b'-' | b'*' | b'/' | b'%' | b'=' | b'<' | b'>' | b'!' | b'&' | b'|' | b'^'
            | b'~' | b'.' => {
                if let Some(last) = levels.last_mut() {
                    *last += 1;
                }
                sum_ops += 1;
            }
            _ => {}
        }
        let cur = levels.len() + sum_ops;
        if cur > peak {
            peak = cur;
        }
        if cur > MAX_STRUCTURAL_DEPTH {
            return cur; // early exit: bounds the scan itself
        }
    }
    peak
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
    if structural_depth(source) > MAX_STRUCTURAL_DEPTH {
        // Parse the empty string: a valid, shallow SOURCE_FILE root with no items.
        let empty = compactp_parser::parse_with("", parse_opts());
        return (SyntaxNode::new_root(empty.green), Vec::new());
    }
    let result = compactp_parser::parse_with(source, parse_opts());
    (SyntaxNode::new_root(result.green), result.errors)
}

pub fn diagnostics(source: &str, file: &str, max: Option<usize>) -> ParseOutcome {
    if structural_depth(source) > MAX_STRUCTURAL_DEPTH {
        let d = Diagnostic {
            severity: crate::Severity::Error,
            source: crate::Source::Compactp,
            message: format!(
                "input structural depth exceeds maximum ({MAX_STRUCTURAL_DEPTH}); refusing to parse"
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

    if structural_depth(source) > MAX_STRUCTURAL_DEPTH {
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

    /// A bracket-free left-nested `x+x+…+x` sum wrapped in a valid circuit. The parser builds
    /// this chain iteratively (its own 256 depth guard never trips) into a tree ~`terms` deep
    /// whose recursive Drop SIGABRTs the server. `structural_depth` charges for the operators,
    /// so the guard catches it even though the bracket count is 1.
    fn operator_chain(terms: usize) -> String {
        format!(
            "pragma language_version >= 0.23;\nexport pure circuit f(x: Field): Field {{ return {}x; }}\n",
            "x+".repeat(terms)
        )
    }

    /// A large but SHALLOW contract: many independent `;`-terminated statements. Its tree is
    /// wide, not deep, so it must be ALLOWED — this is the whole reason we gate on a depth proxy
    /// instead of a token/byte cap.
    fn shallow_statements(count: usize) -> String {
        let body = "x = x + 1; ".repeat(count);
        format!("pragma language_version >= 0.23;\nexport circuit f(): [] {{ {body} }}\n")
    }

    #[test]
    fn structural_depth_charges_for_brackets_and_operator_chains() {
        assert_eq!(structural_depth("a"), 1);
        // One top level plus three open bracket levels at the deepest point.
        assert_eq!(structural_depth("([{}])"), 4);
        // A bare operator chain the bracket count (which would say 1) cannot see.
        let long_chain = format!("{}x", "x+".repeat(1000));
        assert!(structural_depth(&long_chain) > MAX_STRUCTURAL_DEPTH);
        let short_chain = format!("{}x", "x+".repeat(10));
        assert!(structural_depth(&short_chain) <= MAX_STRUCTURAL_DEPTH);
    }

    #[test]
    fn diagnostics_refuses_over_deep_input_instead_of_crashing() {
        // depth 20000 would abort the process via the tree's recursive Drop if we parsed it.
        let out = diagnostics(&deeply_nested(20_000), "deep.compact", None);
        assert!(!out.success);
        assert_eq!(out.error_count, 1);
        assert!(out.diagnostics[0].message.contains("structural depth"));
    }

    #[test]
    fn diagnostics_refuses_bracket_free_operator_chain() {
        // Regression test for the operator-chain bypass: a valid 5000-term `x+x+…+x` sum has
        // bracket nesting 1, so the old scan let it through and it SIGABRT'd on Drop. It must
        // now be refused, and this test must NOT abort.
        let out = diagnostics(&operator_chain(5000), "chain.compact", None);
        assert!(!out.success);
        assert_eq!(out.error_count, 1);
        assert!(out.diagnostics[0].message.contains("depth"));
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
    fn stats_and_symbols_survive_operator_chain() {
        // Same bypass shape via stats/symbols: must return without building the deep tree.
        let src = operator_chain(5000);
        let s = stats(&src);
        assert!(s.token_count > 0 && s.node_count == 0);
        assert!(symbols(&src).is_empty());
    }

    #[test]
    fn moderately_nested_input_still_parses_normally() {
        // Well under the cap: real behaviour is unchanged (this parses, few/no errors).
        let out = diagnostics(&deeply_nested(10), "ok.compact", None);
        assert!(out.diagnostics.iter().all(|d| !d.message.contains("depth")));
    }

    #[test]
    fn large_shallow_contract_is_not_refused() {
        // 5000 short statements: big input, trivial depth. Must NOT trip the depth guard —
        // guards against over-refusal of legitimate large-but-shallow contracts.
        let out = diagnostics(&shallow_statements(5000), "shallow.compact", None);
        assert!(
            out.diagnostics.iter().all(|d| !d.message.contains("depth")),
            "a large shallow contract must not be refused for structural depth"
        );
    }

    #[test]
    fn count_nodes_walks_a_tree_deeper_than_the_parsers_own_limit() {
        // Depth 400 sits above the parser's own 256-deep recursion guard but under our
        // MAX_STRUCTURAL_DEPTH of 512, so this still parses for real (unlike the tests above)
        // and exercises count_nodes walking a tree deeper than the parser's own limit.
        let s = stats(&deeply_nested(400));
        assert!(s.node_count > 0);
    }
}
