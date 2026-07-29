#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
//! Spike step 7: differential harness v1. Every whole-programme
//! in-fragment example is swept with eval_totality-style argument
//! vectors over a depth-2 governed frontier held in PG; every probe runs
//! `propose_differential`, which takes the kernel's verdict and both
//! compiled stages against ONE snapshot and errors on any disagreement.
//!
//! Generator helpers are copied from
//! crates/morpholog-examples/tests/eval_totality.rs (file-local there;
//! copying beats refactoring on a throwaway branch).

mod common;

use common::{reset_db, test_pool};
use morpholog_core::{
    CompiledProgram, EvalValue, ParamKind, PredicateArgKind, Program, TransformationName,
    ValidatedProgram, transformation_param_kinds,
};
use morpholog_postgres::spike::{CompiledInvariantSet, compile_invariants, propose_differential};
use morpholog_postgres::{PgError, PgPool, PgProposalOutcome, Proposal};
use morpholog_test_support::{bool_, coll, date, dec, dec_str, dur, qty, subj, ts};

const REACHABILITY_DEPTH: usize = 2;
const SHARED_SUBJECT: &str = "shared";
const DECIMAL_MAX: &str = "79228162514264337593543950335";

fn baseline(kind: &ParamKind, name: &str) -> EvalValue {
    match kind {
        ParamKind::Concrete(k) => baseline_concrete(k, name),
        ParamKind::Collection(inner) => coll(vec![baseline(inner, name)]),
        ParamKind::Polymorphic | ParamKind::Unconstrained => subj(name),
        ParamKind::Ambiguous(kinds) => kinds
            .first()
            .map(|k| baseline_concrete(k, name))
            .unwrap_or_else(|| subj(name)),
    }
}

fn baseline_concrete(kind: &PredicateArgKind, name: &str) -> EvalValue {
    match kind {
        PredicateArgKind::Subject | PredicateArgKind::Any => subj(name),
        PredicateArgKind::Decimal => dec(1),
        PredicateArgKind::Date => date("2026-07-01"),
        PredicateArgKind::Timestamp => ts("2026-07-01T12:00:00Z"),
        PredicateArgKind::Duration => dur("PT1H"),
        PredicateArgKind::Bool => bool_(true),
        PredicateArgKind::Quantity(unit) => qty("1", unit.as_str()),
        PredicateArgKind::Collection => coll(vec![subj(name)]),
    }
}

struct Witness {
    value: EvalValue,
    extreme: bool,
}

fn ordinary(value: EvalValue) -> Witness {
    Witness {
        value,
        extreme: false,
    }
}

fn boundary_witnesses(kind: &ParamKind, name: &str) -> Vec<Witness> {
    match kind {
        ParamKind::Concrete(k) => boundary_concrete(k, name),
        ParamKind::Collection(_) => vec![ordinary(coll(vec![]))],
        ParamKind::Polymorphic | ParamKind::Unconstrained => {
            vec![ordinary(subj(SHARED_SUBJECT))]
        }
        ParamKind::Ambiguous(kinds) => kinds
            .first()
            .map(|k| boundary_concrete(k, name))
            .unwrap_or_default(),
    }
}

fn boundary_concrete(kind: &PredicateArgKind, _name: &str) -> Vec<Witness> {
    match kind {
        PredicateArgKind::Subject | PredicateArgKind::Any => vec![ordinary(subj(SHARED_SUBJECT))],
        PredicateArgKind::Decimal => vec![
            ordinary(dec(0)),
            ordinary(dec(-1)),
            Witness {
                value: dec_str(DECIMAL_MAX),
                extreme: true,
            },
        ],
        PredicateArgKind::Quantity(unit) => vec![
            ordinary(qty("0", unit.as_str())),
            ordinary(qty("-1", unit.as_str())),
            Witness {
                value: qty(DECIMAL_MAX, unit.as_str()),
                extreme: true,
            },
        ],
        PredicateArgKind::Bool => vec![ordinary(bool_(false))],
        PredicateArgKind::Collection => vec![ordinary(coll(vec![]))],
        PredicateArgKind::Date | PredicateArgKind::Timestamp | PredicateArgKind::Duration => vec![],
    }
}

struct ArgumentCase {
    args: Vec<EvalValue>,
    permits_range_refusal: bool,
}

fn argument_vectors(kinds: &[(String, ParamKind)]) -> Vec<ArgumentCase> {
    let base: Vec<EvalValue> = kinds.iter().map(|(n, k)| baseline(k, n)).collect();
    let mut vectors = vec![ArgumentCase {
        args: base.clone(),
        permits_range_refusal: false,
    }];
    for (i, (name, kind)) in kinds.iter().enumerate() {
        for witness in boundary_witnesses(kind, name) {
            let mut varied = base.clone();
            varied[i] = witness.value;
            vectors.push(ArgumentCase {
                args: varied,
                permits_range_refusal: witness.extreme,
            });
        }
    }
    vectors
}

