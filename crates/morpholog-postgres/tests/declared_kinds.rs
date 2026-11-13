//! A value of another kind than its field declares, on PostgreSQL.
//! Attacker capability: a caller who chooses a proposal's tagged
//! arguments, under a programme it cannot change. Whatever it sends, the
//! record must never hold a claim or an intent its programme's
//! declarations contradict: the proposal is a kernel error on every route,
//! and nothing is written, the acts it staged before the misfit included.
//!
//! The stored-claims check (`check --database-url`) and admission must
//! also agree on what fits, so the kinds read back from the claims table
//! are held to the kinds of the values that were admitted.

#![expect(
    clippy::unwrap_used,
    reason = "helper functions here fail fast on a broken fixture, and clippy exempts only `#[test]` functions"
)]

mod common;

use common::routes::{CLAIM_ROWS, RouteObservation, count, observe, persisted};
use common::{reset_db, seed_claims, test_pool};
use morpholog_core::{
    EvalError, EvalValue, PredicateArgKind, PreparedProgram, Subject, Transition,
};
use morpholog_postgres::{
    InvariantPlan, PgError, PgPool, PgProgram, PgProposalOutcome, Proposal, propose_against_pg,
    propose_all_against_pg, stored_claim_shapes,
};
use morpholog_test_support::differential::normalize_uuids;
use morpholog_test_support::{bool_, coll, date, dec, dur, qty, subj, ts};

const PROGRAMME: &str = "program declared_kinds
predicate Manifest(id: Subject, source_name: Date)
predicate Priced(id: Subject, amount: Decimal[USD])
predicate Day(id: Subject, on: Date)
predicate Held(id: Subject, held: Any)
intent Pay(account: Subject, amount: Decimal[GBP])
invariant priced_positive:
    Priced(id, amount) implies amount > 0 USD
transformation price_then_pay(id, amount, paid):
    admit Priced(id, amount)
    emit Pay(id, paid)
transformation pay_then_manifest(id, paid, source_name):
    emit Pay(id, paid)
    admit Manifest(id, source_name)
transformation days(id, dates):
    for d in dates:
        admit Day(id, d)
transformation hold(id, held):
    admit Held(id, held)
";

fn routes() -> Vec<(&'static str, PgProgram)> {
    let prepared =
        || PreparedProgram::new(morpholog_surface::parse_program(PROGRAMME).unwrap()).unwrap();
    let routes = vec![
        ("interpreted", PgProgram::interpreted(prepared())),
        ("compiled", PgProgram::new(prepared())),
    ];
    for (name, pg) in &routes {
        let plan = match pg.plan() {
            InvariantPlan::Interpreted { .. } => "interpreted",
            InvariantPlan::Compiled { .. } => "compiled",
            InvariantPlan::Mixed { .. } => "mixed",
        };
        assert_eq!(plan, *name, "the route under test");
    }
    routes
}

fn transition(name: &str, args: Vec<EvalValue>) -> Transition {
    Transition {
        transformation_name: name.into(),
        args,
        actor: Subject::from("w"),
        decided_at: None,
    }
}

fn act(name: &str, args: Vec<EvalValue>) -> Proposal {
    Proposal::gateway(&transition(name, args))
}

async fn written(pool: &PgPool) -> [i64; 4] {
    [
        count(pool, "SELECT count(*) FROM morpholog.claims").await,
        count(pool, "SELECT count(*) FROM morpholog.audit").await,
        count(pool, "SELECT count(*) FROM morpholog.outbox").await,
        count(pool, "SELECT count(*) FROM morpholog.rejections").await,
    ]
}

#[tokio::test]
async fn a_misfit_after_a_fitting_act_writes_nothing_on_every_route() {
    let pool = test_pool().await;
    use PredicateArgKind as K;
    let cases = [
        (
            "a fitting claim, then an intent of another kind",
            act("price_then_pay", vec![subj("p1"), qty("5", "USD"), dec(5)]),
            "Pay",
            K::Decimal,
        ),
        (
            "a fitting intent, then a claim of another kind",
            act(
                "pay_then_manifest",
                vec![subj("m1"), qty("5", "GBP"), subj("s1")],
            ),
            "Manifest",
            K::Subject,
        ),
        (
            "a loop whose third item is of another kind",
            act(
                "days",
                vec![
                    subj("d1"),
                    coll(vec![date("2026-11-07"), date("2026-11-08"), subj("x")]),
                ],
            ),
            "Day",
            K::Subject,
        ),
    ];
    for (route, pg) in routes() {
        for (case, proposal, named, found) in &cases {
            reset_db(&pool).await;
            let err = propose_against_pg(&pool, &pg, proposal).await.unwrap_err();
            let named_right = match &err {
                PgError::Kernel(EvalError::ClaimKindMismatch {
                    predicate,
                    found: f,
                    ..
                }) => predicate == named && f == found,
                PgError::Kernel(EvalError::IntentKindMismatch {
                    intent, found: f, ..
                }) => intent == named && f == found,
                _ => false,
            };
            assert!(named_right, "{route}, {case}: {err:?}");
            assert_eq!(
                written(&pool).await,
                [0; 4],
                "{route}, {case}: something was written"
            );
        }
        // One decision of several acts: a misfit in the last act leaves
        // the earlier acts unwritten too.
        reset_db(&pool).await;
        let acts = [
            act("hold", vec![subj("h1"), subj("x")]),
            act("price_then_pay", vec![subj("p1"), qty("5", "USD"), dec(5)]),
        ];
        let err = propose_all_against_pg(&pool, &pg, &acts).await.unwrap_err();
        assert!(
            matches!(err, PgError::Kernel(EvalError::IntentKindMismatch { .. })),
            "{route}, transact: {err:?}"
        );
        assert_eq!(
            written(&pool).await,
            [0; 4],
            "{route}, transact: something was written"
        );
    }
}

