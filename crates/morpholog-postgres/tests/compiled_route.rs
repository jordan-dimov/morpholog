//! The compiled route beside the interpreter, on the production path.
//! Every whole-in-fragment gallery programme is proposed twice from the
//! same seeded state, once through each route, and the two must reach
//! the same decision: the same outcome and reason, the same refusing
//! rule and version, the same witness variables, the same semantic
//! rejection-log fields, and a byte-equal `invariants_checked` audit
//! field. Witness values are observational (a symmetric plan may name
//! the violating pair in another order), and generated identities and
//! times are free.
//!
//! The other half is what a compiled check may never do: decide by
//! falling back. A SQL error inside a check is an operational error,
//! with the transaction rolled back and nothing recorded.

#![allow(clippy::unwrap_used, clippy::expect_used)]

mod common;

use common::{attested, reset_db, test_pool};
use morpholog_core::{
    ClaimInstance, CompiledProgram, EvalError, EvalValue, Program, Subject, Transition,
};
use morpholog_examples::double_entry_ledger;
use morpholog_postgres::{
    InvariantPlan, PgAtomicOutcome, PgError, PgPool, PgProgram, PgProposalOutcome, Proposal,
    list_rejection_rows, propose_against_pg, propose_all_against_pg,
};
use morpholog_test_support::differential::{normalize_uuids, sample_args, sample_state};
use morpholog_test_support::{dec, subj};

async fn seed(pool: &PgPool, claims: &[ClaimInstance]) {
    for claim in claims {
        let args_json = serde_json::to_value(&claim.args).unwrap();
        sqlx::query(
            "INSERT INTO morpholog.claims (predicate_name, arguments, asserted_in)
             VALUES ($1, $2, $3)",
        )
        .bind(claim.predicate.as_str())
        .bind(&args_json)
        .bind(uuid::Uuid::nil())
        .execute(pool)
        .await
        .unwrap();
    }
}

async fn count(pool: &PgPool, sql: &'static str) -> i64 {
    sqlx::query_scalar(sql).fetch_one(pool).await.unwrap()
}

async fn audit_invariants_checked(pool: &PgPool) -> Vec<serde_json::Value> {
    sqlx::query_scalar("SELECT invariants_checked FROM morpholog.audit ORDER BY transition_id")
        .fetch_all(pool)
        .await
        .unwrap()
}

fn eligible_gallery() -> Vec<Program> {
    morpholog_examples::all_programs()
        .into_iter()
        .filter(|p| {
            let program = PgProgram::new(CompiledProgram::new(p.clone()).unwrap());
            matches!(program.plan(), InvariantPlan::Compiled { .. })
        })
        .collect()
}

/// What one proposal left behind that both routes must agree on.
#[derive(Debug, PartialEq, Eq)]
struct Observed {
    outcome: String,
    rejection: Option<String>,
    audit: Vec<serde_json::Value>,
    claims: i64,
    outbox: i64,
}

/// A route's answer, typed: a decision with what it left behind, the
/// kernel's own evaluation error, or an operational failure.
#[derive(Debug, PartialEq, Eq)]
enum RouteObservation {
    Decided(Observed),
    Kernel(EvalError),
    Operational(String),
}

