pub mod contract_info;
pub mod zkir;

pub use contract_info::{Argument, Circuit, ContractInfo, LedgerField, TypeRef, Witness, ts_type};
// Re-export the zkir TYPES for symmetry with contract_info. The `stats` free
// function stays module-qualified (`zkir::stats`) so a future artifact module
// with its own `stats` cannot collide at this level.
pub use zkir::{Instruction, Zkir, ZkirStats, ZkirVersion};
