//! Domain logic for compact-mcp. Contains no MCP protocol knowledge.

pub mod error;
pub mod workspace;

pub use error::CoreError;
pub use workspace::{TempScope, Workspace};