async fn observe(
    pool: &PgPool,
    program: &PgProgram,
    seeded: &[ClaimInstance],
    transition: &Transition,
) -> RouteObservation {
    reset_db(pool).await;
    seed(pool, seeded).await;
    let outcome = match propose_against_pg(pool, program, &attested(transition)).await {
        Ok(outcome) => outcome,
        Err(PgError::Kernel(e)) => return RouteObservation::Kernel(e),
        Err(e) => return RouteObservation::Operational(format!("{e:?}")),
    };
    let outcome = match outcome {
        PgProposalOutcome::Committed {
            asserted_claims,
            retracted_claims,
            emitted_intents,
            ..
        } => normalize_uuids(&format!(
            "committed +{asserted_claims:?} -{retracted_claims:?} !{emitted_intents:?}"
        )),
        PgProposalOutcome::Rejected {
            reason,
            rule,
            witness,
        } => {
            let vars: Vec<_> = witness.iter().map(|w| w.var.to_string()).collect();
            format!("rejected {reason} | rule {rule:?} | witness vars {vars:?}")
        }
    };
    let rejection = list_rejection_rows(pool, 10)
        .await
        .unwrap()
        .into_iter()
        .map(|r| {
            let vars: Vec<_> = r
                .witness
                .iter()
                .flatten()
                .map(|w| w.var.to_string())
                .collect();
            normalize_uuids(&format!(
                "{} {:?} {:?} {} {} {:?} {} {vars:?}",
                r.transformation_name,
                r.arguments,
                r.actor,
                r.kind,
                r.rule,
                r.invariant_version,
                r.reason
            ))
        })
        .reduce(|a, b| format!("{a}\n{b}"));
    RouteObservation::Decided(Observed {
        outcome,
        rejection,
        audit: audit_invariants_checked(pool).await,
        claims: count(pool, "SELECT count(*) FROM morpholog.claims").await,
        outbox: count(pool, "SELECT count(*) FROM morpholog.outbox").await,
    })
}

#[tokio::test]
async fn both_routes_reach_the_same_decision_over_the_gallery() {
    let pool = test_pool().await;
    let (mut cases, mut commits, mut refusals, mut errors, mut skipped) =
        (0usize, 0usize, 0usize, 0usize, 0usize);
    for program in eligible_gallery() {
        let compiled = PgProgram::new(CompiledProgram::new(program.clone()).unwrap());
        let interpreted = PgProgram::interpreted(CompiledProgram::new(program.clone()).unwrap());
        for t in &program.transformations {
            for salt in 0..2u64 {
                let Some(args) = sample_args(&program, t, salt) else {
                    skipped += 1;
                    continue;
                };
                let seeded: Vec<ClaimInstance> = sample_state(&program, 2, salt)
                    .claims()
                    .iter()
                    .cloned()
                    .collect();
                let transition = Transition {
                    transformation_name: t.name.clone(),
                    args,
                    actor: Subject::from("route_test"),
                };
                let spec = observe(&pool, &interpreted, &seeded, &transition).await;
                let real = observe(&pool, &compiled, &seeded, &transition).await;
                assert_eq!(
                    real, spec,
                    "programme `{}`, transformation `{}`, salt {salt}",
                    program.name, t.name
                );
                match &spec {
                    RouteObservation::Decided(o) if o.outcome.starts_with("committed") => {
                        commits += 1;
                    }
                    RouteObservation::Decided(_) => refusals += 1,
                    RouteObservation::Kernel(_) | RouteObservation::Operational(_) => errors += 1,
                }
                cases += 1;
            }
        }
    }
    assert!(
        cases >= 30,
        "generator collapse: only {cases} cases ran ({skipped} skipped)"
    );
    assert!(
        commits > 0 && refusals > 0,
        "both decisions must occur for the comparison to mean anything: \
         {commits} commits, {refusals} refusals, {errors} errors"
    );
}

fn max() -> EvalValue {
    EvalValue::Decimal(rust_decimal::Decimal::MAX)
}

fn line(entry: &str, account: &str, debit: EvalValue, credit: EvalValue) -> ClaimInstance {
    ClaimInstance {
        predicate: "JournalLine".into(),
        args: vec![subj(entry), subj(account), debit, credit],
    }
}

fn entry(entry: &str) -> ClaimInstance {
    ClaimInstance {
        predicate: "JournalEntry".into(),
        args: vec![subj(entry), subj("d_2026_05_17"), subj("p_2026_05")],
    }
}

/// Both routes, from the same seeded ledger, on a fresh balanced posting.
async fn both_routes(pool: &PgPool, seeded: &[ClaimInstance]) -> (RouteObservation, RouteObservation) {
    let transition = Transition {
        transformation_name: "post_simple_entry".into(),
        args: vec![
            subj("fresh"),
            subj("d_2026_05_17"),
            subj("p_2026_05"),
            subj("account_cash"),
            subj("account_revenue"),
            dec(10),
        ],
        actor: Subject::from("route_test"),
    };
    let interpreted =
        PgProgram::interpreted(CompiledProgram::new(double_entry_ledger::program()).unwrap());
    let spec = observe(pool, &interpreted, seeded, &transition).await;
    let real = observe(pool, &ledger(), seeded, &transition).await;
    (spec, real)
}

