use std::path::Path;

use serde::Serialize;

use super::{Output, Toolchain};
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
        // Precondition, enforced BEFORE the parse gate: `write: true` can only
        // rewrite a file on disk, never inline source. Hoisted so the same invalid
        // combination fails identically regardless of whether the content happens
        // to parse — otherwise `write + Source` would surface as `parse_failure`
        // for broken input and `InvalidArgs` for valid input.
        if write && matches!(input, FmtInput::Source(_)) {
            return Err(CoreError::InvalidArgs(
                "`write: true` requires `path`, not `source`".into(),
            ));
        }

        // Read the input and enforce the source-size cap. Core does its own fs
        // read here, so core must enforce `MAX_SOURCE_BYTES`: `analyze::diagnostics`
        // lexes the entire input before its depth guard runs. For a path, stat and
        // reject BEFORE reading so an oversized file is never slurped into memory.
        let (before, path_on_disk, label) = match &input {
            FmtInput::Path(p) => {
                let resolved = ws.resolve(p)?;
                if std::fs::metadata(&resolved)?.len() as usize > crate::MAX_SOURCE_BYTES {
                    return Err(CoreError::InvalidArgs(format!(
                        "input exceeds maximum size ({} bytes)",
                        crate::MAX_SOURCE_BYTES
                    )));
                }
                (
                    std::fs::read_to_string(&resolved)?,
                    Some(resolved),
                    p.clone(),
                )
            }
            FmtInput::Source(s) => {
                if s.len() > crate::MAX_SOURCE_BYTES {
                    return Err(CoreError::InvalidArgs(format!(
                        "input exceeds maximum size ({} bytes)",
                        crate::MAX_SOURCE_BYTES
                    )));
                }
                (s.clone(), None, "<source>".to_string())
            }
        };

        // Gate: never let the formatter answer a parse question. Thread the real
        // filename through for a path so diagnostics point at the user's file.
        let parsed = analyze::diagnostics(&before, &label, None);
        if !parsed.success {
            return Ok(FormatOutcome::parse_failure(parsed.diagnostics));
        }

        if write {
            // Unreachable given the precondition above, but kept as a
            // belt-and-suspenders that returns the same error rather than panics.
            let Some(target) = path_on_disk else {
                return Err(CoreError::InvalidArgs(
                    "`write: true` requires `path`, not `source`".into(),
                ));
            };
            let out = self.run(&[subcommand, &target.to_string_lossy()]).await?;
            if out.status != 0 {
                return Err(self.toolchain_failed(subcommand, &target, out));
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
            return Err(self.toolchain_failed(subcommand, &copy, out));
        }
        let after = std::fs::read_to_string(&copy)?;

        Ok(FormatOutcome {
            ok: true,
            changed: after != before,
            formatted: Some(after),
            diagnostics: Vec::new(),
        })
    }

    /// Build a `ToolchainFailed` from a non-zero `compact <subcommand> <path>`
    /// run. Reports the real `bin` and target path (not a hardcoded `"compact"`)
    /// and joins stdout+stderr with a newline only when both are non-empty, so the
    /// two streams never run together on one line — consistent with `check`.
    fn toolchain_failed(&self, subcommand: &str, target: &Path, out: Output) -> CoreError {
        let stderr = match (out.stdout.is_empty(), out.stderr.is_empty()) {
            (false, false) => format!("{}\n{}", out.stdout, out.stderr),
            _ => format!("{}{}", out.stdout, out.stderr),
        };
        CoreError::ToolchainFailed {
            cmd: format!("{} {subcommand} {}", self.bin, target.display()),
            code: out.status,
            stderr,
        }
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
    /// path). `compact fixup` is a deterministic no-op on an already-canonical file,
    /// so this asserts the full contract: `ok`, no reported change, and — since this
    /// is non-write mode — the original file left byte-identical on disk.
    #[tokio::test]
    #[cfg_attr(not(feature = "toolchain-tests"), ignore)]
    async fn fixup_is_a_noop_on_a_clean_file() {
        let (d, w) = ws();
        let clean = "pragma language_version >= 0.23;\n\nimport CompactStandardLibrary;\n\nexport ledger a: Counter;\n";
        std::fs::write(d.path().join("clean.compact"), clean).unwrap();
        let tc = Toolchain::new("compact", None);
        let out = tc.fixup(&w, "clean.compact", false).await.unwrap();
        assert!(out.ok);
        assert!(
            !out.changed,
            "fixup on already-canonical source must be a no-op"
        );
        let on_disk = std::fs::read_to_string(d.path().join("clean.compact")).unwrap();
        assert_eq!(
            on_disk, clean,
            "non-write fixup must leave the file byte-identical"
        );
    }

    // --- Hermetic tests: these short-circuit BEFORE any subprocess, so they run
    //     without `--features toolchain-tests` and need no `compact` binary. ---

    #[tokio::test]
    async fn oversize_source_is_rejected_before_any_subprocess() {
        // Over the cap by one byte. `rewrite` must reject on size before it ever
        // lexes the input or spawns the formatter.
        let (_d, w) = ws();
        let tc = Toolchain::new("compact", None);
        let huge = "x".repeat(crate::MAX_SOURCE_BYTES + 1);
        let err = tc
            .format(&w, FmtInput::Source(huge), false)
            .await
            .unwrap_err();
        assert!(matches!(err, CoreError::InvalidArgs(_)), "got {err:?}");
    }

    #[tokio::test]
    async fn write_with_inline_source_is_rejected_before_the_parse_gate() {
        // `write: true` + inline `Source` is an unconditional `InvalidArgs`
        // precondition, never a `parse_failure` — even for content that does not
        // parse (BROKEN). This proves the precondition beats the parse gate.
        let (_d, w) = ws();
        let tc = Toolchain::new("compact", None);
        let err = tc
            .format(&w, FmtInput::Source(BROKEN.into()), true)
            .await
            .unwrap_err();
        assert!(matches!(err, CoreError::InvalidArgs(_)), "got {err:?}");
    }
}
