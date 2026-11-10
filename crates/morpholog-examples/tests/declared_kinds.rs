//! A value admitted into a claim, or emitted into an intent, must be of
//! the kind its field declares. Arguments here are tagged values, as
//! `propose --args` sends them, so no codec checks them first: the kernel
//! is the boundary. A misfit is a kernel error naming the predicate or
//! intent, the field, the declared kind and the kind found. It is never a
//! refusal, and the proposal stages nothing.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use morpholog_core::{
    ClaimInstance, EvalError, EvalValue, Outcome, PredicateArgKind, Program, Rejection, State,
    TraceEntry, TracedProposal, Verdict,
};
use morpholog_surface::parse_program;
use morpholog_test_support::{
    cal_span, claim_instance, coll, date, dec, explain, fresh, propose, propose_with_trace, qty,
    subj, test_transition, transition_at,
};

const PROGRAM: &str = "
program kinds

predicate Manifest(id: Subject, source_name: Date)
predicate Priced(id: Subject, amount: Decimal[USD])
predicate Noted(id: Subject, note: Any)
predicate Tagged(id: Subject, tags: Collection)
predicate Day(id: Subject, on: Date)
predicate Stamped(id: Subject, at: Timestamp)

intent Pay(account: Subject, amount: Decimal[GBP])

transformation manifest(id, source_name):
    admit Manifest(id, source_name)

transformation price(id, amount):
    admit Priced(id, amount)

transformation note(id, note):
    admit Noted(id, note)

transformation tag(id, tags):
    admit Tagged(id, tags)

transformation pay(account, amount):
    emit Pay(account, amount)

transformation price_then_pay(id, amount, paid):
    admit Priced(id, amount)
    emit Pay(id, paid)

transformation pay_then_manifest(id, paid, source_name):
    emit Pay(id, paid)
    admit Manifest(id, source_name)

transformation days(id, dates):
    for d in dates:
        admit Day(id, d)

transformation stamp():
    let id = new Subject()
    admit Stamped(id, decided_at)

transformation drop_manifest(id):
    require Manifest(id, _)
    retract Manifest(id, _)
";

fn program() -> Program {
    parse_program(PROGRAM).expect("parses")
}

fn run(name: &str, args: Vec<EvalValue>, state: &State) -> Result<Outcome, EvalError> {
    let p = program();
    let t = p.transformation(name).unwrap();
    propose(&p, &test_transition(t, args), state, &mut fresh())
}

fn claim_misfit(
    predicate: &str,
    field: &str,
    declared: PredicateArgKind,
    found: PredicateArgKind,
) -> EvalError {
    EvalError::ClaimKindMismatch {
        predicate: predicate.into(),
        field: field.into(),
        declared,
        found,
    }
}

fn intent_misfit(
    intent: &str,
    field: &str,
    declared: PredicateArgKind,
    found: PredicateArgKind,
) -> EvalError {
    EvalError::IntentKindMismatch {
        intent: intent.into(),
        field: field.into(),
        declared,
        found,
    }
}

/// Every misfit, with the error it must raise.
fn misfits() -> Vec<(&'static str, Vec<EvalValue>, EvalError)> {
    use PredicateArgKind as K;
    vec![
        (
            "manifest",
            vec![subj("m1"), subj("s1")],
            claim_misfit("Manifest", "source_name", K::Date, K::Subject),
        ),
        (
            "price",
            vec![subj("p1"), dec(5)],
            claim_misfit("Priced", "amount", K::Quantity("USD".into()), K::Decimal),
        ),
        (
            "price",
            vec![subj("p1"), qty("5", "EUR")],
            claim_misfit(
                "Priced",
                "amount",
                K::Quantity("USD".into()),
                K::Quantity("EUR".into()),
            ),
        ),
        (
            "pay",
            vec![subj("a1"), dec(5)],
            intent_misfit("Pay", "amount", K::Quantity("GBP".into()), K::Decimal),
        ),
        (
            "price_then_pay",
            vec![subj("p1"), qty("5", "USD"), dec(5)],
            intent_misfit("Pay", "amount", K::Quantity("GBP".into()), K::Decimal),
        ),
        (
            "pay_then_manifest",
            vec![subj("m1"), qty("5", "GBP"), subj("s1")],
            claim_misfit("Manifest", "source_name", K::Date, K::Subject),
        ),
        (
            "days",
            vec![
                subj("d1"),
                coll(vec![date("2026-11-07"), date("2026-11-08"), subj("x")]),
            ],
            claim_misfit("Day", "on", K::Date, K::Subject),
        ),
    ]
}

#[test]
fn a_value_of_another_kind_is_a_kernel_error_naming_the_field() {
    for (name, args, expected) in misfits() {
        let got = run(name, args, &State::default());
        assert_eq!(got.unwrap_err(), expected, "{name}");
    }
}