/// One entry breaks the balance, another's total is more than any
/// decimal holds. The violation sorts first in witness order; the
/// range error must still be the answer on both routes, with nothing
/// recorded anywhere.
#[tokio::test]
async fn a_range_error_dominates_a_violation_that_sorts_earlier_on_both_routes() {
    let pool = test_pool().await;
    let seeded = vec![
        entry("e0"),
        line("e0", "account_cash", dec(5), dec(0)),
        entry("e1"),
        line("e1", "account_cash", max(), dec(0)),
        line("e1", "account_other", dec(1), dec(0)),
        line("e1", "account_revenue", dec(0), max()),
        line("e1", "account_revenue", dec(0), dec(1)),
    ];
    let (spec, real) = both_routes(&pool, &seeded).await;
    assert_eq!(
        spec,
        RouteObservation::Kernel(EvalError::sum_out_of_decimal_range())
    );
    assert_eq!(real, spec);
    assert_eq!(count(&pool, "SELECT count(*) FROM morpholog.audit").await, 0);
    assert_eq!(count(&pool, "SELECT count(*) FROM morpholog.rejections").await, 0);
    assert_eq!(count(&pool, "SELECT count(*) FROM morpholog.outbox").await, 0);
    assert_eq!(
        count(&pool, "SELECT count(*) FROM morpholog.claims").await,
        seeded.len() as i64,
        "the fresh posting's delta rolled back"
    );
}

/// The kernel accumulates wider than a decimal and tests only the
/// final total, so an excess that cancels is no error: the compiled
/// check must agree, and admit.
#[tokio::test]
async fn an_excess_that_cancels_is_representable_on_both_routes() {
    let pool = test_pool().await;
    let seeded = vec![
        entry("e1"),
        line("e1", "account_cash", max(), dec(0)),
        line("e1", "account_cash", dec(1), dec(0)),
        line("e1", "account_cash", dec(-1), dec(0)),
        line("e1", "account_revenue", dec(0), max()),
        line("e1", "account_revenue", dec(0), dec(1)),
        line("e1", "account_revenue", dec(0), dec(-1)),
    ];
    let (spec, real) = both_routes(&pool, &seeded).await;
    assert!(
        matches!(&spec, RouteObservation::Decided(o) if o.outcome.starts_with("committed")),
        "{spec:?}"
    );
    assert_eq!(real, spec);
}

fn ledger() -> PgProgram {
    PgProgram::new(CompiledProgram::new(double_entry_ledger::program()).unwrap())
}

/// One balanced simple entry.
fn posting(entry: &str, amount: i64) -> Proposal {
    Proposal::gateway(&Transition {
        transformation_name: "post_simple_entry".into(),
        args: vec![
            subj(entry),
            subj("d_2026_05_17"),
            subj("p_2026_05"),
            subj("account_cash"),
            subj("account_revenue"),
            dec(amount),
        ],
        actor: Subject::from("route_test"),
    })
}

/// One debit against two credits; balanced only when they sum to it.
fn split_posting(entry: &str, debit: i64, credit_a: i64, credit_b: i64) -> Proposal {
    Proposal::gateway(&Transition {
        transformation_name: "post_split_entry".into(),
        args: vec![
            subj(entry),
            subj("d_2026_05_17"),
            subj("p_2026_05"),
            subj("account_cash"),
            dec(debit),
            subj("account_revenue"),
            dec(credit_a),
            subj("account_other"),
            dec(credit_b),
        ],
        actor: Subject::from("route_test"),
    })
}

#[tokio::test]
async fn the_ledger_takes_the_compiled_route() {
    assert!(matches!(ledger().plan(), InvariantPlan::Compiled { .. }));
}

