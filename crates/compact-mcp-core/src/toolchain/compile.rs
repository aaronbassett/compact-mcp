use std::path::PathBuf;
use std::process::Stdio;
use std::time::{Duration, Instant};

use serde::Serialize;
use tokio_util::sync::CancellationToken;

use super::{Toolchain, parse_diag::parse_compactc_output, proc};
use crate::{CoreError, Diagnostic};

#[derive(Debug, Clone)]
pub struct CompileRequest {
    pub source: PathBuf,
    pub target_dir: PathBuf,
    /// Skip PLONK proving-key generation. Cost scales with circuit size.
    pub skip_zk: bool,
    pub no_communications_commitment: bool,
    pub source_root: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct CompileOutcome {
    pub ok: bool,
    /// Always reported so a `skip_zk` build is never mistaken for a deployable one.
    pub proving_keys: bool,
    pub diagnostics: Vec<Diagnostic>,
    pub duration_ms: f64,
}

/// `compactc` exits 255 on a compile error, not 1.
const COMPACTC_ERROR_EXIT: i32 = 255;

impl Toolchain {
    pub async fn compile(
        &self,
        req: &CompileRequest,
        ct: CancellationToken,
        timeout: Duration,
    ) -> Result<CompileOutcome, CoreError> {
        if ct.is_cancelled() {
            return Err(CoreError::Cancelled);
        }

        // Build the trailing flags, then delegate the `["compile", "+VERSION"?]`
        // prefix to `compile_argv` — its doc comment names it the single source
        // of truth so the executed and error-reported argv can't drift.
        let src = req.source.to_string_lossy();
        let tgt = req.target_dir.to_string_lossy();
        // `--vscode` prints each error on a single line, for machine consumption.
        let mut trailing: Vec<&str> = vec!["--vscode"];
        if req.skip_zk {
            trailing.push("--skip-zk");
        }
        if req.no_communications_commitment {
            trailing.push("--no-communications-commitment");
        }
        if let Some(root) = &req.source_root {
            trailing.push("--sourceRoot");
            trailing.push(root.as_str());
        }
        trailing.push(&src);
        trailing.push(&tgt);
        let argv = self.compile_argv(&trailing);

        let mut cmd = tokio::process::Command::new(self.bin());
        cmd.args(&argv)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());

        let start = Instant::now();
        let child = proc::spawn_group(&mut cmd).map_err(|e| match e {
            CoreError::Io(io) if io.kind() == std::io::ErrorKind::NotFound => {
                CoreError::ToolchainNotFound
            }
            other => other,
        })?;
        let pid = child.id().unwrap_or(0);

        let output = tokio::select! {
            biased;

            _ = ct.cancelled() => {
                // `child` is moved into the `wait_with_output` branch below when
                // `select!` eagerly constructs all branch futures, so it cannot be
                // referenced here. `kill_group` reaches the whole process tree via
                // the pid captured before `select!`; when this branch wins, the
                // still-owned (never polled) `wait_with_output` future is dropped,
                // and `Child`'s `kill_on_drop` (set in `proc::spawn_group`) reaps
                // the direct child through tokio's orphan queue.
                //
                // Guard `pid != 0`: `child.id()` is `Some` here today, but if a
                // future refactor slips a `try_wait()` in before it, `pid` falls
                // back to 0 and `killpg(0, ...)` would signal OUR OWN process
                // group — a self-inflicted kill.
                if pid != 0 {
                    // `kill_group` is BLOCKING (SIGTERM -> 250ms -> SIGKILL via
                    // thread::sleep) — proc.rs's own doc says never call it
                    // directly from an async task. Hand it to a blocking thread;
                    // fire-and-forget, since spawn_blocking runs to completion
                    // even once the handle is dropped, and `killpg(pgid)` still
                    // reaps the group even after `kill_on_drop` reaps the direct
                    // child (the grandchild keeps the group alive).
                    tokio::task::spawn_blocking(move || proc::kill_group(pid));
                }
                return Err(CoreError::Cancelled);
            }
            _ = tokio::time::sleep(timeout) => {
                if pid != 0 {
                    // Off the async worker — see the cancel branch above.
                    tokio::task::spawn_blocking(move || proc::kill_group(pid));
                }
                return Err(CoreError::Timeout(timeout));
            }
            out = child.wait_with_output() => out?,
        };

        let duration_ms = start.elapsed().as_secs_f64() * 1000.0;
        let status = output.status.code().unwrap_or(-1);
        let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
        let stdout = String::from_utf8_lossy(&output.stdout).into_owned();

