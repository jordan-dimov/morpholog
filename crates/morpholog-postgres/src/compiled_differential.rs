//! Proves the compiled SQL checks agree with the kernel. Both judge the
//! exact same staged delta inside one SERIALIZABLE transaction.
//! `DATABASE_URL`-gated.
//!
//! The body is staged ONCE and the kernel judges that same `StagedDelta`,
//! so a body minting `new Subject()` cannot give the two sides different
//! candidates. Every probe rolls back (no audit, outbox or commit), so a
//! state is built once and probed many times.
//!
//! Two contracts:
//!
//! - **Governed history** (states reached only through accepted
//!   proposals): the kernel, the full SQL check and the case-bound SQL
//!   check agree on the verdict. On rejection, the first failing rule's
//!   name, version and witness variable set must match; witness values
//!   may differ (a symmetric self-join can name a pair in either order).
//! - **Dirty history** (rows the kernel never admitted): the kernel and
//!   the case-bound check still agree. The full check asks the
//!   whole-state question, as `evaluate` does, and may refuse where they
//!   admit.
//!
//! A kernel error while checking invariants (a sum too large for any
//! decimal) must come back from both compiled checks as the same typed
//! error, and the hostile sweep must reach one. Only a body error is
//! skipped: no compiled check runs for a body that could not stage.

use std::fmt::Write as _;

use morpholog_core::{
    CompiledProgram, EvalError, EvalValue, Outcome, Program, RejectionReason, StagedDelta, Subject,
    Transition, finish_staged_delta_with, propose_stage_delta,
};
use uuid::Uuid;

use crate::attestation::Proposal;
use crate::compiled::{CompiledInvariantSet, SqlViolation, Stage, compile_invariants, disable_jit};
use crate::error::{PgError, classify};
use crate::program::PgProgram;
use crate::propose::{Reads, compute_load_scope, load_state, write_claim_delta};
use crate::txn::begin_authorised_proposal_tx;
use crate::{PgPool, PgProposalOutcome, propose_against_pg};

use morpholog_test_support::differential::{boundary_argument_cases, is_permitted_range_error};
use morpholog_test_support::{dec, subj, test_actor};

/// One accepted step from empty, then every transformation again: deep
/// enough that invariants first get evaluated. Rollback-only probes keep
/// this cheap enough to run in full everywhere.
const REACHABILITY_DEPTH: usize = 2;

pub(crate) async fn test_pool() -> PgPool {
    let url = std::env::var("DATABASE_URL")
        .expect("DATABASE_URL must be set for this test (e.g. postgres:///morpholog_dev)");
    let url = crate::with_default_user(&url);
    PgPool::connect(&url)
        .await
        .expect("failed to connect to PostgreSQL test database")
}

async fn reset_db(pool: &PgPool) {
    sqlx::query(crate::testing::RESET_SQL)
        .execute(pool)
        .await
        .expect("failed to truncate test DB");
}

/// What the three evaluators said about one staged candidate. `kernel` is
/// `None` when the body itself rejected: only the kernel runs the body.
struct ProbeObservation {
    kernel: Option<Outcome>,
    stage1: Option<SqlViolation>,
    stage2: Option<SqlViolation>,
}

enum Probe {
    BodyRejected,
    Observed(Box<ProbeObservation>),
    /// The kernel's invariant check errored and both stages reported
    /// the same typed error.
    KernelErrorAgreed,
}

