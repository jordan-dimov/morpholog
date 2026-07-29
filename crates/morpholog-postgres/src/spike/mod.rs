//! SPIKE - throwaway branch code, never merges. Compiles fragment
//! invariants to SQL violation queries executed inside the propose
//! transaction; the sync kernel remains the executable specification.

mod compile;

pub use compile::{
    CaseFilter, CompileRefusal, CompiledInvariant, CompiledInvariantSet, compile_invariants,
};
