pub mod compile;
pub mod fmt;
pub mod parse_diag;
pub mod proc;
pub mod versions;

use std::process::Stdio;
use std::time::Duration;

use tokio::process::Command;
use tokio_util::sync::CancellationToken;

use crate::CoreError;

/// Default wall-clock bound for a single non-compile `compact` subprocess
/// (`list`/`check`/`update`/`clean`/`format`/`fixup`/version probes). Sized so a
/// typical `update` download clears it while a hung process still can't pin a
/// worker indefinitely. A genuinely slow download on a throttled link can trip
/// it and be killed mid-install; that is an operator-gated tool, so the bound
/// trades a rare interrupted install for a guaranteed ceiling. Override with
/// [`Toolchain::with_timeout`].
const DEFAULT_RUN_TIMEOUT: Duration = Duration::from_secs(300);

/// Per-stream cap on captured subprocess output (16 MiB of stdout AND 16 MiB of
/// stderr). Any `compact` invocation's real output — diagnostics, `list`,
/// `check`, `update` progress — sits far below this; the cap exists only so a
/// runaway or hostile subprocess (`256 MiB → ~1:1 RSS growth in <1s`) cannot
/// exhaust memory through our capture buffer. Like [`DEFAULT_RUN_TIMEOUT`] it is
/// an internal safety ceiling, not an operational knob, so it is a constant with
/// a builder override ([`Toolchain::with_output_limit`]) rather than a CLI flag.
const DEFAULT_OUTPUT_LIMIT: usize = 16 * 1024 * 1024;

#[derive(Debug, Clone)]
pub struct Output {
    pub status: i32,
    pub stdout: String,
    pub stderr: String,
}

/// A handle to the `compact` CLI. Every invocation is a fresh subprocess:
/// the resolved compiler version can change between calls, so nothing is cached.
#[derive(Debug, Clone)]
pub struct Toolchain {
    bin: String,
    compiler_version: Option<String>,
    timeout: Duration,
    output_limit: usize,
}

impl Toolchain {
    pub fn new(bin: impl Into<String>, compiler_version: Option<String>) -> Self {
        Self {
            bin: bin.into(),
            compiler_version,
            timeout: DEFAULT_RUN_TIMEOUT,
            output_limit: DEFAULT_OUTPUT_LIMIT,
        }
    }

    /// Override the per-invocation subprocess timeout (default
    /// [`DEFAULT_RUN_TIMEOUT`]). Applies to every non-compile `compact` call;
    /// the heavy `compile` path takes its own timeout per request.
    pub fn with_timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout;
        self
    }

    /// Override the per-stream captured-output cap (default
    /// [`DEFAULT_OUTPUT_LIMIT`]). Applies to both `run` and `compile`; output
    /// past the cap is drained and discarded with a truncation marker, never
    /// buffered.
    pub fn with_output_limit(mut self, limit: usize) -> Self {
        self.output_limit = limit;
        self
    }

    pub(crate) fn bin(&self) -> &str {
        &self.bin
    }

    /// Run `compact <args>` as its own process group, bounded by `self.timeout`
    /// and `ct`. Mirrors [`Toolchain::compile`]'s reaping: on timeout or
    /// cancellation the whole process tree (`compact` execs `compactc.bin`; some
    /// subcommands fork a downloader) is SIGTERM→SIGKILLed off the async worker,
    /// and `kill_on_drop` reaps the direct child if this future is simply dropped
    /// (e.g. an HTTP client disconnects). Without this a hung subprocess would be
    /// awaited forever and orphaned tools would outlive the request.
    pub(crate) async fn run(
        &self,
        args: &[&str],
        ct: &CancellationToken,
    ) -> Result<Output, CoreError> {
        if ct.is_cancelled() {
            return Err(CoreError::Cancelled);
        }

        let mut cmd = Command::new(&self.bin);
        cmd.args(args)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());

        let child = proc::spawn_group(&mut cmd).map_err(|e| match e {
            CoreError::Io(io) if io.kind() == std::io::ErrorKind::NotFound => {
                CoreError::ToolchainNotFound
            }
            other => other,
        })?;
        // Captured before `select!` moves `child` into the `wait_with_output`
        // branch. Guard `pid != 0`: if a future refactor lost the pid, `killpg(0)`
        // would signal our OWN process group — see the compile path's note.
        let pid = child.id().unwrap_or(0);
        // Reap the whole process GROUP if THIS future is dropped before any arm
        // resolves (e.g. an HTTP client disconnects mid-run). `spawn_group`'s
        // `kill_on_drop` reaps only the direct child, orphaning the forked worker;
        // this guard closes that gap. Each arm below reaps explicitly
        // (cancel/timeout) or sees the child already exit (success), so each
        // disarms the guard to avoid a double kill.
        let mut reaper = proc::KillGroupOnDrop::new(pid);

        let output = tokio::select! {
            biased;

            _ = ct.cancelled() => {
                reaper.disarm();
                if pid != 0 {
                    // `kill_group` is blocking (SIGTERM → 250ms → SIGKILL); run it
                    // off the async worker and AWAIT it so the process tree is
                    // fully reaped before we return — same discipline as `compile`.
                    let _ = tokio::task::spawn_blocking(move || proc::kill_group(pid)).await;
                }
                return Err(CoreError::Cancelled);
            }
            _ = tokio::time::sleep(self.timeout) => {
                reaper.disarm();
                if pid != 0 {
                    let _ = tokio::task::spawn_blocking(move || proc::kill_group(pid)).await;
                }
                return Err(CoreError::Timeout(self.timeout));
            }
            out = proc::wait_with_capped_output(child, self.output_limit) => {
                reaper.disarm();
                out?
            }
        };

        Ok(Output {
            status: output.status.code().unwrap_or(-1),
            stdout: proc::lossy_with_marker(
                &output.stdout,
                output.stdout_truncated,
                self.output_limit,
            ),
            stderr: proc::lossy_with_marker(
                &output.stderr,
                output.stderr_truncated,
                self.output_limit,
            ),
        })
    }

    /// The full argv for a `compact compile [+VERSION] <args...>` invocation.
    /// Single source of truth so executed and error-reported commands can't drift.
    fn compile_argv(&self, args: &[&str]) -> Vec<String> {
        let mut full: Vec<String> = vec!["compile".to_string()];
        if let Some(v) = &self.compiler_version {
            full.push(format!("+{v}"));
        }
        full.extend(args.iter().map(|s| s.to_string()));
        full
    }

    /// `compact compile [+VERSION] <args...>`. The `+VERSION` pin MUST come first.
    pub(crate) async fn run_compile(
        &self,
        args: &[&str],
        ct: &CancellationToken,
    ) -> Result<Output, CoreError> {
        let full = self.compile_argv(args);
        let refs: Vec<&str> = full.iter().map(String::as_str).collect();
        self.run(&refs, ct).await
    }

    /// One line of stdout, trimmed. Fails loudly on a non-zero exit.
    pub(crate) async fn line(
        &self,
        args: &[&str],
        compile: bool,
        ct: &CancellationToken,
    ) -> Result<String, CoreError> {
        let out = if compile {
            self.run_compile(args, ct).await?
        } else {
            self.run(args, ct).await?
        };
        if out.status != 0 {
            let argv = if compile {
                self.compile_argv(args)
            } else {
                args.iter().map(|s| s.to_string()).collect()
            };
            return Err(CoreError::ToolchainFailed {
                cmd: format!("{} {}", self.bin, argv.join(" ")),
                code: out.status,
                stderr: out.stderr,
            });
        }
        Ok(out.stdout.trim().to_string())
    }
}