#[test]
fn values_of_the_declared_kind_are_admitted() {
    let fits: Vec<(&str, Vec<EvalValue>)> = vec![
        ("manifest", vec![subj("m1"), date("2026-11-07")]),
        ("price", vec![subj("p1"), qty("5", "USD")]),
        ("note", vec![subj("n1"), subj("x")]),
        ("note", vec![subj("n1"), dec(7)]),
        ("note", vec![subj("n1"), date("2026-11-07")]),
        ("tag", vec![subj("t1"), coll(vec![subj("a"), dec(1)])]),
        ("pay", vec![subj("a1"), qty("5", "GBP")]),
        (
            "price_then_pay",
            vec![subj("p1"), qty("5", "USD"), qty("5", "GBP")],
        ),
        (
            "days",
            vec![
                subj("d1"),
                coll(vec![date("2026-11-07"), date("2026-11-08")]),
            ],
        ),
    ];
    for (name, args) in fits {
        let outcome = run(name, args.clone(), &State::default());
        assert!(
            matches!(outcome, Ok(Outcome::Accepted { .. })),
            "{name} {args:?}: {outcome:?}"
        );
    }
}

#[test]
fn values_the_runtime_gives_fit_their_fields() {
    let p = program();
    let t = p.transformation("stamp").unwrap();
    let transition = transition_at(t, vec![], "alex", "2026-11-07T12:00:00Z");
    let outcome = propose(&p, &transition, &State::default(), &mut fresh()).unwrap();
    let Outcome::Accepted {
        asserted_claims, ..
    } = outcome
    else {
        panic!("a fresh subject and the decision time fit Subject and Timestamp: {outcome:?}");
    };
    assert_eq!(asserted_claims.len(), 1);
}

/// A calendar span is refused before any kind is asked, under `Any` and
/// inside a collection alike: no field may hold one.
#[test]
fn a_calendar_span_stays_refused_where_any_kind_is_declared() {
    for (name, args) in [
        ("note", vec![subj("n1"), cal_span("P1D")]),
        ("tag", vec![subj("t1"), coll(vec![cal_span("P1D")])]),
    ] {
        let err = run(name, args, &State::default()).unwrap_err();
        assert!(
            matches!(&err, EvalError::TypeMismatch(m) if m.contains("calendar span")),
            "{name}: {err:?}"
        );
    }
}

/// The traced run and `explain` meet the same error as the plain run, and
/// the trace never shows the misfitting admit or emit as staged.
#[test]
fn the_traced_run_and_explain_meet_the_same_error() {
    let p = program();
    for (name, args, expected) in misfits() {
        let t = p.transformation(name).unwrap();
        let transition = test_transition(t, args);
        let TracedProposal::Errored { error, trace } =
            propose_with_trace(&p, &transition, &State::default(), &mut fresh())
        else {
            panic!("{name}: the traced run must error");
        };
        assert_eq!(error, expected, "{name}");
        for (staged, values) in staged_values(&trace) {
            let fields = p
                .predicates
                .iter()
                .find(|d| d.name.as_str() == staged)
                .map(|d| &d.args)
                .or_else(|| {
                    p.intents
                        .iter()
                        .find(|d| d.name.as_str() == staged)
                        .map(|d| &d.args)
                })
                .unwrap();
            for (field, value) in fields.iter().zip(&values) {
                assert!(
                    field.kind.holds(&value.kind()),
                    "{name}: the trace shows a misfitting {staged} as staged: {trace:#?}"
                );
            }
        }
        let explained = explain(&p, &transition, &State::default(), &mut fresh());
        let Verdict::Rejected(Rejection::Error(err)) = explained.verdict else {
            panic!("{name}: explain must report the error");
        };
        assert_eq!(err.message, expected.to_string(), "{name}");
    }
}

/// The name and values of every admit and emit a trace shows, loops and
/// branches included.
fn staged_values(trace: &[TraceEntry]) -> Vec<(String, Vec<EvalValue>)> {
    let mut out = vec![];
    for entry in trace {
        match entry {
            TraceEntry::Assert { claim } => {
                out.push((claim.predicate.to_string(), claim.args.clone()))
            }
            TraceEntry::Emit { intent } => out.push((intent.name.to_string(), intent.args.clone())),
            TraceEntry::For { iterations, .. } => {
                for iteration in iterations {
                    out.extend(staged_values(&iteration.trace));
                }
            }
            TraceEntry::ForEach { iterations, .. } => {
                for iteration in iterations {
                    out.extend(staged_values(&iteration.trace));
                }
            }
            TraceEntry::If { trace, .. } => out.extend(staged_values(trace)),
            _ => {}
        }
    }
    out
}

/// A claim stored with another kind, as version 5 could admit, still
/// matches a gate and a retract: the check is on what a proposal
/// introduces, never on what it reads.
#[test]
fn a_stored_claim_of_another_kind_can_still_be_matched_and_retracted() {
    let stored: ClaimInstance = claim_instance("Manifest", &[subj("m1"), subj("s1")]);
    let state = State::from_claims(vec![stored.clone()]);
    let outcome = run("drop_manifest", vec![subj("m1")], &state).unwrap();
    let Outcome::Accepted {
        retracted_claims, ..
    } = outcome
    else {
        panic!("the gate matches and the retract removes it: {outcome:?}");
    };
    assert_eq!(retracted_claims, vec![stored]);
}
