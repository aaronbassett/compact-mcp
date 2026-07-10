//! Domain logic for compact-mcp. Contains no MCP protocol knowledge.

pub mod diagnostic;
pub mod error;
pub mod workspace;

pub use diagnostic::{Diagnostic, Position, Severity, Source, Span};
pub use error::CoreError;
pub use workspace::{TempScope, Workspace};
