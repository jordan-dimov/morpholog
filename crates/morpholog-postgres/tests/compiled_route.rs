//! The compiled route beside the interpreter, on the production path.
//! Every whole-in-fragment gallery programme is proposed twice from the
//! same seeded state, once through each route, and the two must reach
//! the same decision: the same outcome and reason, the same refusing
//! rule and version, the same witness variables, the same semantic
//! rejection-log fields, and the same persisted audit, claim and outbox
//! rows up to generated identities and times. Witness values are
//! observational (a symmetric plan may name the violating pair in
//! another order).
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

/// Rows as text with generated identities normalised, sorted, so two
/// runs that minted different subjects still compare equal when they
/// persisted the same thing. Audit rows keep every field but their
/// transition id and time; claims drop the transition that asserted
/// them; outbox rows drop their ids and the key derived from one.
async fn persisted(pool: &PgPool, sql: &'static str) -> Vec<String> {
    let rows: Vec<String> = sqlx::query_scalar(sql).fetch_all(pool).await.unwrap();
    let mut rows: Vec<String> = rows.iter().map(|r| normalize_uuids(r)).collect();
    rows.sort();
    rows
}

const AUDIT_ROWS: &str = "SELECT jsonb_build_object(
        'transformation', transformation_name, 'arguments', arguments, 'actor', actor,
        'epoch', invariant_epoch, 'checked', invariants_checked,
        'asserted', asserted_claims, 'retracted', retracted_claims,
        'emitted', emitted_intents, 'attestation', attestation, 'parameters', parameters
    )::text FROM morpholog.audit";

const CLAIM_ROWS: &str =
    "SELECT jsonb_build_object('predicate', predicate_name, 'arguments', arguments)::text
     FROM morpholog.claims";

const OUTBOX_ROWS: &str =
    "SELECT jsonb_build_object('intent', intent_type, 'arguments', arguments)::text
     FROM morpholog.outbox";

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
    audit: Vec<String>,
    claims: Vec<String>,
    outbox: Vec<String>,
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
        audit: persisted(pool, AUDIT_ROWS).await,
        claims: persisted(pool, CLAIM_ROWS).await,
        outbox: persisted(pool, OUTBOX_ROWS).await,
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

