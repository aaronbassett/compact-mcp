use std::sync::LazyLock;

use regex::Regex;

use crate::{Diagnostic, Position, Severity, Source, Span};

/// `Exception: <file> line <L> char <C>: <message>`
/// Non-greedy on the filename so a path containing " line " cannot swallow the
/// coordinates; greedy on the tail so a message may contain colons.
static VSCODE_LINE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"^Exception: (.+?) line (\d+) char (\d+): (.*)$").expect("static regex is valid")
});

/// `compactc` emits at most ONE error and stops, so this returns 0 or 1 structured
/// diagnostics. Anything it emits that we cannot parse is returned verbatim with
/// `raw: true` — we never silently drop compiler output.
pub fn parse_compactc_output(text: &str) -> Vec<Diagnostic> {
    let mut out = Vec::new();

    for line in text.lines() {
        if let Some(c) = VSCODE_LINE.captures(line.trim_end()) {
            out.push(Diagnostic {
                severity: Severity::Error,
                source: Source::Compactc,
                message: c[4].to_string(),
                file: Some(c[1].to_string()),
                span: Some(Span {
                    start: Position {
                        offset: None,
                        line: c[2].parse().unwrap_or(1),
                        column: c[3].parse().unwrap_or(1),
                    },
                    end: None,
                }),
                code: None,
                raw: false,
            });
        }
    }

    if out.is_empty() && !text.trim().is_empty() {
        out.push(Diagnostic {
            severity: Severity::Error,
            source: Source::Compactc,
            message: text.trim().to_string(),
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
        assert!(d[0].message.starts_with("parse error: found keyword"));
        assert!(d[0].message.ends_with(r#"or "}""#));
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
}
