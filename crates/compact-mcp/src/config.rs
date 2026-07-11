use std::path::PathBuf;

use clap::{Parser, ValueEnum};

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum Transport {
    Stdio,
    Http,
}

#[derive(Debug, Clone, Parser)]
#[command(
    name = "compact-mcp",
    version,
    about = "MCP server for the Compact toolchain"
)]
pub struct Config {
    #[arg(
        long,
        value_enum,
        default_value = "stdio",
        env = "COMPACT_MCP_TRANSPORT"
    )]
    pub transport: Transport,

    /// Root directory. No tool may touch a path outside it.
    #[arg(long, env = "COMPACT_MCP_WORKSPACE_ROOT")]
    pub workspace_root: Option<PathBuf>,

    #[arg(long, default_value = "127.0.0.1:8080")]
    pub bind: String,

    /// Required to bind a non-loopback address. There is no authorization.
    #[arg(long)]
    pub allow_insecure_bind: bool,

    /// Register the destructive `toolchain_update` (installs a compiler binary)
    /// and `toolchain_clean` (deletes ALL installed compilers) tools. Off by
    /// default — over HTTP these are remote "install a binary" / "wipe the
    /// toolchain" primitives an agent should not reach unless you opt in.
    #[arg(long)]
    pub allow_toolchain_mutation: bool,

    #[arg(long, default_value = "1")]
    pub max_concurrent_builds: usize,

    #[arg(long, default_value = "8")]
    pub max_queued_builds: usize,

    #[arg(long, default_value = "900")]
    pub compile_timeout: u64,

    #[arg(long, default_value = "900")]
    pub default_task_ttl: u64,

    #[arg(long, default_value = "3600")]
    pub max_task_ttl: u64,

    /// Pin the compiler, passed to `compact compile` as `+VERSION`.
    #[arg(long)]
    pub compiler_version: Option<String>,

    #[arg(long, default_value = "compact")]
    pub compact_bin: String,
}

impl Config {
    pub fn resolved_workspace_root(&self) -> anyhow::Result<PathBuf> {
        match (&self.workspace_root, self.transport) {
            (Some(p), _) => Ok(p.clone()),
            (None, Transport::Stdio) => Ok(std::env::current_dir()?),
            (None, Transport::Http) => {
                anyhow::bail!("--workspace-root is required with --transport http")
            }
        }
    }
}
