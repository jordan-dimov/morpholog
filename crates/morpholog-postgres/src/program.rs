//! A programme as the adapter runs it: the validated core programme plus
//! how its invariants are checked. Either every invariant compiles to SQL,
//! or the interpreter runs them all. That is decided once, here, at load.

use morpholog_core::{CompiledProgram, PredicateName, Transformation};

use crate::compiled::{CompileRefusal, CompiledInvariantSet, IndexSpec, compile_invariants};
use crate::propose::{Reads, compute_load_scope};

pub struct PgProgram {
    core: CompiledProgram,
    backend: InvariantBackend,
}

pub(crate) enum InvariantBackend {
    Compiled(CompiledInvariantSet),
    /// The invariants that kept the programme out of the fragment;
    /// empty when the interpreter was chosen without asking.
    Interpreted(Vec<CompileRefusal>),
}

/// How a programme's invariants are checked, as `check -v` reports it.
/// Decided once at load: the whole programme compiles or the whole
/// programme is interpreted.
#[derive(Debug, Clone, Copy)]
pub enum InvariantPlan<'a> {
    /// Every invariant compiles; the count is the whole programme's.
    Compiled { invariants: usize },
    /// The interpreter runs every invariant; each refusal names its
    /// invariant and the construct that kept it out. Empty when the
    /// interpreter was chosen directly rather than forced by a refusal.
    Interpreted { refusals: &'a [CompileRefusal] },
}

/// Which evaluator one execution runs its invariants through.
#[derive(Clone, Copy)]
pub(crate) enum Route<'a> {
    Compiled(&'a CompiledInvariantSet),
    Interpreted,
}

impl Route<'_> {
    /// What to load on this route. The compiled checks read the candidate
    /// from the claims table, so they need only the body's reads; the
    /// interpreter also needs what the invariants read.
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
            Err(refusals) => InvariantBackend::Interpreted(refusals),
        };
        Self { core, backend }
    }

    /// The interpreter for every invariant, whatever the programme is
    /// eligible for. Lets the benchmark run one programme through both
    /// evaluators; not a knob for embedders, since both decide the same.
    #[doc(hidden)]
    pub fn interpreted(core: CompiledProgram) -> Self {
        Self {
            core,
            backend: InvariantBackend::Interpreted(Vec::new()),
        }
    }

    pub fn core(&self) -> &CompiledProgram {
        &self.core
    }

    /// The route a production proposal takes. Diagnostics (trace,
    /// explain-on-reject) always take the interpreted route.
    pub(crate) fn route(&self) -> Route<'_> {
        match &self.backend {
            InvariantBackend::Compiled(set) => Route::Compiled(set),
            InvariantBackend::Interpreted(_) => Route::Interpreted,
        }
    }

    /// The predicates one execution of `transformation` must load on
    /// `route`, for both evaluators.
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
            InvariantBackend::Interpreted(_) => Vec::new(),
        }
    }

    pub fn plan(&self) -> InvariantPlan<'_> {
        match &self.backend {
            InvariantBackend::Compiled(set) => InvariantPlan::Compiled {
                invariants: set.invariants.len(),
            },
            InvariantBackend::Interpreted(refusals) => InvariantPlan::Interpreted { refusals },
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
