use std::sync::LazyLock;

use regex::Regex;

use crate::{Diagnostic, Position, Severity, Source, Span};

/// `Exception: <file> line <L> char <C>: <message>`
/// Non-greedy on the filename so the FIRST `line/char` marker wins; a real
/// compactc path never contains that shape, but an unrelated earlier
/// `line N char M:` could still mis-split (not expected from real output).
/// Greedy on the tail so a message may contain colons.
static VSCODE_LINE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"^Exception: (.+?) line (\d+) char (\d+): (.*)$").expect("static regex is valid")
});

/// `compactc` emits at most ONE structured error and stops, so `out` holds 0 or 1
/// structured diagnostics. Any non-blank line we cannot parse is collected and
/// appended as a single trailing `raw: true` diagnostic — even when a structured
/// line was also matched — so we never silently drop compiler output (a banner or
/// a "Compilation failed" summary alongside the `Exception:` line survives).
pub fn parse_compactc_output(text: &str) -> Vec<Diagnostic> {
    let mut out = Vec::new();
    let mut unmatched = Vec::new();

    for line in text.lines() {
        let trimmed = line.trim_end();
        match VSCODE_LINE.captures(trimmed) {
            Some(c) => out.push(Diagnostic {
                // compactc `--vscode` only emits this shape on the hard-stop
                // exception path (exit 255), so Error is correct today; a future
                // non-fatal diagnostic in this same shape would be mislabeled.
                severity: Severity::Error,
                source: Source::Compactc,
                message: c[4].to_string(),
                file: Some(c[1].to_string()),
                span: Some(Span {
                    start: Position {
                        offset: None,
                        // `\d+` is unbounded, so a pathologically long number
                        // overflows u32; u32::MAX reads unambiguously as "bogus"
                        // (a real "line 1" is not mistaken for an overflow).
                        line: c[2].parse().unwrap_or(u32::MAX),
                        column: c[3].parse().unwrap_or(u32::MAX),
                    },
                    end: None,
                }),
                code: None,
                raw: false,
            }),
            None if !trimmed.trim().is_empty() => unmatched.push(trimmed.to_string()),
            None => {}
        }
    }

    if !unmatched.is_empty() {
        out.push(Diagnostic {
            severity: Severity::Error,
            source: Source::Compactc,
            message: unmatched.join("\n"),
            file: None,
            span: None,
            code: None,
            raw: true,
        });
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Severity, Source};

    #[test]
    fn parses_an_unbound_identifier_error() {
        let s = "Exception: broken.compact line 7 char 10: unbound identifier undefined_thing\n";
        let d = parse_compactc_output(s);
        assert_eq!(d.len(), 1);
        assert_eq!(d[0].source, Source::Compactc);
        assert_eq!(d[0].severity, Severity::Error);
        assert_eq!(d[0].file.as_deref(), Some("broken.compact"));
        assert_eq!(d[0].message, "unbound identifier undefined_thing");
        let span = d[0].span.as_ref().unwrap();
        assert_eq!(span.start.line, 7);
        assert_eq!(span.start.column, 10);
        assert_eq!(span.start.offset, None, "compactc gives no byte offsets");
        assert!(!d[0].raw);
    }

    #[test]
    fn message_may_contain_colons_and_quotes() {
        let s = r#"Exception: syntax_err.compact line 3 char 3: parse error: found keyword "let" (which is reserved for future use) looking for a statement or "}""#;
        let d = parse_compactc_output(s);
        assert_eq!(d.len(), 1);
        assert_eq!(
            d[0].message,
            r#"parse error: found keyword "let" (which is reserved for future use) looking for a statement or "}""#
        );
    }

    #[test]
    fn unrecognised_output_is_returned_raw_never_dropped() {
        let s = "zkir: something went sideways\nand another line";
        let d = parse_compactc_output(s);
        assert_eq!(d.len(), 1);
        assert!(d[0].raw);
        assert_eq!(d[0].source, Source::Compactc);
        assert!(d[0].message.contains("something went sideways"));
        assert!(d[0].span.is_none());
    }

    #[test]
    fn empty_output_yields_no_diagnostics() {
        assert!(parse_compactc_output("   \n ").is_empty());
    }

    #[test]
    fn matched_line_plus_trailing_noise_keeps_the_noise_raw() {
        let s = "Exception: broken.compact line 4 char 10: unbound identifier x\nCompilation failed with 1 error\n";
        let d = parse_compactc_output(s);
        assert_eq!(d.len(), 2);

        // d[0]: the structured diagnostic.
        assert!(!d[0].raw);
        assert_eq!(d[0].source, Source::Compactc);
        assert_eq!(d[0].file.as_deref(), Some("broken.compact"));
        assert_eq!(d[0].message, "unbound identifier x");
        let span = d[0].span.as_ref().unwrap();
        assert_eq!(span.start.line, 4);
        assert_eq!(span.start.column, 10);

        // d[1]: the trailing noise, preserved raw rather than dropped.
        assert!(d[1].raw);
        assert_eq!(d[1].source, Source::Compactc);
        assert!(d[1].message.contains("Compilation failed"));
        assert!(d[1].span.is_none());
        assert!(d[1].file.is_none());
    }

    #[test]
    fn crlf_line_endings_are_handled() {
        let s = "Exception: a.compact line 1 char 1: oops\r\n";
        let d = parse_compactc_output(s);
        assert_eq!(d.len(), 1);
        assert!(!d[0].raw);
        assert_eq!(d[0].file.as_deref(), Some("a.compact"));
        // `trim_end()` strips the stray `\r` before the regex sees the tail.
        assert_eq!(d[0].message, "oops");
    }
}
