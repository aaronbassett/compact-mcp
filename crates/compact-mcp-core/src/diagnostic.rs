use serde::Serialize;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Severity {
    Error,
    Warning,
    Note,
}

/// Which tool produced this diagnostic. Never merge the two streams: they can
/// disagree, and the agent must be able to tell which one spoke.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Source {
    Compactp,
    Compactc,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Position {
    /// Byte offset. Present for `compactp`; absent for `compactc`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub offset: Option<u32>,
    /// 1-based.
    pub line: u32,
    /// 1-based.
    pub column: u32,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Span {
    pub start: Position,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub end: Option<Position>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Diagnostic {
    pub severity: Severity,
    pub source: Source,
    pub message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub file: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub span: Option<Span>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub code: Option<String>,
    /// True when we could not parse structure out of the tool's output and are
    /// passing it through verbatim. Never silently drop a diagnostic.
    pub raw: bool,
}

impl Diagnostic {
    /// Convert a `compactp` diagnostic. We render it to JSON via the upstream
    /// renderer (which owns span/line/column computation) and re-shape it.
    pub fn from_compactp(
        d: &compactp_diagnostics::Diagnostic,
        source_text: &str,
        file: &str,
    ) -> Self {
        let v = compactp_diagnostics::render_json(d, source_text);

        let severity = match v["severity"].as_str() {
            Some("warning") => Severity::Warning,
            Some("note") => Severity::Note,
            _ => Severity::Error,
        };

        let pos = |p: &serde_json::Value| -> Option<Position> {
            Some(Position {
                offset: p["offset"].as_u64().map(|n| n as u32),
                line: p["line"].as_u64()? as u32,
                column: p["column"].as_u64()? as u32,
            })
        };
        let span = v.get("primary_span").and_then(|s| {
            Some(Span {
                start: pos(&s["start"])?,
                end: pos(&s["end"]),
            })
        });

        let code = match (&v["code"]["prefix"], &v["code"]["number"]) {
            (serde_json::Value::String(p), serde_json::Value::Number(n)) => Some(format!("{p}{n}")),
            _ => None,
        };

        Self {
            severity,
            source: Source::Compactp,
            message: v["message"].as_str().unwrap_or_default().to_string(),
            file: Some(file.to_string()),
            span,
            code,
            raw: false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn compactp_diagnostic_is_tagged_with_its_source() {
        let src = "ledger count Field;"; // missing `:`
        let result = compactp_parser::parse(src);
        assert!(
            !result.errors.is_empty(),
            "fixture must produce a parse error"
        );

        let d = Diagnostic::from_compactp(&result.errors[0], src, "a.compact");
        assert_eq!(d.source, Source::Compactp);
        assert_eq!(d.severity, Severity::Error);
        assert_eq!(d.file.as_deref(), Some("a.compact"));
        assert!(!d.raw);
        assert!(d.span.is_some());
    }

    #[test]
    fn serializes_source_in_lowercase() {
        let d = Diagnostic {
            severity: Severity::Error,
            source: Source::Compactc,
            message: "boom".into(),
            file: None,
            span: None,
            code: None,
            raw: true,
        };
        let v = serde_json::to_value(&d).unwrap();
        assert_eq!(v["source"], "compactc");
        assert_eq!(v["severity"], "error");
        assert_eq!(v["raw"], true);
    }
}
