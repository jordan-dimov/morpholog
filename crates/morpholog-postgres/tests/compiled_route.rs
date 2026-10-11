//! The compiled route beside the interpreter, on the production path.
//! Each fully compilable programme is proposed through both routes from
//! the same state. They must agree on outcome, reason, refusing rule and
//! version, witness variables, rejection-log fields, and persisted rows
//! (ignoring generated ids and times). Witness values may differ in
//! order, so they are not compared.
//!
//! A compiled check never falls back: a SQL error inside one is an
//! operational error, rolled back with nothing recorded.

#![allow(clippy::unwrap_used, clippy::expect_used)]

mod common;

use common::routes::{RouteObservation, count, observe};
use common::{reset_db, seed_claims, test_pool};
use morpholog_core::{
    ClaimInstance, EvalError, EvalValue, PreparedProgram, Program, Subject, Transition,
};
use morpholog_examples::{clinical_trial_enrolment, double_entry_ledger};
use morpholog_postgres::{
    InvariantPlan, PgAtomicOutcome, PgError, PgPool, PgProgram, PgProposalOutcome, Proposal,
    list_rejection_rows, propose_against_pg, propose_all_against_pg,
};
use morpholog_test_support::differential::{sample_args, sample_state};
use morpholog_test_support::{date, dec, subj};

fn eligible_gallery() -> Vec<Program> {
    morpholog_examples::all_programs()
        .into_iter()
        .filter(|p| {
            let program = PgProgram::new(PreparedProgram::new(p.clone()).unwrap());
            matches!(program.plan(), InvariantPlan::Compiled)
        })
        .collect()
}

#[tokio::test]
async fn both_routes_reach_the_same_decision_over_the_gallery() {
    let pool = test_pool().await;
    let (mut cases, mut commits, mut refusals, mut errors, mut skipped) =
        (0usize, 0usize, 0usize, 0usize, 0usize);
    for program in eligible_gallery() {
        let compiled = PgProgram::new(PreparedProgram::new(program.clone()).unwrap());
        let interpreted = PgProgram::interpreted(PreparedProgram::new(program.clone()).unwrap());
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
        PgProgram::interpreted(PreparedProgram::new(double_entry_ledger::program()).unwrap());
    let spec = observe(pool, &interpreted, seeded, &transition).await;
    let real = observe(pool, &ledger(), seeded, &transition).await;
    (spec, real)
}

/// One entry is unbalanced; another's total overflows a decimal. On both
/// routes a posting elsewhere is admitted, a posting onto the overflowing
/// entry is a range error with nothing recorded, and a posting onto the
/// unbalanced entry is refused for that entry, not the other's error.
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

/// A rule that calls a definition is admitted case by case too. One
/// participant was randomised with no consent on record, history the rule
/// forbids. On both routes a consent for another participant is admitted,
/// and a late consent for the first, which cures nothing, is refused.
#[tokio::test]
async fn a_rule_calling_a_definition_is_case_local_on_both_routes() {
    let pool = test_pool().await;
    let seeded = vec![
        ClaimInstance {
            predicate: "Trial".into(),
            args: vec![subj("t1")],
        },
        ClaimInstance {
            predicate: "ParticipantRandomised".into(),
            args: vec![
                subj("p1"),
                subj("t1"),
                subj("v1"),
                date("2026-03-01"),
                subj("investigator"),
            ],
        },
    ];
    let consent = |participant: &str, on: &str| Transition {
        transformation_name: "record_consent".into(),
        args: vec![
            subj(participant),
            subj("t1"),
            subj("cf1"),
            date(on),
            subj("investigator"),
        ],
        actor: Subject::from("route_test"),
    };
    let compiled =
        PgProgram::new(PreparedProgram::new(clinical_trial_enrolment::program()).unwrap());
    let interpreted =
        PgProgram::interpreted(PreparedProgram::new(clinical_trial_enrolment::program()).unwrap());

    let elsewhere = consent("p2", "2026-02-01");
    let spec = observe(&pool, &interpreted, &seeded, &elsewhere).await;
    assert!(
        matches!(&spec, RouteObservation::Decided(o) if o.outcome.starts_with("committed")),
        "inherited dirt in another participant's case does not block: {spec:?}"
    );
    assert_eq!(observe(&pool, &compiled, &seeded, &elsewhere).await, spec);

    let late = consent("p1", "2026-04-01");
    let spec = observe(&pool, &interpreted, &seeded, &late).await;
    assert!(
        matches!(&spec, RouteObservation::Decided(o)
            if o.outcome.contains("consent_obtained_before_randomisation")),
        "touching the participant's own case refuses on it: {spec:?}"
    );
    assert_eq!(observe(&pool, &compiled, &seeded, &late).await, spec);
}

/// The kernel sums wider than a decimal and checks only the final
/// total, so an overflow that cancels is no error. The compiled check
/// must agree and admit.
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
    // A zero posting onto the entry forces its check: the totals stay
    // at the maximum, and both routes admit.
    let (spec, real) = both_routes(&pool, &seeded, "e1", 0).await;
    assert!(
        matches!(&spec, RouteObservation::Decided(o) if o.outcome.starts_with("committed")),
        "{spec:?}"
    );
    assert_eq!(real, spec);
}

