//! The same-candidate differential: the kernel and the compiled SQL
//! checker judge the exact same staged delta, inside one SERIALIZABLE
//! transaction, and must agree - the permanent gate on the compiled
//! path's correctness claim. `DATABASE_URL`-gated like every PG suite.
//!
//! Stronger than the spike's differential in two ways. The body is
//! staged ONCE: the kernel's verdict comes from `finish_staged_delta`
//! over the same `StagedDelta` whose claims are written into the
//! transaction, so a body minting `new Subject()` can no longer make
//! the two evaluators see different candidates. And every probe is
//! observationally inert: BEGIN through today's authorised seam, write
//! the delta, interrogate, ROLLBACK - no audit, no outbox, no commit -
//! so a frontier state is built once per chain and probed many times.
//!
//! Two contracts, named:
//!
//! - **Governed history** (states reached only through accepted
//!   current-programme proposals): kernel, full (stage-1) SQL, and
//!   case-bound (stage-2) SQL agree on the verdict; on rejection, the
//!   first failing rule's name, version, and witness VARIABLE SET are
//!   strict; witness values are observational (a symmetric self-join
//!   names the violating pair in a different order).
//! - **Dirty history** (rows the kernel never admitted): full SQL
//!   remains verdict- and identity-equivalent to the kernel; the
//!   case-bound check may ACCEPT where they refuse - never the
//!   reverse - and when both reject, matching rule identity is not
//!   required (the full check can trip an earlier pre-existing
//!   violation the case-bound check lawfully skips).
//!
//! Probes whose argument vector carries the range-extreme witness may
//! raise the kernel's named out-of-range refusals; those probes are
//! skipped, not compared - PG numeric is wider than the kernel's
//! decimal, the recorded `ArithOutOfRange` parity gap.

use std::fmt::Write as _;

use morpholog_core::{
    CompiledProgram, EvalError, EvalValue, Outcome, Program, RejectionReason, StagedDelta, Subject,
    Transition, WitnessBinding, finish_staged_delta, propose_stage_delta,
};
use sqlx::Row;
use uuid::Uuid;

use crate::attestation::Proposal;
use crate::compiled::{CaseFilter, CompiledInvariant, CompiledInvariantSet, compile_invariants};
use crate::error::{PgError, classify};
use crate::propose::{compute_load_scope, load_state, write_claim_delta};
use crate::txn::begin_authorised_proposal_tx;
use crate::{PgPool, PgProposalOutcome, propose_against_pg};

use morpholog_test_support::differential::{boundary_argument_cases, is_permitted_range_error};
use morpholog_test_support::{dec, subj, test_actor};

/// One accepted step from empty, then every transformation again: the
/// depth that reaches first-commission invariant evaluation. The
/// rollback-only probe structure keeps the full declared frontier
/// cheap enough to always be the gate - no reduced CI depth.
const REACHABILITY_DEPTH: usize = 2;

async fn test_pool() -> PgPool {
    let url = std::env::var("DATABASE_URL").expect(
        "DATABASE_URL must be set for the compiled differential \
         (e.g. postgres:///morpholog_dev)",
    );
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

/// A rule identity plus decoded witness, as one SQL stage reports it.
type SqlViolation = (morpholog_core::InvariantName, u32, Vec<WitnessBinding>);

/// What one probe observed from all three evaluators over the same
/// staged candidate. `kernel` is `None` when the body itself rejected
/// (only one evaluator runs the body, so there is nothing to compare).
struct ProbeObservation {
    kernel: Option<Outcome>,
    stage1: Option<SqlViolation>,
    stage2: Option<SqlViolation>,
}

enum Probe {
    BodyRejected,
    Observed(Box<ProbeObservation>),
}

/// Stage once, judge three times, roll back. The comparator core both
/// contracts share; the contract itself is applied by the caller.
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
    let scope = compute_load_scope(transformation, invariants, definitions);
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
    // table is about to receive.
    let kernel = finish_staged_delta(staged, &state, invariants, definitions)
        .map_err(ProbeFailure::Kernel)?;

    let transition_id = Uuid::now_v7();
    write_claim_delta(&mut tx, transition_id, &asserted, &retracted)
        .await
        .map_err(ProbeFailure::Pg)?;

    // Correlated-subquery estimates inflate planned cost and trip the
    // JIT threshold (~118ms of compilation for a sub-ms plan, measured
    // at N=100k in the spike). JIT is for analytics; off for this tx.
    sqlx::raw_sql("SET LOCAL jit = off")
        .execute(&mut *tx)
        .await
        .map_err(|e| ProbeFailure::Pg(classify(e)))?;

    let stage1 = first_violation(&mut tx, sql_set, Stage::Full, &asserted, &retracted)
        .await
        .map_err(ProbeFailure::Pg)?;
    let stage2 = first_violation(&mut tx, sql_set, Stage::CaseBound, &asserted, &retracted)
        .await
        .map_err(ProbeFailure::Pg)?;

    // Observationally inert: every probe rolls back, whatever it saw.
    tx.rollback()
        .await
        .map_err(|e| ProbeFailure::Pg(classify(e)))?;

    Ok(Probe::Observed(Box::new(ProbeObservation {
        kernel: Some(kernel),
        stage1,
        stage2,
    })))
}