        match status {
            0 => Ok(CompileOutcome {
                ok: true,
                proving_keys: keys_present(&req.target_dir),
                diagnostics: Vec::new(),
                duration_ms,
            }),
            COMPACTC_ERROR_EXIT => Ok(CompileOutcome {
                ok: false,
                proving_keys: false,
                diagnostics: parse_compactc_output(&format!("{stderr}{stdout}")),
                duration_ms,
            }),
            code => Err(CoreError::ToolchainFailed {
                cmd: format!("{} {}", self.bin(), argv.join(" ")),
                code,
                stderr: format!("{stderr}{stdout}"),
            }),
        }
    }
}

/// Proving keys land in `<target>/keys/`. `--skip-zk` never creates it.
fn keys_present(target_dir: &std::path::Path) -> bool {
    std::fs::read_dir(target_dir.join("keys"))
        .map(|mut d| d.next().is_some())
        .unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Workspace;
    use std::time::Duration;
    use tokio_util::sync::CancellationToken;

    const COUNTER: &str = "pragma language_version >= 0.23;\nimport CompactStandardLibrary;\n\nexport ledger round: Counter;\n\nexport circuit increment(): [] {\n  round.increment(1);\n}\n";
    const BROKEN: &str = "pragma language_version >= 0.23;\nimport CompactStandardLibrary;\n\nexport ledger round: Counter;\n\nexport circuit bad(): Field {\n  return undefined_thing;\n}\n";

    fn setup(src: &str) -> (tempfile::TempDir, Workspace, CompileRequest) {
        let d = tempfile::tempdir().unwrap();
        std::fs::write(d.path().join("c.compact"), src).unwrap();
        let w = Workspace::new(d.path()).unwrap();
        let req = CompileRequest {
            source: w.resolve("c.compact").unwrap(),
            target_dir: w.resolve("out").unwrap(),
            skip_zk: true,
            no_communications_commitment: false,
            source_root: None,
        };
        (d, w, req)
    }

    #[tokio::test]
    #[cfg_attr(not(feature = "toolchain-tests"), ignore)]
    async fn skip_zk_compile_produces_artifacts_but_no_proving_keys() {
        let (d, _w, req) = setup(COUNTER);
        let tc = Toolchain::new("compact", None);
        let out = tc
            .compile(&req, CancellationToken::new(), Duration::from_secs(300))
            .await
            .unwrap();

        assert!(out.ok, "diagnostics: {:?}", out.diagnostics);
        assert!(!out.proving_keys, "--skip-zk must not emit keys");
        assert!(out.diagnostics.is_empty());
        assert!(d.path().join("out/compiler/contract-info.json").exists());
        assert!(d.path().join("out/contract/index.d.ts").exists());
        assert!(!d.path().join("out/keys").exists());
    }

    #[tokio::test]
    #[cfg_attr(not(feature = "toolchain-tests"), ignore)]
    async fn full_compile_emits_proving_keys() {
        let (d, _w, mut req) = setup(COUNTER);
        req.skip_zk = false;
        let tc = Toolchain::new("compact", None);
        let out = tc
            .compile(&req, CancellationToken::new(), Duration::from_secs(600))
            .await
            .unwrap();

        assert!(out.ok);
        assert!(out.proving_keys);
        assert!(d.path().join("out/keys/increment.prover").exists());
        assert!(d.path().join("out/zkir/increment.bzkir").exists());
    }

    #[tokio::test]
    #[cfg_attr(not(feature = "toolchain-tests"), ignore)]
    async fn a_broken_contract_is_not_an_error_it_is_a_diagnostic() {
        let (_d, _w, req) = setup(BROKEN);
        let tc = Toolchain::new("compact", None);
        let out = tc
            .compile(&req, CancellationToken::new(), Duration::from_secs(300))
            .await
            .expect("compile() must succeed even when the contract does not");

        assert!(!out.ok);
        assert_eq!(
            out.diagnostics.len(),
            1,
            "compactc stops at the first error"
        );
        assert_eq!(out.diagnostics[0].source, crate::Source::Compactc);
        assert!(out.diagnostics[0].message.contains("unbound identifier"));
        assert_eq!(out.diagnostics[0].span.as_ref().unwrap().start.line, 7);
    }

    #[tokio::test]
    #[cfg_attr(not(feature = "toolchain-tests"), ignore)]
    async fn cancellation_returns_cancelled() {
        let (_d, _w, mut req) = setup(COUNTER);
        req.skip_zk = false;
        let tc = Toolchain::new("compact", None);
        let ct = CancellationToken::new();
        ct.cancel(); // pre-cancelled
        let err = tc
            .compile(&req, ct, Duration::from_secs(600))
            .await
            .unwrap_err();
        assert!(matches!(err, CoreError::Cancelled));
    }
}