/// The ledger on the compiled route. Checked here so no test that
/// compares the two routes can silently compare the interpreter with
/// itself.
fn ledger() -> PgProgram {
    let program = PgProgram::new(PreparedProgram::new(double_entry_ledger::program()).unwrap());
    assert!(matches!(program.plan(), InvariantPlan::Compiled));
    program
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
async fn a_failing_compiled_check_is_an_operational_error_never_a_decision() {
    let pool = test_pool().await;
    reset_db(&pool).await;
    let program = ledger();
    let first = propose_against_pg(&pool, &program, &posting("e1", 100))
        .await
        .unwrap();
    assert!(matches!(first, PgProposalOutcome::Committed { .. }));

    // A line whose amount is not a number: a corrupt row is the only way
    // to make a correct query fail. Only a posting onto that entry reads
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
        list_rejection_rows(&pool, 10, None)
            .await
            .unwrap()
            .is_empty(),
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

    // The same split after a simple posting of the same entry in one
    // batch: its debit line is the earlier act's line (claims are a
    // set), so credits exceed debits. Only a check that sees the first
    // act's delta can refuse the second.
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

/// A ledger whose one act retracts a line and admits it again, on the
/// compiled route and on the interpreter.
fn churn_ledger() -> (PgProgram, PgProgram) {
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
    let compiled = PgProgram::new(PreparedProgram::new(program.clone()).unwrap());
    assert!(matches!(compiled.plan(), InvariantPlan::Compiled));
    let interpreted = PgProgram::interpreted(PreparedProgram::new(program).unwrap());
    (compiled, interpreted)
}

/// One unbalanced entry; churning its line repairs nothing and changes
/// nothing.
fn dirty_entry() -> Vec<ClaimInstance> {
    vec![
        ClaimInstance {
            predicate: "Entry".into(),
            args: vec![subj("legacy")],
        },
        ClaimInstance {
            predicate: "Line".into(),
            args: vec![subj("legacy"), subj("cash"), dec(100), dec(0)],
        },
    ]
}

fn churn() -> Transition {
    Transition {
        transformation_name: "churn".into(),
        args: vec![subj("legacy"), subj("cash"), dec(100), dec(0)],
        actor: Subject::from("route_test"),
    }
}

/// Retracting and re-admitting a claim in one delta changes nothing, on
/// both routes. The compiled route sees a deletion and an insertion and
/// must net them, or it would recheck a dirty case the kernel leaves
/// alone.
#[tokio::test]
async fn a_retract_and_readmit_touches_nothing_on_both_routes() {
    let (compiled, interpreted) = churn_ledger();
    let (seeded, transition) = (dirty_entry(), churn());
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
    let (compiled, interpreted) = churn_ledger();
    let (seeded, act) = (dirty_entry(), Proposal::gateway(&churn()));
    let pool = test_pool().await;
    for program in [&interpreted, &compiled] {
        reset_db(&pool).await;
        seed_claims(&pool, &seeded).await;
        let outcome = propose_all_against_pg(&pool, program, std::slice::from_ref(&act))
            .await
            .unwrap();
        assert!(
            matches!(outcome, PgAtomicOutcome::Committed { .. }),
            "{outcome:?}"
        );
    }
}

/// Two foreign units already in history and a third admitted by the
/// batch's first act; the second act's admission makes the rule check
/// every row. Both routes raise, and both must name the pair the kernel
/// meets first: history in its loaded order, then the batch's acts in
/// order. Attacker capability modelled: none; the fixture rows stand in
/// for history admitted under an older declaration.
#[tokio::test]
async fn a_compiled_batch_names_the_kernels_first_pair_across_acts() {
    let program = morpholog_surface::parse_program(
        "program foreign_units
predicate Enabled(flag: Subject)
predicate Terms(x: Subject, q: Decimal[MW])
invariant quantity_is_positive:
    Enabled(_) and Terms(_, q) implies q > 0 MW
transformation enable(flag):
    admit Enabled(flag)
transformation hold(x, q):
    admit Terms(x, q)
",
    )
    .unwrap();
    let interpreted = PgProgram::interpreted(PreparedProgram::new(program.clone()).unwrap());
    let compiled = PgProgram::new(PreparedProgram::new(program).unwrap());
    assert!(matches!(compiled.plan(), InvariantPlan::Compiled { .. }));
    let history = [
        ClaimInstance {
            predicate: "Terms".into(),
            args: vec![subj("a"), morpholog_test_support::qty("5", "EUR")],
        },
        ClaimInstance {
            predicate: "Terms".into(),
            args: vec![subj("b"), morpholog_test_support::qty("7", "GBP")],
        },
    ];
    let acts = [
        Proposal::gateway(&Transition {
            transformation_name: "hold".into(),
            args: vec![subj("c"), morpholog_test_support::qty("9", "USD")],
            actor: Subject::from("route_test"),
        }),
        Proposal::gateway(&Transition {
            transformation_name: "enable".into(),
            args: vec![subj("f")],
            actor: Subject::from("route_test"),
        }),
    ];
    let pool = test_pool().await;
    let mut errors = Vec::new();
    for route in [&interpreted, &compiled] {
        reset_db(&pool).await;
        seed_claims(&pool, &history).await;
        match propose_all_against_pg(&pool, route, &acts).await {
            Err(PgError::Kernel(e)) => errors.push(e),
            other => panic!("the batch must raise on both routes, got {other:?}"),
        }
    }
    assert_eq!(errors[0], errors[1], "interpreted vs compiled");
    let named = errors[0].to_string();
    assert!(
        !named.contains("USD"),
        "the batch's own row is met last, never named first: {named}"
    );
}

// ============================================================
// The comparison a refusal blames, on both routes
// ============================================================

/// The two forms that make the compiled diagnosis do real work: a sum as
/// an operand, computed once more under the chosen rows, and a comparison
/// of two literals, with no claim row of its own to pin. Both routes name
/// the same comparison with the same values, beside the same witness.
#[tokio::test]
async fn both_routes_blame_the_same_comparison_with_the_same_values() {
    let pool = test_pool().await;
    let program = morpholog_surface::parse_program(
        "program parcels\n\
         predicate Cap(vessel: Subject, cap: Decimal)\n\
             unique by (vessel)\n\
         predicate Parcel(parcel: Subject, vessel: Subject, qty: Decimal)\n\
             unique by (parcel)\n\
         predicate Flag(flag: Subject)\n\
         transformation set_cap(v, c):\n    admit Cap(v, c)\n\
         transformation load(p, v, q):\n    admit Parcel(p, v, q)\n\
         transformation raise(f):\n    admit Flag(f)\n\
         invariant within_capacity:\n\
         \x20   Cap(v, cap) implies sum(q | Parcel(_, v, q)) <= cap\n\
         invariant never_raised:\n\
         \x20   Flag(f) implies 1 <= 0\n\
         invariant fleet_total:\n\
         \x20   sum(q | Parcel(_, _, q)) <= 100\n",
    )
    .unwrap();
    let compiled = PgProgram::new(PreparedProgram::new(program.clone()).unwrap());
    assert!(
        matches!(compiled.plan(), InvariantPlan::Compiled),
        "{:?}",
        compiled.plan()
    );
    let interpreted = PgProgram::interpreted(PreparedProgram::new(program.clone()).unwrap());
    let act = |name: &str| {
        compiled
            .prepared()
            .transformation(&name.into())
            .unwrap()
            .clone()
    };
    let seeded = vec![
        ClaimInstance {
            predicate: "Cap".into(),
            args: vec![subj("v1"), dec(10)],
        },
        ClaimInstance {
            predicate: "Cap".into(),
            args: vec![subj("v2"), dec(200)],
        },
        ClaimInstance {
            predicate: "Parcel".into(),
            args: vec![subj("p1"), subj("v1"), dec(6)],
        },
    ];
    let refusals = |t: Transition| {
        let compiled = &compiled;
        let interpreted = &interpreted;
        let seeded = &seeded;
        let pool = &pool;
        async move {
            let mut out = Vec::new();
            for route in [interpreted, compiled] {
                reset_db(pool).await;
                seed_claims(pool, seeded).await;
                let outcome = propose_against_pg(pool, route, &Proposal::gateway(&t))
                    .await
                    .unwrap();
                let PgProposalOutcome::Rejected {
                    rule,
                    witness,
                    compared,
                    ..
                } = outcome
                else {
                    panic!("expected a refusal, got {outcome:?}");
                };
                out.push((rule, witness, compared));
            }
            out
        }
    };

    // A sum as an operand: 6 + 7 against a cap of 10.
    let t = Transition {
        transformation_name: "load".into(),
        args: vec![subj("p2"), subj("v1"), dec(7)],
        actor: Subject::from("route_test"),
    };
    let both = refusals(t).await;
    assert_eq!(both[0], both[1], "interpreted vs compiled");
    let (rule, _, compared) = &both[1];
    assert_eq!(rule.as_deref(), Some("within_capacity"));
    let compared = compared.clone().expect("the comparison is blamed");
    assert_eq!(compared.op, "<=");
    assert_eq!(compared.left, dec(13));
    assert_eq!(compared.right, dec(10));
    assert_eq!(compared.holds(), Some(false));

    // Two literals: no claim row of the comparison's own to pin.
    let t = Transition {
        transformation_name: "raise".into(),
        args: vec![subj("f1")],
        actor: Subject::from("route_test"),
    };
    let both = refusals(t).await;
    assert_eq!(both[0], both[1], "interpreted vs compiled");
    let (rule, _, compared) = &both[1];
    assert_eq!(rule.as_deref(), Some("never_raised"));
    assert_eq!(
        compared.clone().expect("the comparison is blamed"),
        morpholog_core::Compared {
            op: "<=".to_string(),
            left: dec(1),
            right: dec(0),
        }
    );
    // A comparison that is the whole rule, with nothing to descend
    // into: 6 + 95 across the fleet against 100, the vessel's own cap
    // untouched.
    let t = Transition {
        transformation_name: "load".into(),
        args: vec![subj("p3"), subj("v2"), dec(95)],
        actor: Subject::from("route_test"),
    };
    let both = refusals(t).await;
    assert_eq!(both[0], both[1], "interpreted vs compiled");
    let (rule, witness, compared) = &both[1];
    assert_eq!(rule.as_deref(), Some("fleet_total"));
    assert!(
        witness.is_empty(),
        "nothing bound at a top-level comparison"
    );
    assert_eq!(
        compared.clone().expect("the comparison is blamed"),
        morpholog_core::Compared {
            op: "<=".to_string(),
            left: dec(101),
            right: dec(100),
        }
    );
    let _ = act("set_cap");
}

/// The one query run only to explain a refusal: a statement error in it
/// answers nothing and leaves the transaction usable, never aborted, so
/// the refusal already decided is what the caller gets.
#[tokio::test]
async fn a_failing_diagnostic_query_answers_nothing_and_keeps_the_transaction() {
    let pool = test_pool().await;
    let mut tx = pool.begin().await.unwrap();
    let answer = morpholog_postgres::fetch_diagnostic(&mut tx, "SELECT 1/0 AS \"t\"".to_string())
        .await
        .expect("a statement error is not an operational error here");
    assert!(answer.is_none());
    let one: i32 = sqlx::query_scalar("SELECT 1")
        .fetch_one(&mut *tx)
        .await
        .expect("the transaction is not aborted");
    assert_eq!(one, 1);
    let answered = morpholog_postgres::fetch_diagnostic(&mut tx, "SELECT '7' AS \"t\"".to_string())
        .await
        .unwrap();
    assert!(answered.is_some(), "a sound query still answers");
    tx.rollback().await.unwrap();
}

/// A consequent `or` on both routes: either branch admits, neither
/// refuses with the entry witness and no comparison blamed, and the
/// kernel's non-short-circuit error rule holds: a later branch that
/// raises raises whether or not an earlier one held, and the first
/// raising branch names the error. The raising is kind drift, a stored
/// subject where the rule compares a decimal.
#[tokio::test]
async fn a_consequent_or_decides_and_raises_as_the_kernel_does_on_both_routes() {
    let pool = test_pool().await;
    let program = morpholog_surface::parse_program(
        "program either\n\
         predicate A(x: Subject, v: Decimal, w: Date)\n\
         predicate B(x: Subject)\n\
         predicate C(x: Subject)\n\
         transformation note(x, v, w):\n    admit A(x, v, w)\n\
         transformation mark_b(x):\n    admit B(x)\n\
         invariant either_record:\n\
         \x20   A(x, _, _) implies (B(x) or C(x))\n\
         invariant marked_or_small:\n\
         \x20   A(x, v, w) implies (B(x) or v <= 10 or w on_or_before @2026-12-31)\n",
    )
    .unwrap();
    let compiled = PgProgram::new(PreparedProgram::new(program.clone()).unwrap());
    assert!(
        matches!(compiled.plan(), InvariantPlan::Compiled),
        "{:?}",
        compiled.plan()
    );
    let interpreted = PgProgram::interpreted(PreparedProgram::new(program.clone()).unwrap());
    let run = |seeded: Vec<ClaimInstance>, t: Transition| {
        let compiled = &compiled;
        let interpreted = &interpreted;
        let pool = &pool;
        async move {
            let mut out = Vec::new();
            for route in [interpreted, compiled] {
                reset_db(pool).await;
                seed_claims(pool, &seeded).await;
                out.push(
                    propose_against_pg(pool, route, &Proposal::gateway(&t))
                        .await
                        .map(|o| match o {
                            PgProposalOutcome::Committed { .. } => "committed".to_string(),
                            PgProposalOutcome::Rejected {
                                rule,
                                witness,
                                compared,
                                ..
                            } => format!("rejected {rule:?} {witness:?} {compared:?}"),
                        })
                        .map_err(|e| e.to_string()),
                );
            }
            assert_eq!(out[0], out[1], "interpreted vs compiled");
            out.remove(1)
        }
    };
    let note = |x: &str, v: i64, w: &str| Transition {
        transformation_name: "note".into(),
        args: vec![subj(x), dec(v), morpholog_test_support::date(w)],
        actor: Subject::from("route_test"),
    };
    let claim = |p: &str, args: Vec<EvalValue>| ClaimInstance {
        predicate: p.into(),
        args,
    };

    // Either branch admits; both admit; neither refuses with the entry
    // witness and nothing compared, though a branch is a comparison.
    assert_eq!(
        run(
            vec![claim("B", vec![subj("x1")])],
            note("x1", 1, "2026-01-01")
        )
        .await,
        Ok("committed".into())
    );
    assert_eq!(
        run(
            vec![claim("C", vec![subj("x1")])],
            note("x1", 1, "2026-01-01")
        )
        .await,
        Ok("committed".into())
    );
    assert_eq!(
        run(
            vec![claim("B", vec![subj("x1")]), claim("C", vec![subj("x1")])],
            note("x1", 1, "2026-01-01")
        )
        .await,
        Ok("committed".into())
    );
    let refused = run(vec![], note("x1", 1, "2026-01-01")).await.unwrap();
    assert!(
        refused.starts_with("rejected Some(\"either_record\")"),
        "{refused}"
    );
    assert!(
        refused.contains("Var(\"x\")") && refused.ends_with("None"),
        "{refused}"
    );

    // Marking x2 touches the case of a stored row that holds a subject
    // where the rule compares a decimal: every antecedent variable is
    // part of a case, so only a change through `B(x)` reaches that row.
    // B holds for it, the later branch raises, and the kernel's error
    // wins over the branch that held.
    let mark_b = |x: &str| Transition {
        transformation_name: "mark_b".into(),
        args: vec![subj(x)],
        actor: Subject::from("route_test"),
    };
    let drifted = |x: &str, v: EvalValue, w: EvalValue| vec![claim("A", vec![subj(x), v, w])];
    let one = run(
        drifted(
            "x2",
            subj("first"),
            morpholog_test_support::date("2026-01-01"),
        ),
        mark_b("x2"),
    )
    .await;
    let one = one.expect_err("a raising branch raises whatever held");
    // Two raising branches: the first names the error, so the message
    // is the one-drift message exactly.
    let two = run(drifted("x2", subj("first"), subj("second")), mark_b("x2")).await;
    assert_eq!(two.expect_err("both raise"), one);
    let second_only = run(drifted("x2", dec(1), subj("second")), mark_b("x2")).await;
    assert_ne!(
        second_only.expect_err("the later branch raises on its own"),
        one,
        "the two drifts are in different domains and word differently"
    );
}