enum ProbeFailure {
    Pg(PgError),
    Kernel(EvalError),
}

#[derive(Clone, Copy)]
enum Stage {
    Full,
    CaseBound,
}

/// First violating invariant in programme order at the given stage,
/// with its decoded witness - the compiled analogue of the kernel's
/// first-failure loop.
async fn first_violation(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    sql_set: &CompiledInvariantSet,
    stage: Stage,
    asserted: &[morpholog_core::ClaimInstance],
    retracted: &[morpholog_core::ClaimInstance],
) -> Result<Option<SqlViolation>, PgError> {
    for inv in &sql_set.invariants {
        let sql = match stage {
            Stage::Full => inv.violation_sql(None),
            Stage::CaseBound => match inv.case_filter(asserted, retracted) {
                CaseFilter::Untouched => continue,
                CaseFilter::Bounded(filter) => inv.violation_sql(Some(&filter)),
                CaseFilter::Unbounded => inv.violation_sql(None),
            },
        };
        // Audited for AssertSqlSafe: the SQL is rendered entirely by
        // `compiled.rs` from a validated programme - identifiers are
        // quoted, literals escaped, and the provenance comment
        // neutralised there.
        let row = sqlx::query(sqlx::AssertSqlSafe(sql))
            .fetch_optional(&mut **tx)
            .await
            .map_err(classify)?;
        if let Some(row) = row {
            let witness = decode_witness(inv, &row)?;
            return Ok(Some((inv.name.clone(), inv.version, witness)));
        }
    }
    Ok(None)
}

/// Decode a violation row's witness columns: each is the `::text` of
/// the full tagged value, so `EvalValue`'s own serde is the decoder -
/// the one wire contract, no per-kind column logic.
fn decode_witness(
    inv: &CompiledInvariant,
    row: &sqlx::postgres::PgRow,
) -> Result<Vec<WitnessBinding>, PgError> {
    let mut witness = Vec::with_capacity(inv.witness_vars.len());
    for var in &inv.witness_vars {
        let col = format!("w_{var}");
        let text: String = row
            .try_get(col.as_str())
            .map_err(|e| PgError::InvalidState(format!("witness column {col} missing: {e}")))?;
        let value: EvalValue = serde_json::from_str(&text)
            .map_err(|e| PgError::InvalidState(format!("witness value {col} undecodable: {e}")))?;
        witness.push(WitnessBinding {
            var: var.clone(),
            value,
        });
    }
    Ok(witness)
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
                // finish_staged_delta over a staged (not rejected)
                // delta only ever rejects on an invariant; anything
                // else is a comparator bug worth failing loudly.
                return Err(disagreement(
                    "rejection shape",
                    &reason.to_string(),
                    "an invariant violation",
                ));
            };
            for (label, s) in [("full", s1), ("case-bound", s2)] {
                if &s.0 != name || s.1 != *version {
                    return Err(disagreement(
                        &format!("{label} rule identity"),
                        &format!("{name} v{version}"),
                        &format!("{} v{}", s.0, s.1),
                    ));
                }
                // Witness VARS must agree; values are observational -
                // the adopted witness contract.
                let s_vars: Vec<_> = s.2.iter().map(|w| &w.var).collect();
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
    v.as_ref().map(|(n, ver, _)| format!("{n} v{ver}"))
}

