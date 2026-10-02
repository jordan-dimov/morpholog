//! A programme as the adapter runs it: a prepared programme bound to its
//! execution plan, which says how its invariants are checked. Each invariant compiles to SQL or the
//! interpreter keeps it, decided once, here, at load; an execution walks
//! them in programme order, each run of like kind through its evaluator.

use morpholog_core::{PreparedProgram, ReadPlan, Transformation, Transition};

use crate::compiled::{CompileRefusal, CompiledInvariantSet, IndexSpec, Run, compile_each};
use crate::propose::{LoadScope, Reads, compute_load_scope};

/// The PostgreSQL binding of a programme: the prepared programme and the
/// execution plan that runs it. The plan changes what a proposal costs,
/// never what it decides.
pub struct PgProgram {
    prepared: PreparedProgram,
    execution: ExecutionPlan,
}

/// The execution plan: how a programme's invariants are checked. The
/// ones that compiled, in programme order, the refusals that keep the
/// rest with the interpreter, and the runs that say which evaluator
/// checks which, in order. Every invariant compiled, none did, and some
/// did are the same shape.
pub(crate) struct ExecutionPlan {
    pub(crate) compiled: CompiledInvariantSet,
    /// Empty when the interpreter was chosen without asking.
    refusals: Vec<CompileRefusal>,
    pub(crate) runs: Vec<Run>,
    /// The programme's invariants the interpreter keeps, by index.
    interpreted: Vec<usize>,
}

impl ExecutionPlan {
    fn interpreted_indices(runs: &[Run]) -> Vec<usize> {
        runs.iter()
            .filter_map(|run| match run {
                Run::Interpreted(range) => Some(range.clone()),
                Run::Compiled(_) => None,
            })
            .flatten()
            .collect()
    }
}

/// How a programme's invariants are checked, as `check -v` reports it.
/// Decided once at load, invariant by invariant.
#[derive(Debug, Clone, Copy)]
pub enum InvariantPlan<'a> {
    /// Every invariant compiles.
    Compiled,
    /// The interpreter runs every invariant; each refusal names its
    /// invariant and the construct that kept it out. Empty when the
    /// interpreter was chosen directly rather than forced by a refusal.
    Interpreted { refusals: &'a [CompileRefusal] },
    /// The compiled checks run the invariants that compile, `compiled`
    /// of them, and the interpreter the rest, each named by its refusal.
    Mixed {
        compiled: usize,
        refusals: &'a [CompileRefusal],
    },
}

/// Which evaluator one execution runs its invariants through.
#[derive(Clone, Copy)]
pub(crate) enum Route<'a> {
    Compiled(&'a CompiledInvariantSet),
    Interpreted,
    /// Both, in programme order, each run of like kind through its
    /// evaluator, over one effective delta the claims table reports.
    Mixed(&'a ExecutionPlan),
}

impl<'a> Route<'a> {
    /// What to load on this route. The compiled checks read the candidate
    /// from the claims table, so they need only the body's reads; the
    /// interpreter also needs what the invariants read, and, when it
    /// decides the effective delta itself, what the body admits. A mixed
    /// execution takes the effective delta from the table, so it needs the
    /// interpreted invariants' reads and nothing for the admits.
    pub(crate) fn reads(self) -> Reads<'a> {
        match self {
            Route::Compiled(_) => Reads::Body,
            Route::Interpreted => Reads::BodyAndInvariants,
            Route::Mixed(execution) => Reads::BodyAndSome(&execution.interpreted),
        }
    }
}

impl PgProgram {
    pub fn new(prepared: PreparedProgram) -> Self {
        let compilation = compile_each(prepared.validated());
        let interpreted = ExecutionPlan::interpreted_indices(&compilation.runs);
        Self {
            prepared,
            execution: ExecutionPlan {
                compiled: compilation.compiled,
                refusals: compilation.refusals,
                runs: compilation.runs,
                interpreted,
            },
        }
    }

    /// The interpreter for every invariant, whatever the programme is
    /// eligible for. Lets the benchmark run one programme through both
    /// evaluators; not a knob for embedders, since both decide the same.
    #[doc(hidden)]
    pub fn interpreted(prepared: PreparedProgram) -> Self {
        let count = prepared.program().invariants.len();
        let runs = if count == 0 {
            Vec::new()
        } else {
            vec![Run::Interpreted(0..count)]
        };
        Self {
            prepared,
            execution: ExecutionPlan {
                compiled: CompiledInvariantSet {
                    invariants: Vec::new(),
                },
                refusals: Vec::new(),
                interpreted: ExecutionPlan::interpreted_indices(&runs),
                runs,
            },
        }
    }

    pub fn prepared(&self) -> &PreparedProgram {
        &self.prepared
    }

    /// The route a production proposal takes. Diagnostics (trace,
    /// explain-on-reject) always take the interpreted route.
    pub(crate) fn route(&self) -> Route<'_> {
        let execution = &self.execution;
        let compiled = execution.compiled.invariants.len();
        if execution.interpreted.is_empty() {
            Route::Compiled(&execution.compiled)
        } else if compiled == 0 {
            Route::Interpreted
        } else {
            Route::Mixed(execution)
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
        let program = self.prepared.program();
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
        let program = self.prepared.program();
        let mut specs: Vec<IndexSpec> = program
            .transformations
            .iter()
            .flat_map(|t| ReadPlan::of(t, &program.definitions).seek_positions())
            .map(|(predicate, position)| IndexSpec::new(predicate, position))
            .collect();
        specs.extend(self.execution.compiled.required_indexes());
        specs.sort();
        specs.dedup();
        specs
    }

    pub fn plan(&self) -> InvariantPlan<'_> {
        let execution = &self.execution;
        let compiled = execution.compiled.invariants.len();
        if execution.interpreted.is_empty() {
            InvariantPlan::Compiled
        } else if compiled == 0 {
            InvariantPlan::Interpreted {
                refusals: &execution.refusals,
            }
        } else {
            InvariantPlan::Mixed {
                compiled,
                refusals: &execution.refusals,
            }
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
        let prepared = PreparedProgram::new(morpholog_examples::double_entry_ledger::program())
            .expect("ledger is valid");
        let program = PgProgram::new(prepared);
        let Route::Compiled(_) = program.route() else {
            panic!("the ledger is whole-in-fragment");
        };
        let post = program
            .prepared()
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
            PreparedProgram::new(morpholog_examples::double_entry_ledger::program()).unwrap(),
        );
        assert!(matches!(pinned.route(), Route::Interpreted));
        assert!(
            matches!(pinned.plan(), InvariantPlan::Interpreted { refusals } if refusals.is_empty())
        );
    }
}
