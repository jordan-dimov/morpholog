//! A programme as the adapter runs it: the validated core programme plus
//! how its invariants are checked. Either every invariant compiles to SQL,
//! or the interpreter runs them all. That is decided once, here, at load.

use morpholog_core::{CompiledProgram, ReadPlan, Transformation, Transition};

use crate::compiled::{CompileRefusal, CompiledInvariantSet, IndexSpec, compile_invariants};
use crate::propose::{LoadScope, Reads, compute_load_scope};

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
    /// Every invariant compiles.
    Compiled,
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

    /// What one execution of `transformation` must load on `route`, for
    /// both evaluators, keyed by the transition's values.
    pub(crate) fn load_scope(
        &self,
        transformation: &Transformation,
        transition: &Transition,
        route: Route<'_>,
    ) -> LoadScope {
        let program = self.core.program();
        compute_load_scope(
            transformation,
            Some(transition),
            &program.invariants,
            &program.definitions,
            route.reads(),
        )
    }

    /// The indexes this programme's executions seek on: every position a
    /// transformation's reads key and one per keyed admit, on either
    /// route, plus the compiled checks' own when the programme compiles.
    /// What `provision indexes` reconciles; an interpreted programme has
    /// the same physical contract as a compiled one for its loads.
    pub(crate) fn required_indexes(&self) -> Vec<IndexSpec> {
        let program = self.core.program();
        let mut specs: Vec<IndexSpec> = program
            .transformations
            .iter()
            .flat_map(|t| ReadPlan::of(t, &program.definitions).seek_positions())
            .map(|(predicate, position)| IndexSpec::new(predicate, position))
            .collect();
        if let InvariantBackend::Compiled(set) = &self.backend {
            specs.extend(set.required_indexes());
        }
        specs.sort();
        specs.dedup();
        specs
    }

    pub fn plan(&self) -> InvariantPlan<'_> {
        match &self.backend {
            InvariantBackend::Compiled(_) => InvariantPlan::Compiled,
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
        let transition = morpholog_core::Transition {
            transformation_name: post.name.clone(),
            args: vec![
                morpholog_test_support::subj("e1"),
                morpholog_test_support::subj("d1"),
                morpholog_test_support::subj("p1"),
                morpholog_test_support::subj("cash"),
                morpholog_test_support::subj("rev"),
                morpholog_test_support::dec(1),
            ],
            actor: morpholog_test_support::test_actor(),
        };
        let compiled = program
            .load_scope(&post, &transition, program.route())
            .predicates();
        let interpreted = program
            .load_scope(&post, &transition, Route::Interpreted)
            .predicates();
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