/// What the claims table says a stored value is must be what admission
/// says it is, kind for kind, units included: the two checks share one
/// rule, and this holds the storage representation to it.
#[tokio::test]
async fn the_kinds_read_back_from_storage_are_the_kinds_admitted() {
    let pool = test_pool().await;
    reset_db(&pool).await;
    let pg = PgProgram::new(
        PreparedProgram::new(morpholog_surface::parse_program(PROGRAMME).unwrap()).unwrap(),
    );
    let values = [
        subj("a"),
        dec(5),
        date("2026-11-07"),
        ts("2026-11-07T12:00:00Z"),
        dur("PT30M"),
        bool_(true),
        coll(vec![subj("a"), dec(1)]),
        qty("5", "USD"),
        qty("5", "EUR"),
    ];
    for (i, value) in values.iter().enumerate() {
        let outcome = propose_against_pg(
            &pool,
            &pg,
            &act("hold", vec![subj(&format!("h{i}")), value.clone()]),
        )
        .await
        .unwrap();
        assert!(
            matches!(outcome, PgProposalOutcome::Committed { .. }),
            "{value:?}"
        );
    }
    let mut stored: Vec<PredicateArgKind> = stored_claim_shapes(&pool)
        .await
        .unwrap()
        .into_iter()
        .filter(|s| s.predicate.as_str() == "Held")
        .map(|s| {
            assert_eq!(s.claims, 1, "one claim per kind: {s:?}");
            s.kinds[1].clone()
        })
        .collect();
    let mut admitted: Vec<PredicateArgKind> = values.iter().map(EvalValue::kind).collect();
    stored.sort();
    admitted.sort();
    assert_eq!(stored, admitted);
    let declared = [
        PredicateArgKind::Subject,
        PredicateArgKind::Decimal,
        PredicateArgKind::Quantity("USD".into()),
        PredicateArgKind::Quantity("EUR".into()),
        PredicateArgKind::Collection,
        PredicateArgKind::Any,
    ];
    for d in &declared {
        for (s, a) in stored.iter().zip(&admitted) {
            assert_eq!(d.holds(s), d.holds(a), "{d} over {s} and {a}");
        }
    }
}

const POTS: &str = "program pots
predicate Cap(c: Subject, cap: Decimal)
predicate Pot(c: Subject, p: Subject, amount: Decimal)
invariant pots_within_cap:
    Cap(c, cap) implies sum(a | Pot(c, _, a)) <= cap
transformation open(c, cap):
    admit Cap(c, cap)
transformation drop_pot(c, p):
    retract Pot(c, p, _)
";

/// History that holds quantities where decimals are declared, as an
/// older programme could admit: a sum over them succeeds in the kernel,
/// and the invariant then holds, fails or errors as written. Every route
/// decides each as the kernel does, alone and after another act in one
/// decision.
#[tokio::test]
async fn a_sum_over_drifted_values_of_one_kind_decides_as_the_kernel_does() {
    let pool = test_pool().await;
    let prepared =
        || PreparedProgram::new(morpholog_surface::parse_program(POTS).unwrap()).unwrap();
    let routes = [
        ("interpreted", PgProgram::interpreted(prepared())),
        ("compiled", PgProgram::new(prepared())),
    ];
    assert!(matches!(routes[1].1.plan(), InvariantPlan::Compiled));
    let claim = |predicate: &str, args: Vec<EvalValue>| morpholog_core::ClaimInstance {
        predicate: predicate.into(),
        args,
    };
    let history = |cap: EvalValue| {
        vec![
            claim("Cap", vec![subj("k"), cap]),
            claim("Pot", vec![subj("k"), subj("a"), qty("2", "MW")]),
            claim("Pot", vec![subj("k"), subj("b"), qty("3", "MW")]),
        ]
    };
    let drop = transition("drop_pot", vec![subj("k"), subj("b")]);
    let open = act("open", vec![subj("k2"), dec(5)]);
    for (case, cap, expected) in [
        ("holds", qty("100", "MW"), "committed"),
        ("fails", qty("1", "MW"), "rejected"),
        ("errors", dec(100), "kernel error"),
    ] {
        let mut seen = Vec::new();
        for (route, pg) in &routes {
            let alone = observe(&pool, pg, &history(cap.clone()), &drop).await;
            let label = match &alone {
                RouteObservation::Decided(o) if o.outcome.starts_with("committed") => "committed",
                RouteObservation::Decided(_) => "rejected",
                RouteObservation::Kernel(_) => "kernel error",
                RouteObservation::Operational(_) => "operational",
            };
            assert_eq!(label, expected, "{route}, {case}: {alone:?}");

            reset_db(&pool).await;
            seed_claims(&pool, &history(cap.clone())).await;
            let together =
                match propose_all_against_pg(&pool, pg, &[open.clone(), Proposal::gateway(&drop)])
                    .await
                {
                    Ok(outcome) => normalize_uuids(&format!("{outcome:?}")),
                    Err(PgError::Kernel(e)) => format!("kernel error {e:?}"),
                    Err(e) => panic!("{route}, {case}: operational {e:?}"),
                };
            seen.push((route, alone, together, persisted(&pool, CLAIM_ROWS).await));
        }
        let (_, alone, together, claims) = &seen[0];
        for (route, a, t, c) in &seen[1..] {
            assert_eq!(a, alone, "{route}, {case}: alone");
            assert_eq!(t, together, "{route}, {case}: after another act");
            assert_eq!(c, claims, "{route}, {case}: what the decision left");
        }
    }
}
