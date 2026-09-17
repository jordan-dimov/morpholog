//! A programme as the adapter runs it: the validated core programme
//! beside the PostgreSQL execution plan built once from it. Whole-
//! programme eligibility decides the plan: every invariant compiles to
//! SQL or none does, and the interpreter runs the whole programme. The
//! decision is made here, once, and reported; nothing on a commit path
//! classifies again.

use morpholog_core::CompiledProgram;

use crate::compiled::{CompileRefusal, CompiledInvariantSet, compile_invariants};

pub struct PgProgram {
    core: CompiledProgram,
    backend: InvariantBackend,
}

pub(crate) enum InvariantBackend {
    Compiled(CompiledInvariantSet),
    Interpreted(Vec<CompileRefusal>),
}

/// How a programme's invariants are checked, as `check -v` reports it.
#[derive(Debug, Clone, Copy)]
pub enum Backend<'a> {
    /// Every invariant compiles; the count is the whole programme's.
    Compiled { invariants: usize },
    /// At least one invariant is outside the compiled fragment, so the
    /// interpreter runs them all; each refusal names its invariant and
    /// the construct that kept it out.
    Interpreted { refusals: &'a [CompileRefusal] },
}

impl PgProgram {
    pub fn new(core: CompiledProgram) -> Self {
        let backend = match compile_invariants(core.validated()) {
            Ok(set) => InvariantBackend::Compiled(set),
            Err(refusals) => InvariantBackend::Interpreted(refusals),
        };
        Self { core, backend }
    }

    pub fn core(&self) -> &CompiledProgram {
        &self.core
    }

    pub fn backend(&self) -> Backend<'_> {
        match &self.backend {
            InvariantBackend::Compiled(set) => Backend::Compiled {
                invariants: set.invariants.len(),
            },
            InvariantBackend::Interpreted(refusals) => Backend::Interpreted { refusals },
        }
    }
}
