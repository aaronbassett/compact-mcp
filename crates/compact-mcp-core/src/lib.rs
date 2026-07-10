//! Domain logic for compact-mcp. Contains no MCP protocol knowledge.

pub mod analyze;
pub mod artifacts;
pub mod diagnostic;
pub mod error;
pub mod jobs;
pub mod toolchain;
pub mod workspace;

pub use analyze::MAX_SOURCE_BYTES;
pub use diagnostic::{Diagnostic, Position, Severity, Source, Span};
pub use error::CoreError;
pub use toolchain::Toolchain;
pub use workspace::{TempScope, Workspace};