fn param_kinds(
    validated: &ValidatedProgram<'_>,
    name: &TransformationName,
) -> Vec<(String, ParamKind)> {
    transformation_param_kinds(validated, name)
        .expect("corpus parameter kinds resolve")
        .into_iter()
        .map(|(v, k)| (v.to_string(), k))
        .collect()
}

fn proposal_for(
    compiled: &CompiledProgram,
    name: &TransformationName,
    args: Vec<EvalValue>,
) -> Proposal {
    let t = compiled.transformation(name).expect("known transformation");
    let transition = morpholog_test_support::test_transition(t, args);
    Proposal::gateway(&transition)
}

/// One probe: parity is asserted inside `propose_differential`; a
/// `Kernel` error is lawful only on range-extreme vectors (the compiled
/// path is deliberately not consulted there - PG numeric would not
/// overflow at rust_decimal's bound; recorded as a residual risk).
async fn probe(
    pool: &PgPool,
    compiled: &CompiledProgram,
    sql_set: &CompiledInvariantSet,
    name: &TransformationName,
    case: &ArgumentCase,
) -> Option<PgProposalOutcome> {
    let proposal = proposal_for(compiled, name, case.args.clone());
    match propose_differential(pool, compiled, sql_set, &proposal).await {
        Ok(outcome) => Some(outcome),
        Err(PgError::Kernel(err)) if case.permits_range_refusal => {
            let text = err.to_string();
            assert!(
                text.contains("range") || text.contains("Range"),
                "extreme vector raised a non-range kernel error: {text}"
            );
            None
        }
        Err(e) => panic!("differential probe failed on {name}: {e}"),
    }
}

async fn replay_chain(
    pool: &PgPool,
    compiled: &CompiledProgram,
    sql_set: &CompiledInvariantSet,
    chain: &[(TransformationName, Vec<EvalValue>)],
) {
    for (name, args) in chain {
        let proposal = proposal_for(compiled, name, args.clone());
        let outcome = propose_differential(pool, compiled, sql_set, &proposal)
            .await
            .expect("replaying an accepted chain step");
        assert!(
            matches!(outcome, PgProposalOutcome::Committed { .. }),
            "chain step must re-commit deterministically"
        );
    }
}

async fn sweep(program: Program) {
    let pool = test_pool().await;
    let compiled = CompiledProgram::new(program).expect("corpus programme is valid");
    let sql_set =
        compile_invariants(compiled.program()).expect("corpus programme is whole-in-fragment");

    let names: Vec<TransformationName> = compiled
        .program()
        .transformations
        .iter()
        .map(|t| t.name.clone())
        .collect();
    let validated = compiled.validated();
    let vectors: Vec<(TransformationName, Vec<ArgumentCase>)> = names
        .iter()
        .map(|n| (n.clone(), argument_vectors(&param_kinds(&validated, n))))
        .collect();

    let mut probes = 0usize;
    let mut chains: Vec<Vec<(TransformationName, Vec<EvalValue>)>> = vec![vec![]];
    for _depth in 0..REACHABILITY_DEPTH {
        let mut next = Vec::new();
        for chain in &chains {
            for (name, cases) in &vectors {
                for (i, case) in cases.iter().enumerate() {
                    reset_db(&pool).await;
                    replay_chain(&pool, &compiled, &sql_set, chain).await;
                    let outcome = probe(&pool, &compiled, &sql_set, name, case).await;
                    probes += 1;
                    let is_baseline = i == 0;
                    if is_baseline && matches!(outcome, Some(PgProposalOutcome::Committed { .. })) {
                        let mut extended = chain.clone();
                        extended.push((name.clone(), case.args.clone()));
                        next.push(extended);
                    }
                }
            }
        }
        chains = next;
        if chains.is_empty() {
            break;
        }
    }
    println!(
        "{}: {} differential probes, zero disagreements",
        compiled.program().name,
        probes
    );
}

#[tokio::test]
async fn ledger_differential_sweep() {
    sweep(morpholog_examples::double_entry_ledger::program()).await;
}

#[tokio::test]
async fn settlement_netting_differential_sweep() {
    sweep(morpholog_examples::settlement_netting::program()).await;
}

#[tokio::test]
async fn verified_revenue_differential_sweep() {
    sweep(morpholog_examples::verified_revenue::program()).await;
}

#[tokio::test]
async fn approval_controls_differential_sweep() {
    sweep(morpholog_examples::approval_controls::program()).await;
}

#[tokio::test]
async fn carbon_credit_provenance_differential_sweep() {
    sweep(morpholog_examples::carbon_credit_provenance::program()).await;
}
