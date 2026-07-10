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

    /// `compact compile [+VERSION] <args...>`. The `+VERSION` pin MUST come first.
    pub(crate) async fn run_compile(&self, args: &[&str]) -> Result<Output, CoreError> {
        let mut full: Vec<String> = vec!["compile".to_string()];
        if let Some(v) = &self.compiler_version {
            full.push(format!("+{v}"));
        }
        full.extend(args.iter().map(|s| s.to_string()));
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
            return Err(CoreError::ToolchainFailed {
                cmd: format!("{} {}", self.bin, args.join(" ")),
                code: out.status,
                stderr: out.stderr,
            });
        }
        Ok(out.stdout.trim().to_string())
    }
}