impl Toolchain {
    /// Run and fail loudly on a non-zero exit (like [`Toolchain::line`], but
    /// preserves the full multi-line output instead of collapsing to one line).
    /// `run` alone never inspects `status`, so without this a failed `list`/
    /// `check` would return `Ok` carrying error text and the caller would
    /// misreport it as an empty list / not-up-to-date.
    async fn run_checked(
        &self,
        args: &[&str],
        ct: &CancellationToken,
    ) -> Result<Output, CoreError> {
        let out = self.run(args, ct).await?;
        if out.status != 0 {
            return Err(CoreError::ToolchainFailed {
                cmd: format!("{} {}", self.bin, args.join(" ")),
                code: out.status,
                stderr: out.stderr,
            });
        }
        Ok(out)
    }

    pub async fn list(&self, ct: &CancellationToken) -> Result<String, CoreError> {
        Ok(self.run_checked(&["list"], ct).await?.stdout)
    }

    pub async fn check(&self, ct: &CancellationToken) -> Result<String, CoreError> {
        let out = self.run_checked(&["check"], ct).await?;
        Ok(joined_output(&out))
    }

    /// Downloads and installs a compiler. Network + filesystem side effects.
    pub async fn update(
        &self,
        version: Option<&str>,
        ct: &CancellationToken,
    ) -> Result<String, CoreError> {
        // A version is passed straight to `compact` as an argv token. Reject a
        // non-version value (e.g. a flag like `--foo`) before it can steer the
        // subprocess — defense in depth even though the tool is operator-gated.
        // `None` means "latest".
        if let Some(v) = version {
            semver::Version::parse(v).map_err(|_| {
                CoreError::InvalidArgs(format!("not a valid compiler version: {v}"))
            })?;
        }
        let out = match version {
            Some(v) => self.run_checked(&["update", v], ct).await?,
            None => self.run_checked(&["update"], ct).await?,
        };
        Ok(joined_output(&out))
    }

    /// Removes every installed compiler version. Destructive.
    pub async fn clean(&self, ct: &CancellationToken) -> Result<String, CoreError> {
        let out = self.run_checked(&["clean"], ct).await?;
        Ok(joined_output(&out))
    }
}

/// Join a finished command's stdout and stderr for display, with a newline
/// between them only when both are non-empty — so a stdout line without a
/// trailing newline never runs straight into the first stderr line.
fn joined_output(out: &Output) -> String {
    match (out.stdout.is_empty(), out.stderr.is_empty()) {
        (false, false) => format!("{}\n{}", out.stdout, out.stderr),
        _ => format!("{}{}", out.stdout, out.stderr),
    }
}

