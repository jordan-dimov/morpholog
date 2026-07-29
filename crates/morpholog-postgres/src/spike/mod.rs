//! SPIKE - throwaway branch code, never merges. Compiles fragment
//! invariants to SQL violation queries executed inside the propose
//! transaction; the sync kernel remains the executable specification.

mod compile;
mod indexes;
mod propose;

pub use compile::{
    CaseFilter, CompileRefusal, CompiledInvariant, CompiledInvariantSet, compile_invariants,
};
pub use indexes::{drop_spike_index_sql, spike_index_sql};
pub use propose::{Stage, propose_against_pg_compiled, propose_differential};
