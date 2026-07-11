pub mod compile;
pub mod fmt;
pub mod parse_diag;
pub mod proc;
pub mod versions;

use std::process::Stdio;

use tokio::process::Command;

use crate::CoreError;

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
}

impl Toolchain {
    pub fn new(bin: impl Into<String>, compiler_version: Option<String>) -> Self {
        Self {
            bin: bin.into(),
            compiler_version,
        }
    }

    pub(crate) fn bin(&self) -> &str {
        &self.bin
    }

    pub(crate) async fn run(&self, args: &[&str]) -> Result<Output, CoreError> {
        let out = Command::new(&self.bin)
            .args(args)
            .stdin(Stdio::null())
            .output()
            .await
            .map_err(|e| match e.kind() {
                std::io::ErrorKind::NotFound => CoreError::ToolchainNotFound,
                _ => CoreError::Io(e),
            })?;

        Ok(Output {
            status: out.status.code().unwrap_or(-1),
            stdout: String::from_utf8_lossy(&out.stdout).into_owned(),
            stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
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
    pub(crate) async fn run_compile(&self, args: &[&str]) -> Result<Output, CoreError> {
        let full = self.compile_argv(args);
        let refs: Vec<&str> = full.iter().map(String::as_str).collect();
        self.run(&refs).await
    }

    /// One line of stdout, trimmed. Fails loudly on a non-zero exit.
    pub(crate) async fn line(&self, args: &[&str], compile: bool) -> Result<String, CoreError> {
        let out = if compile {
            self.run_compile(args).await?
        } else {
            self.run(args).await?
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
    async fn run_checked(&self, args: &[&str]) -> Result<Output, CoreError> {
        let out = self.run(args).await?;
        if out.status != 0 {
            return Err(CoreError::ToolchainFailed {
                cmd: format!("{} {}", self.bin, args.join(" ")),
                code: out.status,
                stderr: out.stderr,
            });
        }
        Ok(out)
    }

    pub async fn list(&self) -> Result<String, CoreError> {
        Ok(self.run_checked(&["list"]).await?.stdout)
    }

    pub async fn check(&self) -> Result<String, CoreError> {
        let out = self.run_checked(&["check"]).await?;
        Ok(joined_output(&out))
    }

    /// Downloads and installs a compiler. Network + filesystem side effects.
    pub async fn update(&self, version: Option<&str>) -> Result<String, CoreError> {
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
            Some(v) => self.run_checked(&["update", v]).await?,
            None => self.run_checked(&["update"]).await?,
        };
        Ok(joined_output(&out))
    }

    /// Removes every installed compiler version. Destructive.
    pub async fn clean(&self) -> Result<String, CoreError> {
        let out = self.run_checked(&["clean"]).await?;
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
    use super::*;

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