#[cfg(test)]
mod tests {
    // `Duration` and `CancellationToken` come in via the parent's imports.
    use super::*;

    #[tokio::test]
    async fn run_times_out_instead_of_hanging() {
        // A subprocess that would run for 30s must be bounded by the toolchain
        // timeout and reaped — never awaited to completion. Hermetic: uses
        // `sleep`, so it needs no `compact` binary.
        let tc = Toolchain::new("sleep", None).with_timeout(Duration::from_millis(200));
        let start = std::time::Instant::now();
        let err = tc
            .run(&["30"], &CancellationToken::new())
            .await
            .unwrap_err();
        assert!(matches!(err, CoreError::Timeout(_)), "got {err:?}");
        assert!(
            start.elapsed() < Duration::from_secs(5),
            "run() blocked past the timeout instead of killing the child"
        );
    }

    #[tokio::test]
    async fn run_cancels_a_running_child() {
        // Cancellation that arrives WHILE the child is running (the `select!`
        // cancel arm, not the pre-spawn guard) must abort it promptly.
        let tc = Toolchain::new("sleep", None).with_timeout(Duration::from_secs(30));
        let ct = CancellationToken::new();
        let ct_child = ct.clone();
        let handle = tokio::spawn(async move { tc.run(&["30"], &ct_child).await });
        // Give the child time to spawn, then cancel.
        tokio::time::sleep(Duration::from_millis(100)).await;
        let start = std::time::Instant::now();
        ct.cancel();
        let err = handle.await.unwrap().unwrap_err();
        assert!(matches!(err, CoreError::Cancelled), "got {err:?}");
        assert!(
            start.elapsed() < Duration::from_secs(5),
            "run() did not react to cancellation"
        );
    }

    #[tokio::test]
    async fn run_returns_output_for_a_fast_command() {
        // Happy path unchanged: a quick command's stdout is captured verbatim.
        let tc = Toolchain::new("echo", None).with_timeout(Duration::from_secs(5));
        let out = tc.run(&["hello"], &CancellationToken::new()).await.unwrap();
        assert_eq!(out.status, 0);
        assert_eq!(out.stdout.trim(), "hello");
    }

    #[tokio::test]
    async fn run_caps_subprocess_output_instead_of_buffering_it_whole() {
        // A subprocess that writes far more than the cap must have its captured
        // stdout TRUNCATED (bounded memory) with a marker, not buffered whole.
        // Hermetic: `yes` piped through `head -c` emits a fixed, large byte
        // count and needs no `compact` binary.
        let tc = Toolchain::new("sh", None).with_output_limit(64);
        let out = tc
            .run(
                &["-c", "yes abcdefgh | head -c 200000"],
                &CancellationToken::new(),
            )
            .await
            .unwrap();
        assert_eq!(out.status, 0);
        assert!(
            out.stdout.len() < 4096,
            "captured stdout must be capped near the 64-byte limit, got {} bytes",
            out.stdout.len()
        );
        assert!(
            out.stdout.contains("truncated"),
            "a truncated capture must carry the marker, got {:?}",
            out.stdout
        );
    }

    #[tokio::test]
    async fn run_does_not_mark_within_limit_output_as_truncated() {
        // Regression guard: output under the cap is returned verbatim with NO
        // marker, so the cap never alters a normal (within-limit) capture.
        let tc = Toolchain::new("echo", None).with_output_limit(64);
        let out = tc.run(&["hello"], &CancellationToken::new()).await.unwrap();
        assert_eq!(out.stdout.trim(), "hello");
        assert!(!out.stdout.contains("truncated"), "got {:?}", out.stdout);
    }

    #[tokio::test]
    async fn run_returns_cancelled_for_a_pre_cancelled_token() {
        // The pre-spawn guard short-circuits before any subprocess is spawned.
        let tc = Toolchain::new("sleep", None);
        let ct = CancellationToken::new();
        ct.cancel();
        let err = tc.run(&["30"], &ct).await.unwrap_err();
        assert!(matches!(err, CoreError::Cancelled), "got {err:?}");
    }

    #[tokio::test]
    async fn run_maps_a_missing_binary_to_toolchain_not_found() {
        // A spawn `NotFound` must surface as `ToolchainNotFound`, not a raw `Io`,
        // so callers report "compact is not installed" rather than a generic error.
        let tc = Toolchain::new("compact-does-not-exist-xyz", None);
        let err = tc
            .run(&["list"], &CancellationToken::new())
            .await
            .unwrap_err();
        assert!(matches!(err, CoreError::ToolchainNotFound), "got {err:?}");
    }

    #[test]
    fn compile_argv_places_the_pin_first_and_is_the_error_source_of_truth() {
        let tc = Toolchain::new("compact", None);
        assert_eq!(
            tc.compile_argv(&["--version"]),
            vec!["compile", "--version"]
        );

        let pinned = Toolchain::new("compact", Some("0.31.0".to_string()));
        assert_eq!(
            pinned.compile_argv(&["--language-version"]),
            vec!["compile", "+0.31.0", "--language-version"]
        );
    }
}
