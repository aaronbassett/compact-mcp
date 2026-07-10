use std::path::PathBuf;
use std::time::Duration;

/// Every fallible operation in this crate returns this error.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum CoreError {
    #[error("path escapes workspace root: {0}")]
    PathEscape(PathBuf),

    #[error("io error: {0}")]
    Io(#[from] std::io::Error),

    #[error("`compact` CLI not found on PATH")]
    ToolchainNotFound,

    #[error("`{cmd}` failed with exit code {code}: {stderr}")]
    ToolchainFailed {
        cmd: String,
        code: i32,
        stderr: String,
    },

    #[error("invalid arguments: {0}")]
    InvalidArgs(String),

    #[error("artifact missing: {0}")]
    ArtifactMissing(PathBuf),

    #[error("operation timed out after {0:?}")]
    Timeout(Duration),

    #[error("operation cancelled")]
    Cancelled,

    #[error("build queue is full (max {0})")]
    QueueFull(usize),

    #[error("task not found: {0}")]
    TaskNotFound(String),

    #[error("task {0} is already in a terminal state")]
    TaskTerminal(String),

    #[error("malformed artifact {path}: {reason}")]
    MalformedArtifact { path: PathBuf, reason: String },
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[test]
    fn path_escape_message_names_the_path() {
        let e = CoreError::PathEscape(PathBuf::from("/etc/passwd"));
        assert_eq!(e.to_string(), "path escapes workspace root: /etc/passwd");
    }

    #[test]
    fn toolchain_failed_reports_exit_code() {
        let e = CoreError::ToolchainFailed {
            cmd: "compact compile".into(),
            code: 255,
            stderr: "boom".into(),
        };
        assert!(e.to_string().contains("255"));
    }
}
