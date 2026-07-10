use serde::Serialize;

use super::Toolchain;
use crate::{CoreError, Diagnostic, Workspace, analyze};

#[derive(Debug, Clone)]
pub enum FmtInput {
    Path(String),
    Source(String),
}

#[derive(Debug, Clone, Serialize)]
pub struct FormatOutcome {
    pub ok: bool,
    pub changed: bool,
    /// Present only when `write == false`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub formatted: Option<String>,
    pub diagnostics: Vec<Diagnostic>,
}

impl FormatOutcome {
    fn parse_failure(diagnostics: Vec<Diagnostic>) -> Self {
        Self {
            ok: false,
            changed: false,
            formatted: None,
            diagnostics,
        }
    }
}

impl Toolchain {
    pub async fn format(
        &self,
        ws: &Workspace,
        input: FmtInput,
        write: bool,
    ) -> Result<FormatOutcome, CoreError> {
        self.rewrite(ws, input, write, "format").await
    }

    pub async fn fixup(
        &self,
        ws: &Workspace,
        path: &str,
        write: bool,
    ) -> Result<FormatOutcome, CoreError> {
        self.rewrite(ws, FmtInput::Path(path.to_string()), write, "fixup")
            .await
    }

    /// `compact format` and `compact fixup` share a shape: both rewrite files in
    /// place and both exit 1 on either "would change" or "cannot parse".
    async fn rewrite(
        &self,
        ws: &Workspace,
        input: FmtInput,
        write: bool,
        subcommand: &str,
    ) -> Result<FormatOutcome, CoreError> {
        let (before, path_on_disk) = match &input {
            FmtInput::Path(p) => {
                let resolved = ws.resolve(p)?;
                (std::fs::read_to_string(&resolved)?, Some(resolved))
            }
            FmtInput::Source(s) => (s.clone(), None),
        };

        // Gate: never let the formatter answer a parse question.
        let parsed = analyze::diagnostics(&before, "<input>", None);
        if !parsed.success {
            return Ok(FormatOutcome::parse_failure(parsed.diagnostics));
        }

        if write {
            let Some(target) = path_on_disk else {
                return Err(CoreError::InvalidArgs(
                    "`write: true` requires `path`, not `source`".into(),
                ));
            };
            let out = self.run(&[subcommand, &target.to_string_lossy()]).await?;
            if out.status != 0 {
                return Err(CoreError::ToolchainFailed {
                    cmd: format!("compact {subcommand}"),
                    code: out.status,
                    stderr: format!("{}{}", out.stdout, out.stderr),
                });
            }
            let after = std::fs::read_to_string(&target)?;
            return Ok(FormatOutcome {
                ok: true,
                changed: after != before,
                formatted: None,
                diagnostics: Vec::new(),
            });
        }

        // Non-destructive: format a copy inside the workspace.
        let scope = ws.temp_scope(subcommand)?;
        let copy = scope.write_file("input.compact", &before)?;
        let out = self.run(&[subcommand, &copy.to_string_lossy()]).await?;
        if out.status != 0 {
            return Err(CoreError::ToolchainFailed {
                cmd: format!("compact {subcommand}"),
                code: out.status,
                stderr: format!("{}{}", out.stdout, out.stderr),
            });
        }
        let after = std::fs::read_to_string(&copy)?;

        Ok(FormatOutcome {
            ok: true,
            changed: after != before,
            formatted: Some(after),
            diagnostics: Vec::new(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Workspace;

    const MESSY: &str = "pragma language_version >= 0.23;\nimport CompactStandardLibrary;\nexport ledger    a:Counter;\n";
    const BROKEN: &str = "export circuit oops(): [] { let x = }";

    fn ws() -> (tempfile::TempDir, Workspace) {
        let d = tempfile::tempdir().unwrap();
        let w = Workspace::new(d.path()).unwrap();
        (d, w)
    }

    #[tokio::test]
    #[cfg_attr(not(feature = "toolchain-tests"), ignore)]
    async fn formats_inline_source_without_touching_disk() {
        let (_d, w) = ws();
        let tc = Toolchain::new("compact", None);
        let out = tc
            .format(&w, FmtInput::Source(MESSY.into()), false)
            .await
            .unwrap();
        assert!(out.ok);
        assert!(out.changed);
        let formatted = out.formatted.unwrap();
        assert!(
            formatted.contains("export ledger a: Counter;"),
            "got {formatted:?}"
        );
    }

    #[tokio::test]
    #[cfg_attr(not(feature = "toolchain-tests"), ignore)]
    async fn already_canonical_source_reports_unchanged() {
        let (_d, w) = ws();
        let tc = Toolchain::new("compact", None);
        let once = tc
            .format(&w, FmtInput::Source(MESSY.into()), false)
            .await
            .unwrap();
        let twice = tc
            .format(&w, FmtInput::Source(once.formatted.unwrap()), false)
            .await
            .unwrap();
        assert!(twice.ok);
        assert!(!twice.changed, "formatting must be idempotent");
    }

    #[tokio::test]
    #[cfg_attr(not(feature = "toolchain-tests"), ignore)]
    async fn syntax_errors_are_reported_as_parse_errors_not_formatting_failures() {
        // `compact format --check` exits 1 for BOTH cases; we must not conflate them.
        let (_d, w) = ws();
        let tc = Toolchain::new("compact", None);
        let out = tc
            .format(&w, FmtInput::Source(BROKEN.into()), false)
            .await
            .unwrap();
        assert!(!out.ok);
        assert!(!out.changed);
        assert!(out.formatted.is_none());
        assert!(!out.diagnostics.is_empty());
        assert!(
            out.diagnostics
                .iter()
                .all(|d| d.source == crate::Source::Compactp)
        );
    }

    #[tokio::test]
    #[cfg_attr(not(feature = "toolchain-tests"), ignore)]
    async fn write_mode_rewrites_the_file_in_place() {
        let (d, w) = ws();
        std::fs::write(d.path().join("m.compact"), MESSY).unwrap();
        let tc = Toolchain::new("compact", None);
        let out = tc
            .format(&w, FmtInput::Path("m.compact".into()), true)
            .await
            .unwrap();
        assert!(out.ok && out.changed);
        assert!(out.formatted.is_none(), "write mode returns no text");
        let on_disk = std::fs::read_to_string(d.path().join("m.compact")).unwrap();
        assert!(on_disk.contains("export ledger a: Counter;"));
    }

    /// The brief's tests cover `format` but not `fixup` (which shares the `rewrite`
    /// path). This proves the `fixup` path runs end-to-end without over-asserting:
    /// we don't know whether `compact fixup` wants to change an already-canonical
    /// file, so we only assert `ok`.
    #[tokio::test]
    #[cfg_attr(not(feature = "toolchain-tests"), ignore)]
    async fn fixup_runs_end_to_end_on_a_clean_file() {
        let (d, w) = ws();
        let clean = "pragma language_version >= 0.23;\n\nimport CompactStandardLibrary;\n\nexport ledger a: Counter;\n";
        std::fs::write(d.path().join("clean.compact"), clean).unwrap();
        let tc = Toolchain::new("compact", None);
        let out = tc.fixup(&w, "clean.compact", false).await.unwrap();
        assert!(out.ok);
    }
}