/// Stage once, judge three times, roll back. The caller applies the
/// contract.
async fn probe_raw(
    pool: &PgPool,
    compiled: &CompiledProgram,
    sql_set: &CompiledInvariantSet,
    transformation_name: &str,
    args: Vec<EvalValue>,
) -> Result<Probe, ProbeFailure> {
    let transition = Transition {
        transformation_name: transformation_name.into(),
        args,
        actor: Subject::from("differential"),
    };
    let (transformation, invariants, definitions) =
        crate::propose::resolve(compiled, &transition.transformation_name)
            .map_err(ProbeFailure::Pg)?;

    let (mut tx, _login_role) = begin_authorised_proposal_tx(pool, &transition.actor)
        .await
        .map_err(ProbeFailure::Pg)?;
    // The kernel judges this probe too, so its invariants' reads load.
    let scope = compute_load_scope(
        transformation,
        invariants,
        definitions,
        Reads::BodyAndInvariants,
    );
    let state = load_state(&mut tx, &scope)
        .await
        .map_err(ProbeFailure::Pg)?;

    let staged = propose_stage_delta(transformation, &transition, &state, definitions)
        .map_err(ProbeFailure::Kernel)?;
    let StagedDelta::Staged {
        asserted,
        retracted,
        ..
    } = &staged
    else {
        tx.rollback()
            .await
            .map_err(|e| ProbeFailure::Pg(classify(e)))?;
        return Ok(Probe::BodyRejected);
    };
    let asserted = asserted.clone();
    let retracted = retracted.clone();

    // The kernel's verdict, from the SAME staged delta the claims
    // table is about to receive. An error while the kernel checks the
    // invariants (a sum past the decimal range) is a verdict the
    // compiled checks must reproduce as the same typed error.
    let kernel = finish_staged_delta_with(staged, &state, &compiled.admission());

    let transition_id = Uuid::now_v7();
    let effective = write_claim_delta(&mut tx, transition_id, &asserted, &retracted)
        .await
        .map_err(ProbeFailure::Pg)?;

    disable_jit(&mut tx).await.map_err(ProbeFailure::Pg)?;
    let stage1 = sql_set
        .first_violation(&mut tx, Stage::Full, &asserted, &retracted)
        .await;
    let stage2 = sql_set
        .first_violation(
            &mut tx,
            Stage::CaseBound,
            &effective.asserted,
            &effective.retracted,
        )
        .await;

    // Observationally inert: every probe rolls back, whatever it saw.
    tx.rollback()
        .await
        .map_err(|e| ProbeFailure::Pg(classify(e)))?;

    let kernel = match kernel {
        Ok(outcome) => outcome,
        Err(expected) => {
            for (label, stage) in [("full", stage1), ("case-bound", stage2)] {
                match stage {
                    Err(PgError::Kernel(got)) if got == expected => {}
                    other => {
                        return Err(ProbeFailure::Disagreement(disagreement(
                            &format!("{label} kernel error"),
                            &format!("{expected:?}"),
                            &format!("{other:?}"),
                        )));
                    }
                }
            }
            return Ok(Probe::KernelErrorAgreed);
        }
    };
    let stage1 = stage1.map_err(ProbeFailure::Pg)?;
    let stage2 = stage2.map_err(ProbeFailure::Pg)?;

    Ok(Probe::Observed(Box::new(ProbeObservation {
        kernel: Some(kernel),
        stage1,
        stage2,
    })))
}

enum ProbeFailure {
    Pg(PgError),
    /// The body itself could not be evaluated; no compiled check runs.
    Kernel(EvalError),
    Disagreement(String),
}

fn disagreement(what: &str, spec: &str, compiled: &str) -> String {
    format!(
        "DIFFERENTIAL DISAGREEMENT on {what}: kernel-as-spec said [{spec}], \
         compiled said [{compiled}]"
    )
}