#[tokio::test]
async fn a_failing_compiled_check_is_an_operational_error_never_a_decision() {
    let pool = test_pool().await;
    reset_db(&pool).await;
    let program = ledger();
    let first = propose_against_pg(&pool, &program, &posting("e1", 100))
        .await
        .unwrap();
    assert!(matches!(first, PgProposalOutcome::Committed { .. }));

    // A line whose amount is not a number. Nothing the codec would
    // write; a corrupt row is the one way to make a correct query fail.
    let corrupted = sqlx::query(
        "UPDATE morpholog.claims
            SET arguments = jsonb_set(arguments, '{2,value}', '\"abc\"')
          WHERE predicate_name = 'JournalLine'
            AND arguments -> 1 ->> 'value' = 'account_cash'",
    )
    .execute(&pool)
    .await
    .unwrap()
    .rows_affected();
    assert_eq!(corrupted, 1);
    let audit_before = count(&pool, "SELECT count(*) FROM morpholog.audit").await;
    let claims_before = count(&pool, "SELECT count(*) FROM morpholog.claims").await;

    let second = propose_against_pg(&pool, &program, &posting("e2", 50)).await;
    assert!(
        matches!(second, Err(PgError::Database(_))),
        "a check that cannot run is an error, got {second:?}"
    );
    assert_eq!(
        count(&pool, "SELECT count(*) FROM morpholog.audit").await,
        audit_before,
        "nothing recorded"
    );
    assert_eq!(
        count(&pool, "SELECT count(*) FROM morpholog.claims").await,
        claims_before,
        "the written delta rolled back"
    );
    assert!(
        list_rejection_rows(&pool, 10).await.unwrap().is_empty(),
        "an error is not a refusal"
    );
}

#[tokio::test]
async fn a_compiled_batch_checks_each_act_against_the_acts_before_it() {
    let pool = test_pool().await;
    reset_db(&pool).await;
    let program = ledger();

    // Two postings admitted as one decision.
    let outcome = propose_all_against_pg(&pool, &program, &[posting("e1", 100), posting("e2", 40)])
        .await
        .unwrap();
    let PgAtomicOutcome::Committed { acts } = outcome else {
        panic!("both admitted, got {outcome:?}");
    };
    assert_eq!(acts.len(), 2);
    assert_eq!(
        count(&pool, "SELECT count(*) FROM morpholog.audit").await,
        2
    );

    // A split that balances on its own, admitted alone.
    let outcome = propose_all_against_pg(&pool, &program, &[split_posting("e3", 10, 6, 4)])
        .await
        .unwrap();
    assert!(
        matches!(outcome, PgAtomicOutcome::Committed { .. }),
        "{outcome:?}"
    );
    assert_eq!(
        count(&pool, "SELECT count(*) FROM morpholog.audit").await,
        3
    );

    // The same split after a simple posting of the same entry in the
    // same batch: its debit line is the earlier act's line (claims are
    // a set), so the entry's credits outrun its debits. Only a check
    // that sees the first act's delta can refuse the second.
    let outcome = propose_all_against_pg(
        &pool,
        &program,
        &[posting("e4", 10), split_posting("e4", 10, 6, 4)],
    )
    .await
    .unwrap();
    let PgAtomicOutcome::Rejected { act, rule, .. } = outcome else {
        panic!("the combined entry refuses, got {outcome:?}");
    };
    assert_eq!(act, 2);
    assert_eq!(rule.as_deref(), Some("balanced_posted_entry"));
    assert_eq!(
        count(&pool, "SELECT count(*) FROM morpholog.audit").await,
        3,
        "the refused batch wrote nothing"
    );
    assert_eq!(
        count(&pool, "SELECT count(*) FROM morpholog.rejections").await,
        1
    );

    // An unbalanced later act rolls the balanced earlier one back.
    let outcome = propose_all_against_pg(
        &pool,
        &program,
        &[posting("e5", 10), split_posting("e6", 10, 6, 1)],
    )
    .await
    .unwrap();
    let PgAtomicOutcome::Rejected { act, rule, .. } = outcome else {
        panic!("the unbalanced act refuses, got {outcome:?}");
    };
    assert_eq!(act, 2);
    assert_eq!(rule.as_deref(), Some("balanced_posted_entry"));
    assert_eq!(
        count(&pool, "SELECT count(*) FROM morpholog.audit").await,
        3
    );
}