/// Sweep one whole-in-fragment programme: reset and replay each
/// accepted baseline chain once through the REAL production propose
/// path, then run every transformation's boundary argument cases as
/// rollback-only probes against that frontier state.
async fn sweep(program: Program) {
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
                let outcome = propose_against_pg(&pool, &compiled, &Proposal::gateway(&transition))
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
                        Err(ProbeFailure::Kernel(e))
                            if case.permits_range_refusal && is_permitted_range_error(&e) =>
                        {
                            // The recorded ArithOutOfRange parity gap:
                            // PG numeric is wider, so range-extreme
                            // probes are skipped, never compared.
                        }
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
}

/// Hostile fragments: the gallery supplies breadth, these supply
/// spite. The break-check that forced them: flipping the compiled
/// `<=` to `<` survived the whole gallery sweep, because NO gallery
/// programme in the fragment carries an ordered comparison in an
/// invariant. Each operator gets its own predicate and its own
/// invariant with its bound at ZERO - a generated boundary witness,
/// and (negative literals not being surface-spellable) the one place
/// every operator pair meets its equality case. First-failure
/// discriminator no other invariant can mask; the sum comparison
/// rides a two-step chain to its exact boundary; and every kind the
/// compiler accepts a jsonb equality representation for (Bool, Date,
/// Timestamp, Duration) carries its own join fragment, probed on both
/// the matching and mismatching side. The rule the incident taught:
/// probe count is not semantic coverage - every Ok(Repr) arm in
/// `repr_for`, like every operator, needs a forcing discriminator
/// here, not merely a unit test asserting emitted text.
const HOSTILE: &[&str] = &[
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
    for source in HOSTILE {
        let program = morpholog_surface::parse_program(source).expect("hostile fragment parses");
        sweep(program).await;
    }
}

/// The named minimum corpus: gallery programmes that must stay
/// whole-in-fragment, so the sweep can never silently go vacuous.
/// Additional qualifiers join the sweep automatically via
/// `every_whole_in_fragment_programme_is_swept`; this list only stops
/// the floor from eroding.
const MINIMUM_CORPUS: &[&str] = &[
    "settlement_netting",
    "verified_revenue",
    "double_entry_ledger",
    "approval_controls",
    "carbon_credit_provenance",
];

fn whole_in_fragment() -> Vec<Program> {
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
        sweep(program).await;
    }
}

// ============================================================
// Dirty history: the one-directional contract
// ============================================================

/// Attacker capability modelled: none - the dirty row stands in for
/// history admitted under an older programme or a since-superseded
/// rule version, which commit-time checking must tolerate.
#[tokio::test]
async fn dirty_history_diverges_only_in_the_pinned_direction() {
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
                Err(ProbeFailure::Kernel(e)) => format!("kernel error {e:?}"),
                Err(ProbeFailure::Pg(e)) => format!("pg error {e:?}"),
                Ok(Probe::Observed(_)) => unreachable!(),
            }
        ),
    };

    // Full SQL stays verdict- and identity-equivalent to the kernel:
    // name, version, and witness variable set - the same strength the
    // governed contract demands, because stage 1 is the
    // semantics-equivalent compiler on EVERY history.
    let kernel_rule = assert_stage1_keeps_kernel_identity(&obs);
    assert_eq!(kernel_rule, "balanced_posted_entry");

    // The case-bound check lawfully ACCEPTS the non-worsening write -
    // the deliberate, pinned divergence direction.
    assert!(
        obs.stage2.is_none(),
        "the case-bound check must admit the non-worsening write; got {:?}",
        summarise(&obs.stage2)
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
    assert!(
        obs.stage2.is_some(),
        "the case-bound check refuses the worsening write - divergence is one-directional"
    );
}

/// On any history, stage 1 keeps the kernel's full rejection identity:
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
    let (s1_name, s1_version, s1_witness) = obs
        .stage1
        .as_ref()
        .expect("the full check must refuse alongside the kernel");
    assert_eq!(s1_name, name, "stage 1 keeps the kernel's rule name");
    assert_eq!(
        s1_version, version,
        "stage 1 keeps the kernel's rule version"
    );
    let s1_vars: Vec<_> = s1_witness.iter().map(|w| &w.var).collect();
    let k_vars: Vec<_> = witness.iter().map(|w| &w.var).collect();
    assert_eq!(
        s1_vars, k_vars,
        "stage 1 keeps the kernel's witness variables"
    );
    name.to_string()
}

/// The compile-coverage census: reported, never pinned to a count
/// (counts change as the gallery grows); what is pinned is that every
/// refusal names a real invariant of its programme - attribution, not
/// arithmetic.
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