/// The governed-history contract over one observation. `Ok(true)` when
/// the probe's baseline acceptance may extend the frontier.
fn governed_contract(obs: &ProbeObservation) -> Result<bool, String> {
    let kernel = obs
        .kernel
        .as_ref()
        .expect("observed probes carry a verdict");
    match (kernel, &obs.stage1, &obs.stage2) {
        (Outcome::Accepted { .. }, None, None) => Ok(true),
        (Outcome::Rejected { reason }, Some(s1), Some(s2)) => {
            let RejectionReason::Invariant {
                name,
                version,
                witness,
            } = reason
            else {
                // finish_staged_delta_with over a staged (not rejected)
                // delta only ever rejects on an invariant; anything
                // else is a comparator bug worth failing loudly.
                return Err(disagreement(
                    "rejection shape",
                    &reason.to_string(),
                    "an invariant violation",
                ));
            };
            for (label, s) in [("full", s1), ("case-bound", s2)] {
                if &s.name != name || s.version != *version {
                    return Err(disagreement(
                        &format!("{label} rule identity"),
                        &format!("{name} v{version}"),
                        &format!("{} v{}", s.name, s.version),
                    ));
                }
                // Witness variables must agree; values may differ.
                let s_vars: Vec<_> = s.witness.iter().map(|w| &w.var).collect();
                let k_vars: Vec<_> = witness.iter().map(|w| &w.var).collect();
                if s_vars != k_vars {
                    return Err(disagreement(
                        &format!("{label} witness variables"),
                        &format!("{k_vars:?}"),
                        &format!("{s_vars:?}"),
                    ));
                }
            }
            Ok(false)
        }
        (kernel, s1, s2) => {
            let mut got = String::new();
            let _ = write!(
                got,
                "full {:?}, case-bound {:?}",
                summarise(s1),
                summarise(s2)
            );
            Err(disagreement(
                "verdict",
                if matches!(kernel, Outcome::Accepted { .. }) {
                    "accepted"
                } else {
                    "rejected"
                },
                &got,
            ))
        }
    }
}

fn summarise(v: &Option<SqlViolation>) -> Option<String> {
    v.as_ref().map(|s| format!("{} v{}", s.name, s.version))
}

/// Sweep one fully compilable programme: build each baseline chain through
/// the real propose path, then probe every transformation's boundary
/// arguments against that state, rolling back each probe. Returns how many
/// probes hit a kernel error that both compiled checks reproduced.
async fn sweep(program: Program) -> usize {
    let validated = program.validated().expect("gallery programme validates");
    let sql_set = compile_invariants(validated).expect("whole-in-fragment programme");
    let boundary_cases: Vec<(
        String,
        Vec<morpholog_test_support::differential::ArgumentCase>,
    )> = program
        .transformations
        .iter()
        .map(|t| {
            (
                t.name.to_string(),
                boundary_argument_cases(&validated, &t.name),
            )
        })
        .collect();
    let compiled = CompiledProgram::new(program).expect("gallery programme compiles");
    let pool = test_pool().await;

    let mut probes = 0usize;
    let mut agreed_errors = 0usize;
    let mut chains: Vec<Vec<(String, Vec<EvalValue>)>> = vec![vec![]];
    for _depth in 0..REACHABILITY_DEPTH {
        let mut next_chains = Vec::new();
        for chain in &chains {
            reset_db(&pool).await;
            for (name, args) in chain {
                let transition = Transition {
                    transformation_name: name.as_str().into(),
                    args: args.clone(),
                    actor: test_actor(),
                };
                let outcome = propose_against_pg(
                    &pool,
                    &PgProgram::new(CompiledProgram::new(compiled.program().clone()).unwrap()),
                    &Proposal::gateway(&transition),
                )
                .await
                .expect("replaying an accepted chain step");
                assert!(
                    matches!(outcome, PgProposalOutcome::Committed { .. }),
                    "a previously accepted chain step must replay accepted"
                );
            }
            for (name, cases) in &boundary_cases {
                for (v, case) in cases.iter().enumerate() {
                    probes += 1;
                    match probe_raw(&pool, &compiled, &sql_set, name, case.args.clone()).await {
                        Ok(Probe::BodyRejected) => {}
                        Ok(Probe::Observed(obs)) => {
                            let baseline_accepted = governed_contract(&obs).unwrap_or_else(|msg| {
                                panic!(
                                    "{}::{name} with {:?}: {msg}",
                                    compiled.program().name,
                                    case.args
                                )
                            });
                            if baseline_accepted && v == 0 {
                                let mut extended = chain.clone();
                                extended.push((name.clone(), case.args.clone()));
                                next_chains.push(extended);
                            }
                        }
                        Ok(Probe::KernelErrorAgreed) => agreed_errors += 1,
                        Err(ProbeFailure::Kernel(e))
                            if case.permits_range_refusal && is_permitted_range_error(&e) =>
                        {
                            // A range error in the body itself, on
                            // range-extreme arguments: no compiled
                            // check runs, so nothing to compare.
                        }
                        Err(ProbeFailure::Disagreement(d)) => panic!(
                            "{}::{name} with {:?}: {d}",
                            compiled.program().name,
                            case.args
                        ),
                        Err(ProbeFailure::Kernel(e)) => panic!(
                            "{}::{name} with {:?} raised a kernel error: {e:?}",
                            compiled.program().name,
                            case.args
                        ),
                        Err(ProbeFailure::Pg(e)) => panic!(
                            "{}::{name} with {:?} failed operationally: {e:?}",
                            compiled.program().name,
                            case.args
                        ),
                    }
                }
            }
        }
        chains = next_chains;
    }
    assert!(
        probes > 0,
        "anti-vacuity: the sweep must have probed something"
    );
    agreed_errors
}

