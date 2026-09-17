//! A programme as the adapter runs it: the validated core programme
//! beside the PostgreSQL execution plan built once from it. Whole-
//! programme eligibility decides the plan: every invariant compiles to
//! SQL or none does, and the interpreter runs the whole programme. The
//! decision is made here, once, and reported; nothing on a commit path
//! classifies again.

use morpholog_core::{CompiledProgram, PredicateName, Transformation};

use crate::compiled::{CompileRefusal, CompiledInvariantSet, IndexSpec, compile_invariants};
use crate::propose::{Reads, compute_load_scope};

pub struct PgProgram {
    core: CompiledProgram,
    backend: InvariantBackend,
}

pub(crate) enum InvariantBackend {
    Compiled(CompiledInvariantSet),
    /// At least one invariant is outside the fragment.
    Refused(Vec<CompileRefusal>),
    /// The interpreter by construction, eligibility never consulted.
    Pinned,
}

/// The plan for checking a programme's invariants, as `check -v`
/// reports it: what the programme is eligible for, decided once at
/// load. Two outcomes by construction - the whole programme compiles
/// or the whole programme is interpreted - so a caller matches both
/// and nothing else.
#[derive(Debug, Clone, Copy)]
pub enum InvariantPlan<'a> {
    /// Every invariant compiles; the count is the whole programme's.
    Compiled { invariants: usize },
    /// The interpreter runs every invariant; each refusal names its
    /// invariant and the construct that kept it out. Empty when the
    /// interpreter was pinned by construction rather than forced by a
    /// refusal.
    Interpreted { refusals: &'a [CompileRefusal] },
}

/// Which evaluator one execution runs its invariants through.
#[derive(Clone, Copy)]
pub(crate) enum Route<'a> {
    Compiled(&'a CompiledInvariantSet),
    Interpreted,
}

impl Route<'_> {
    /// What the loaded state must serve on this route: the compiled
    /// checks read the candidate from the claims table, so only the
    /// body's reads are loaded; the interpreter needs the invariants'
    /// too.
    pub(crate) fn reads(self) -> Reads {
        match self {
            Route::Compiled(_) => Reads::Body,
            Route::Interpreted => Reads::BodyAndInvariants,
        }
    }
}

impl PgProgram {
    pub fn new(core: CompiledProgram) -> Self {
        let backend = match compile_invariants(core.validated()) {
            Ok(set) => InvariantBackend::Compiled(set),
            Err(refusals) => InvariantBackend::Refused(refusals),
        };
        Self { core, backend }
    }

    /// The interpreter for every invariant, whatever the programme is
    /// eligible for. The benchmark's ruler: the same programme through
    /// both evaluators. Not a knob for embedders; both evaluators reach
    /// the same decisions.
    #[doc(hidden)]
    pub fn interpreted(core: CompiledProgram) -> Self {
        Self {
            core,
            backend: InvariantBackend::Pinned,
        }
    }

    pub fn core(&self) -> &CompiledProgram {
        &self.core
    }

    /// The route a production proposal takes. Diagnostics (trace,
    /// explain-on-reject) always take the interpreted route: they ask
    /// the executable specification to run.
    pub(crate) fn route(&self) -> Route<'_> {
        match &self.backend {
            InvariantBackend::Compiled(set) => Route::Compiled(set),
            InvariantBackend::Refused(_) | InvariantBackend::Pinned => Route::Interpreted,
        }
    }

    /// The predicates one execution of `transformation` must load on
    /// `route`; the one authority for both evaluators.
    pub(crate) fn load_scope(
        &self,
        transformation: &Transformation,
        route: Route<'_>,
    ) -> Vec<PredicateName> {
        let program = self.core.program();
        compute_load_scope(
            transformation,
            &program.invariants,
            &program.definitions,
            route.reads(),
        )
    }

    /// The indexes the compiled SQL can seek on; none when the
    /// programme is interpreted. What `provision indexes` reconciles.
    pub(crate) fn required_indexes(&self) -> Vec<IndexSpec> {
        match &self.backend {
            InvariantBackend::Compiled(set) => set.required_indexes(),
            InvariantBackend::Refused(_) | InvariantBackend::Pinned => Vec::new(),
        }
    }

    pub fn plan(&self) -> InvariantPlan<'_> {
        match &self.backend {
            InvariantBackend::Compiled(set) => InvariantPlan::Compiled {
                invariants: set.invariants.len(),
            },
            InvariantBackend::Refused(refusals) => InvariantPlan::Interpreted { refusals },
            InvariantBackend::Pinned => InvariantPlan::Interpreted { refusals: &[] },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The compiled route loads the body's reads alone; the interpreted
    /// route adds what the invariants read. On the ledger the balance
    /// invariant reads lines a posting never consults.
    #[test]
    fn the_compiled_route_loads_less_than_the_interpreter() {
        let core = CompiledProgram::new(morpholog_examples::double_entry_ledger::program())
            .expect("ledger compiles");
        let program = PgProgram::new(core);
        let Route::Compiled(_) = program.route() else {
            panic!("the ledger is whole-in-fragment");
        };
        let post = program
            .core()
            .transformation(&"post_simple_entry".into())
            .expect("declared")
            .clone();
        let compiled = program.load_scope(&post, program.route());
        let interpreted = program.load_scope(&post, Route::Interpreted);
        assert!(compiled.iter().all(|p| interpreted.contains(p)));
        assert!(
            interpreted.contains(&"JournalLine".into())
                && !compiled.contains(&"JournalLine".into()),
            "compiled {compiled:?}, interpreted {interpreted:?}"
        );
        let pinned = PgProgram::interpreted(
            CompiledProgram::new(morpholog_examples::double_entry_ledger::program()).unwrap(),
        );
        assert!(matches!(pinned.route(), Route::Interpreted));
        assert!(
            matches!(pinned.plan(), InvariantPlan::Interpreted { refusals } if refusals.is_empty())
        );
    }
}