/// Both routes, from the same seeded ledger, on a balanced posting of
/// `amount` to `entry_id`.
async fn both_routes(
    pool: &PgPool,
    seeded: &[ClaimInstance],
    entry_id: &str,
    amount: i64,
) -> (RouteObservation, RouteObservation) {
    let transition = Transition {
        transformation_name: "post_simple_entry".into(),
        args: vec![
            subj(entry_id),
            subj("d_2026_05_17"),
            subj("p_2026_05"),
            subj("account_cash"),
            subj("account_revenue"),
            dec(amount),
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
/// decimal holds. Admission is case-local on both routes: a posting
/// elsewhere is admitted despite both; a posting onto the overflowing
/// entry is the range error, with nothing recorded anywhere; a posting
/// onto the unbalanced entry is that entry's refusal, never the other
/// entry's error.
#[tokio::test]
async fn admission_is_case_local_on_both_routes() {
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

    let (spec, real) = both_routes(&pool, &seeded, "fresh", 10).await;
    assert!(
        matches!(&spec, RouteObservation::Decided(o) if o.outcome.starts_with("committed")),
        "inherited dirt elsewhere does not block: {spec:?}"
    );
    assert_eq!(real, spec);

    let (spec, real) = both_routes(&pool, &seeded, "e1", 10).await;
    assert_eq!(
        spec,
        RouteObservation::Kernel(EvalError::sum_out_of_decimal_range())
    );
    assert_eq!(real, spec);
    assert_eq!(
        count(&pool, "SELECT count(*) FROM morpholog.audit").await,
        0
    );
    assert_eq!(
        count(&pool, "SELECT count(*) FROM morpholog.rejections").await,
        0
    );
    assert_eq!(
        count(&pool, "SELECT count(*) FROM morpholog.outbox").await,
        0
    );
    assert_eq!(
        count(&pool, "SELECT count(*) FROM morpholog.claims").await,
        seeded.len() as i64,
        "the posting's delta rolled back"
    );

    let (spec, real) = both_routes(&pool, &seeded, "e0", 10).await;
    assert!(
        matches!(&spec, RouteObservation::Decided(o)
            if o.outcome.contains("balanced_posted_entry") && o.outcome.contains("witness vars [\"entry\"]")),
        "touching the unbalanced entry refuses on it: {spec:?}"
    );
    assert_eq!(real, spec);
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
    // A posting of nothing onto the entry raises its obligation: the
    // totals stay at the maximum, and both routes admit.
    let (spec, real) = both_routes(&pool, &seeded, "e1", 0).await;
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
    // Admission being case-local, only a posting onto that entry reads
    // it.
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

    // A posting onto the corrupt entry: its obligation reads the row.
    let second = propose_against_pg(&pool, &program, &split_posting("e1", 10, 6, 4)).await;
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

/// A retract followed by a re-admit of the same claim in one delta
/// changes nothing, on both routes: the compiled route reads the
/// delta back from the table as one deletion and one insertion and
/// must net them, or it would revalidate a dirty case the kernel
/// leaves untouched.
#[tokio::test]
async fn a_retract_and_readmit_touches_nothing_on_both_routes() {
    let source = "program churn_ledger
predicate Entry(e: Subject)
predicate Line(e: Subject, side: Subject, dr: Decimal, cr: Decimal)
invariant balanced:
    Entry(e) implies sum(d | Line(e, _, d, _)) = sum(c | Line(e, _, _, c))
transformation churn(e, side, dr, cr):
    retract Line(e, side, dr, cr)
    admit Line(e, side, dr, cr)
";
    let program = morpholog_surface::parse_program(source).expect("parses");
    let compiled = PgProgram::new(CompiledProgram::new(program.clone()).unwrap());
    assert!(matches!(compiled.plan(), InvariantPlan::Compiled { .. }));
    let interpreted = PgProgram::interpreted(CompiledProgram::new(program).unwrap());
    // One dirty entry; churning its line repairs nothing and changes nothing.
    let seeded = vec![
        ClaimInstance {
            predicate: "Entry".into(),
            args: vec![subj("legacy")],
        },
        ClaimInstance {
            predicate: "Line".into(),
            args: vec![subj("legacy"), subj("cash"), dec(100), dec(0)],
        },
    ];
    let transition = Transition {
        transformation_name: "churn".into(),
        args: vec![subj("legacy"), subj("cash"), dec(100), dec(0)],
        actor: Subject::from("route_test"),
    };
    let pool = test_pool().await;
    let spec = observe(&pool, &interpreted, &seeded, &transition).await;
    assert!(
        matches!(&spec, RouteObservation::Decided(o) if o.outcome.starts_with("committed")),
        "{spec:?}"
    );
    let real = observe(&pool, &compiled, &seeded, &transition).await;
    assert_eq!(real, spec);
}

/// The same churn as one act of an atomic batch: the batch route nets
/// its row effects too.
#[tokio::test]
async fn a_retract_and_readmit_touches_nothing_in_a_batch_on_both_routes() {
    let source = "program churn_ledger
predicate Entry(e: Subject)
predicate Line(e: Subject, side: Subject, dr: Decimal, cr: Decimal)
invariant balanced:
    Entry(e) implies sum(d | Line(e, _, d, _)) = sum(c | Line(e, _, _, c))
transformation churn(e, side, dr, cr):
    retract Line(e, side, dr, cr)
    admit Line(e, side, dr, cr)
";
    let program = morpholog_surface::parse_program(source).expect("parses");
    let compiled = PgProgram::new(CompiledProgram::new(program.clone()).unwrap());
    let interpreted = PgProgram::interpreted(CompiledProgram::new(program).unwrap());
    let seeded = vec![
        ClaimInstance {
            predicate: "Entry".into(),
            args: vec![subj("legacy")],
        },
        ClaimInstance {
            predicate: "Line".into(),
            args: vec![subj("legacy"), subj("cash"), dec(100), dec(0)],
        },
    ];
    let act = Proposal::gateway(&Transition {
        transformation_name: "churn".into(),
        args: vec![subj("legacy"), subj("cash"), dec(100), dec(0)],
        actor: Subject::from("route_test"),
    });
    let pool = test_pool().await;
    for program in [&interpreted, &compiled] {
        reset_db(&pool).await;
        seed(&pool, &seeded).await;
        let outcome = propose_all_against_pg(&pool, program, std::slice::from_ref(&act))
            .await
            .unwrap();
        assert!(
            matches!(outcome, PgAtomicOutcome::Committed { .. }),
            "{outcome:?}"
        );
    }
}