/// Hostile fragments for what the gallery misses: no gallery programme
/// has an ordered comparison in a compilable invariant, so flipping `<=`
/// to `<` would pass the gallery sweep. Each operator gets its own
/// predicate and invariant, bound at ZERO, where every operator pair meets
/// its equality case, and no other invariant can mask its failure. The sum
/// comparison reaches its exact boundary over a two-step chain. Every kind
/// with a jsonb equality representation (Bool, Date, Timestamp, Duration)
/// has its own join fragment, probed on matching and mismatching sides.
/// Probe count is not coverage: every `Ok(Repr)` arm in `repr_for` and
/// every operator needs a fragment here that would catch it.
const HOSTILE: &[&str] = &[
    // Two lines of one figure under a cap and over a floor: on the
    // range-extreme argument the exact total leaves the decimal range,
    // and the kernel's error must be the compiled checks' error too.
    // Under the floor the oversized total compares as holding, so only
    // the range test itself can report it.
    "program two_lines
predicate Cap(b: Subject, cap: Decimal)
predicate Floor(b: Subject, floor: Decimal)
predicate Line(b: Subject, side: Subject, v: Decimal)
invariant capped:
    Cap(b, cap) implies sum(v | Line(b, _, v)) <= cap
invariant floored:
    Floor(b, floor) implies sum(v | Line(b, _, v)) >= floor
transformation set_cap(b, cap):
    admit Cap(b, cap)
transformation set_floor(b, floor):
    admit Floor(b, floor)
transformation add_two(b, v):
    admit Line(b, #left, v)
    admit Line(b, #right, v)
",
    // A bare top-level negation, violated by admitting the second
    // conjunct: the kernel reports no witness for a failure with
    // nothing bound above it, and the compiled check must say the same.
    "program bare_negation
predicate Marked(x: Subject)
predicate Sealed(x: Subject)
invariant never_both:
    not (Marked(x) and Sealed(x))
transformation mark(x):
    admit Marked(x)
transformation seal(x):
    admit Sealed(x)
",
    "program comparison_edges
predicate LeBand(x: Subject, level: Decimal)
predicate LtBand(x: Subject, level: Decimal)
predicate GeBand(x: Subject, level: Decimal)
predicate GtBand(x: Subject, level: Decimal)
invariant le_holds_at_zero:
    LeBand(x, level) implies 0 <= level
invariant lt_excludes_zero:
    LtBand(x, level) implies 0 < level
invariant ge_holds_at_zero:
    GeBand(x, level) implies level >= 0
invariant gt_excludes_zero:
    GtBand(x, level) implies level > 0
transformation hold_le(x, level):
    admit LeBand(x, level)
transformation hold_lt(x, level):
    admit LtBand(x, level)
transformation hold_ge(x, level):
    admit GeBand(x, level)
transformation hold_gt(x, level):
    admit GtBand(x, level)
",
    "program summed_cap
predicate PotCap(cap: Decimal)
predicate Pot(p: Subject, amount: Decimal)
invariant pots_within_cap:
    PotCap(cap) implies sum(a | Pot(_, a)) <= cap
transformation set_cap(cap):
    require not PotCap(_)
    admit PotCap(cap)
transformation add_pot(p, amount):
    admit Pot(p, amount)
",
    "program tagged_date_join
predicate Opened(x: Subject, on: Date)
predicate Closed(x: Subject, on: Date)
invariant closed_on_the_open_date:
    Closed(x, d) implies Opened(x, d)
transformation open(x, on):
    admit Opened(x, on)
transformation close(x, on):
    admit Closed(x, on)
",
    "program tagged_bool_join
predicate LeftFlag(x: Subject, v: Bool)
predicate RightFlag(x: Subject, v: Bool)
invariant flags_agree:
    LeftFlag(x, v) implies RightFlag(x, v)
transformation set_right_flag(x, v):
    admit RightFlag(x, v)
transformation set_left_flag(x, v):
    admit LeftFlag(x, v)
",
    "program tagged_timestamp_join
predicate LeftAt(x: Subject, v: Timestamp)
predicate RightAt(x: Subject, v: Timestamp)
invariant instants_agree:
    LeftAt(x, v) implies RightAt(x, v)
transformation set_right_at(x, v):
    admit RightAt(x, v)
transformation set_left_at(x, v):
    admit LeftAt(x, v)
",
    "program tagged_duration_join
predicate LeftSpan(x: Subject, v: Duration)
predicate RightSpan(x: Subject, v: Duration)
invariant spans_agree:
    LeftSpan(x, v) implies RightSpan(x, v)
transformation set_right_span(x, v):
    admit RightSpan(x, v)
transformation set_left_span(x, v):
    admit LeftSpan(x, v)
",
];

#[tokio::test]
async fn every_hostile_fragment_agrees_with_the_kernel() {
    let mut agreed_errors = 0usize;
    for source in HOSTILE {
        let program = morpholog_surface::parse_program(source).expect("hostile fragment parses");
        agreed_errors += sweep(program).await;
    }
    // The range parity is only proven if some probe reached a kernel
    // range error and both stages reproduced it; `two_lines` puts one
    // at depth one on the range-extreme argument.
    assert!(
        agreed_errors > 0,
        "no probe reached a kernel range error; the sum-range parity went unexercised"
    );
}

/// Gallery programmes that must stay fully compilable, so the sweep never
/// silently goes empty. Others join automatically via
/// `every_whole_in_fragment_programme_is_swept`.
const MINIMUM_CORPUS: &[&str] = &[
    "settlement_netting",
    "verified_revenue",
    "double_entry_ledger",
    "approval_controls",
    "carbon_credit_provenance",
    "release_governance",
];

pub(crate) fn whole_in_fragment() -> Vec<Program> {
    morpholog_examples::all_programs()
        .into_iter()
        .filter(|p| {
            p.validated()
                .ok()
                .is_some_and(|v| compile_invariants(v).is_ok())
        })
        .collect()
}

#[tokio::test]
async fn the_minimum_corpus_is_still_whole_in_fragment() {
    let qualifying: Vec<String> = whole_in_fragment().iter().map(|p| p.name.clone()).collect();
    for name in MINIMUM_CORPUS {
        assert!(
            qualifying.iter().any(|q| q == name),
            "`{name}` fell out of the compiled fragment; the differential floor eroded \
             (qualifying: {qualifying:?})"
        );
    }
}

#[tokio::test]
async fn every_whole_in_fragment_programme_agrees_with_the_kernel() {
    for program in whole_in_fragment() {
        let _ = sweep(program).await;
    }
}

// ============================================================
// Dirty history: the one-directional contract
// ============================================================

/// The overflow that compares as holding: under a floor of zero, two
/// lines of the largest decimal total past the range while `>= 0` is
/// true of the oversized number. The kernel errors; only the range
/// test itself can make the compiled check say the same.
#[tokio::test]
async fn an_overflow_that_compares_as_holding_is_the_kernels_error_on_both_stages() {
    let program = morpholog_surface::parse_program(HOSTILE[0]).expect("two_lines parses");
    assert_eq!(program.name, "two_lines");
    let validated = program.validated().expect("validates");
    let sql_set = compile_invariants(validated).expect("two_lines is whole-in-fragment");
    let compiled = CompiledProgram::new(program.clone()).expect("compiles");
    let pool = test_pool().await;
    reset_db(&pool).await;
    let floor = Transition {
        transformation_name: "set_floor".into(),
        args: vec![subj("x"), dec(0)],
        actor: test_actor(),
    };
    let outcome = propose_against_pg(
        &pool,
        &PgProgram::new(CompiledProgram::new(program).expect("compiles")),
        &Proposal::gateway(&floor),
    )
    .await
    .expect("sets the floor");
    assert!(matches!(outcome, PgProposalOutcome::Committed { .. }));
    let largest = morpholog_test_support::dec_str("79228162514264337593543950335");
    let probe = probe_raw(
        &pool,
        &compiled,
        &sql_set,
        "add_two",
        vec![subj("x"), largest],
    )
    .await;
    match probe {
        Ok(Probe::KernelErrorAgreed) => {}
        Ok(Probe::BodyRejected) => panic!("the body admits"),
        Ok(Probe::Observed(obs)) => panic!("the kernel must error, got {:?}", obs.kernel),
        Err(ProbeFailure::Disagreement(d)) => panic!("{d}"),
        Err(ProbeFailure::Kernel(e)) => panic!("body error {e:?}"),
        Err(ProbeFailure::Pg(e)) => panic!("pg error {e:?}"),
    }
}

/// Attacker capability modelled: none. The dirty row stands in for history
/// admitted under an older programme or rule version, which commit-time
/// checking must tolerate.
#[tokio::test]
async fn dirty_history_blocks_only_the_writes_that_touch_it() {
    let program = morpholog_examples::double_entry_ledger::program();
    let validated = program.validated().expect("ledger validates");
    let sql_set = compile_invariants(validated).expect("ledger is whole-in-fragment");
    let compiled = CompiledProgram::new(program).expect("ledger compiles");
    let pool = test_pool().await;
    reset_db(&pool).await;

    // One unbalanced legacy entry, bypassing the kernel.
    for (pred, args) in [
        (
            "JournalEntry",
            serde_json::json!([{"type":"subject","value":"e_dirty"}, {"type":"subject","value":"d0"}, {"type":"subject","value":"p0"}]),
        ),
        (
            "JournalLine",
            serde_json::json!([{"type":"subject","value":"e_dirty"}, {"type":"subject","value":"cash"}, {"type":"decimal","value":"100"}, {"type":"decimal","value":"0"}]),
        ),
    ] {
        sqlx::query("INSERT INTO morpholog.claims (predicate_name, arguments, asserted_in) VALUES ($1, $2, $3)")
            .bind(pred)
            .bind(args)
            .bind(Uuid::nil())
            .execute(&pool)
            .await
            .expect("dirty fixture insert");
    }

    let balanced: Vec<EvalValue> = vec![
        subj("e_new"),
        subj("d1"),
        subj("p1"),
        subj("cash"),
        subj("rev"),
        dec(100),
    ];
    let obs = match probe_raw(&pool, &compiled, &sql_set, "post_simple_entry", balanced).await {
        Ok(Probe::Observed(obs)) => obs,
        other => panic!(
            "expected an observed probe, got {:?}",
            match other {
                Ok(Probe::BodyRejected) => "body rejection".to_string(),
                Ok(Probe::KernelErrorAgreed) => "an agreed kernel error".to_string(),
                Err(ProbeFailure::Kernel(e)) => format!("kernel error {e:?}"),
                Err(ProbeFailure::Pg(e)) => format!("pg error {e:?}"),
                Err(ProbeFailure::Disagreement(d)) => d,
                Ok(Probe::Observed(_)) => unreachable!(),
            }
        ),
    };

    // The kernel and the case-bound check both admit a write that leaves
    // the inherited bad row alone. The whole-state check refuses it: the
    // one place the three may lawfully differ.
    assert!(
        matches!(obs.kernel, Some(Outcome::Accepted { .. })),
        "the kernel admits the non-worsening write; got {:?}",
        obs.kernel
    );
    assert!(
        obs.stage2.is_none(),
        "the case-bound check admits the non-worsening write; got {:?}",
        summarise(&obs.stage2)
    );
    assert_eq!(
        summarise(&obs.stage1).as_deref(),
        Some("balanced_posted_entry v1"),
        "the whole-state check refuses on the inherited entry"
    );

    // A WORSENING write still refuses everywhere: the divergence never
    // runs the other way.
    let unbalanced: Vec<EvalValue> = vec![
        subj("e_worse"),
        subj("d1"),
        subj("p1"),
        subj("cash"),
        dec(100),
        subj("pay"),
        dec(60),
        subj("tax"),
        dec(30),
    ];
    let Ok(Probe::Observed(obs)) =
        probe_raw(&pool, &compiled, &sql_set, "post_split_entry", unbalanced).await
    else {
        panic!("expected an observed probe for the worsening write")
    };
    assert_stage1_keeps_kernel_identity(&obs);
    governed_contract(&obs).expect("the worsening write refuses everywhere, with one identity");
}

/// On any history, the full check keeps the kernel's rejection identity:
/// rule name, version, and witness variable set. Returns the rule name
/// for the caller's own pin.
fn assert_stage1_keeps_kernel_identity(obs: &ProbeObservation) -> String {
    let Some(Outcome::Rejected {
        reason:
            RejectionReason::Invariant {
                name,
                version,
                witness,
            },
    }) = &obs.kernel
    else {
        panic!("the kernel must refuse here");
    };
    let s1 = obs
        .stage1
        .as_ref()
        .expect("the full check must refuse alongside the kernel");
    assert_eq!(&s1.name, name, "stage 1 keeps the kernel's rule name");
    assert_eq!(
        s1.version, *version,
        "stage 1 keeps the kernel's rule version"
    );
    let s1_vars: Vec<_> = s1.witness.iter().map(|w| &w.var).collect();
    let k_vars: Vec<_> = witness.iter().map(|w| &w.var).collect();
    assert_eq!(
        s1_vars, k_vars,
        "stage 1 keeps the kernel's witness variables"
    );
    name.to_string()
}

/// Reports how much of the gallery compiles, without pinning a count. What
/// is pinned: every refusal names a real invariant of its programme.
#[test]
fn compile_coverage_census_attributes_every_refusal() {
    let mut compiled_count = 0usize;
    let mut refused_count = 0usize;
    for program in morpholog_examples::all_programs() {
        let validated = program.validated().expect("gallery programme validates");
        match compile_invariants(validated) {
            Ok(set) => compiled_count += set.invariants.len(),
            Err(refusals) => {
                for refusal in &refusals {
                    assert!(
                        program
                            .invariants
                            .iter()
                            .any(|i| i.name == refusal.invariant),
                        "refusal names `{}`, which `{}` does not declare",
                        refusal.invariant,
                        program.name
                    );
                }
                compiled_count += program.invariants.len() - refusals.len();
                refused_count += refusals.len();
            }
        }
    }
    assert!(
        compiled_count > 0 && refused_count > 0,
        "anti-vacuity: the gallery exercises both sides of the fragment \
         (compiled {compiled_count}, refused {refused_count})"
    );
}
