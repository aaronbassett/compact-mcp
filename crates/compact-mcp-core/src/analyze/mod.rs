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

/// Conservative proxy for the depth of the syntax tree `parse` would build, computed from the
/// token stream. The parser's own `max_depth` guard bounds only recursive descent; its Pratt
/// loop builds left-associative infix and postfix chains (`a+a+…`, `a.b.c…`, `f()()…`, `a[i][j]…`,
/// `a as T as T…`) iteratively in one stack frame, so those trees are nested to the full chain
/// length and their recursive Drop overflows the stack. This charges one unit of depth per such
/// chain operator plus one per open bracket, banking a postfix call/index onto its enclosing
/// expression when it closes (so a chain accumulates but a nest is not double-counted), and
/// resetting at statement/block boundaries. Refusing over-deep input keeps the tree from being
/// built and bounds the parser's O(n²) cost on long chains. Over-estimates slightly (e.g. a
/// prefix `-` counts like an infix one); real Compact never approaches the cap.
pub fn structural_depth(source: &str) -> usize {
    use compactp_syntax::SyntaxKind::*;
    let tokens = compactp_lexer::lex(source);
    // (chain operators banked at this bracket level, is this a postfix call/index bracket)
    let mut levels: Vec<(usize, bool)> = vec![(0, false)];
    let mut sum_ops: usize = 0;
    let mut peak: usize = 1;
    let mut prev_value_ender = false; // did the previous significant token end a value?
    for (kind, _text) in tokens {
        if kind.is_trivia() {
            continue;
        }
        match kind {
            L_PAREN | L_BRACKET => levels.push((0, prev_value_ender)), // postfix iff after a value
            L_BRACE => levels.push((0, false)),
            R_PAREN | R_BRACKET | R_BRACE => {
                if let Some((n, postfix)) = levels.pop() {
                    sum_ops = sum_ops.saturating_sub(n);
                    if postfix {
                        // the CALL/INDEX node banks one level onto the enclosing expression
                        if let Some(l) = levels.last_mut() {
                            l.0 += 1;
                        }
                        sum_ops += 1;
                    }
                }
                if levels.is_empty() {
                    levels.push((0, false));
                }
                if kind == R_BRACE {
                    // end of a block/item: clear the enclosing chain accumulator
                    if let Some(l) = levels.last_mut() {
                        sum_ops = sum_ops.saturating_sub(l.0);
                        l.0 = 0;
                    }
                }
            }
            SEMICOLON => {
                if let Some(l) = levels.last_mut() {
                    sum_ops = sum_ops.saturating_sub(l.0);
                    l.0 = 0;
                }
            }
            PLUS | MINUS | STAR | PIPE_PIPE | AMP_AMP | EQ_EQ | BANG_EQ | LT | LT_EQ | GT
            | GT_EQ | AS_KW | DOT | QUESTION => {
                if let Some(l) = levels.last_mut() {
                    l.0 += 1;
                }
                sum_ops += 1;
            }
            _ => {}
        }
        prev_value_ender = matches!(
            kind,
            IDENT
                | R_PAREN
                | R_BRACKET
                | R_BRACE
                | INT_LIT
                | HEX_LIT
                | OCT_LIT
                | BIN_LIT
                | STRING_LIT
                | TRUE_KW
                | FALSE_KW
        );
        let cur = levels.len() + sum_ops;
        if cur > peak {
            peak = cur;
        }
        if cur > MAX_STRUCTURAL_DEPTH {
            return cur; // early exit: also bounds the scan's own cost
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

    /// Wrap an expression `body` in a minimal valid circuit so the whole string is a realistic,
    /// parseable contract — the exact shape whose deep tree would SIGABRT on Drop if we parsed it.
    fn circuit(body: &str) -> String {
        format!(
            "pragma language_version >= 0.23;\nexport pure circuit f(x: Field): Field {{ return {body}; }}\n"
        )
    }

    #[test]
    fn structural_depth_charges_for_brackets_and_operator_chains() {
        assert_eq!(structural_depth("a"), 1);
        // One top level plus open bracket levels at the deepest point.
        assert!(structural_depth("([{}])") >= 4);
        // A bare operator chain the bracket count (which would say 1) cannot see.
        let long_chain = format!("{}x", "x+".repeat(1000));
        assert!(structural_depth(&long_chain) > MAX_STRUCTURAL_DEPTH);
        let short_chain = format!("{}x", "x+".repeat(10));
        assert!(structural_depth(&short_chain) <= MAX_STRUCTURAL_DEPTH);
    }

    /// The core regression guard: enumerate EVERY chain-forming construct the parser's Pratt loop
    /// builds in one stack frame (brackets, infix, member/method, call, index, cast, ternary).
    /// Each must be refused before its ~2000-deep tree is built; a future construct the guard
    /// cannot see fails this table loudly (the `structural_depth` assert panics) rather than
    /// silently SIGABRT'ing the server.
    #[test]
    fn every_chain_construct_over_the_cap_is_refused_without_crashing() {
        let n = 2000; // each shape builds a ~2000-deep tree that WOULD SIGABRT if parsed
        let shapes: Vec<(&str, String)> = vec![
            (
                "parens",
                circuit(&format!("{}x{}", "(".repeat(n), ")".repeat(n))),
            ),
            (
                "plus",
                circuit(&{
                    let mut s = String::from("x");
                    for _ in 0..n {
                        s.push_str("+x");
                    }
                    s
                }),
            ),
            (
                "member",
                circuit(&{
                    let mut s = String::from("x");
                    for _ in 0..n {
                        s.push_str(".a");
                    }
                    s
                }),
            ),
            ("call", circuit(&format!("x{}", "()".repeat(n)))),
            ("index", circuit(&format!("x{}", "[0]".repeat(n)))),
            ("cast", circuit(&format!("x{}", " as Field".repeat(n)))),
            (
                "ternary",
                circuit(&{
                    let mut s = String::from("x");
                    for _ in 0..n {
                        s.push_str("?x:x");
                    }
                    s
                }),
            ),
            (
                "method",
                circuit(&{
                    let mut s = String::from("x");
                    for _ in 0..n {
                        s.push_str(".a(b)");
                    }
                    s
                }),
            ),
        ];
        for (name, src) in shapes {
            assert!(
                structural_depth(&src) > MAX_STRUCTURAL_DEPTH,
                "{name}: not over cap"
            );
            let out = diagnostics(&src, "deep.compact", None); // must NOT SIGABRT
            assert!(!out.success && out.error_count == 1, "{name}: not refused");
            assert!(
                out.diagnostics[0].message.contains("structural depth"),
                "{name}"
            );
            let s = stats(&src);
            assert_eq!(s.node_count, 0, "{name}: stats must skip the parse");
            assert!(symbols(&src).is_empty(), "{name}: symbols must be empty");
        }
    }

    /// The other side of the guard: legitimate large/deep-but-bounded contracts must NOT be
    /// refused. This is why we gate on a tree-depth proxy rather than a token/byte cap.
    #[test]
    fn legitimate_shapes_are_not_refused() {
        let ok: Vec<(&str, String)> = vec![
            (
                "nested_calls_500",
                circuit(&{
                    let mut s = String::from("x");
                    for _ in 0..500 {
                        s = format!("f({s})");
                    }
                    s
                }),
            ),
            (
                "wide_args_2000",
                circuit(&{
                    let mut s = String::from("f(");
                    for i in 0..2000 {
                        if i > 0 {
                            s.push(',');
                        }
                        s.push('a');
                    }
                    s.push(')');
                    s
                }),
            ),
            (
                "mixed_chain_50",
                circuit(&{
                    let mut s = String::from("x");
                    for _ in 0..50 {
                        s.push_str(".a(b)[0]");
                    }
                    s
                }),
            ),
            ("many_circuits_400", {
                let mut s = String::from("pragma language_version >= 0.23;\nledger n: Counter;\n");
                for i in 0..400 {
                    s.push_str(&format!(
                        "export circuit c{i}(a: Field, b: Field): Field {{ return (a + b) * {i} - a; }}\n"
                    ));
                }
                s
            }),
            ("generics_200", {
                let mut s = String::from("pragma language_version >= 0.23;\n");
                for i in 0..200 {
                    s.push_str(&format!(
                        "export circuit g{i}(m: Map<Bytes<32>, Vector<10, Uint<64>>>): Field {{ return 0; }}\n"
                    ));
                }
                s
            }),
        ];
        for (name, src) in ok {
            assert!(
                structural_depth(&src) <= MAX_STRUCTURAL_DEPTH,
                "{name}: wrongly over cap ({})",
                structural_depth(&src)
            );
            let out = diagnostics(&src, "ok.compact", None);
            assert!(
                !out.diagnostics
                    .iter()
                    .any(|d| d.message.contains("structural depth")),
                "{name}: wrongly refused"
            );
        }
    }

    #[test]
    fn count_nodes_walks_a_tree_deeper_than_the_parsers_own_limit() {
        // Depth 400 sits above the parser's own 256-deep recursion guard but under our
        // MAX_STRUCTURAL_DEPTH of 512, so this still parses for real (unlike the tests above)
        // and exercises count_nodes walking a tree deeper than the parser's own limit.
        let s = stats(&circuit(&format!(
            "{}x{}",
            "(".repeat(400),
            ")".repeat(400)
        )));
        assert!(s.node_count > 0);
    }
}
