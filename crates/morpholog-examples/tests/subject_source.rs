//! Which `new Subject()` gets which subject. The source is an input to
//! execution, so its consumption order is part of the semantics: one subject
//! is drawn each time execution reaches `new Subject()`, in statement order,
//! a `for` body once per element in collection order, nested bodies
//! depth-first, and nothing after execution stops. A recorded sequence
//! replays only if this order holds.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use morpholog_core::{EvalError, Outcome, State, Subject, TraceEntry, propose_with_trace};
use morpholog_surface::parse_program;
use morpholog_test_support::{claim_instance, coll, subj, test_transition};

const DRAWS: &str = "
program draws
predicate Top(s: Subject)
predicate Outer(item: Subject, s: Subject)
predicate Inner(item: Subject, inner: Subject, s: Subject)
predicate Never(s: Subject)

transformation mint(items, inners):
    let a = new Subject()
    admit Top(a)
    for i in items:
        let b = new Subject()
        admit Outer(i, b)
        for j in inners:
            let c = new Subject()
            admit Inner(i, j, c)

transformation stops(x):
    let a = new Subject()
    require Never(a)
    let b = new Subject()
    admit Top(b)
";

fn sequence(n: usize) -> std::vec::IntoIter<Subject> {
    (0..n)
        .map(|i| Subject::from(format!("s{i}")))
        .collect::<Vec<_>>()
        .into_iter()
}

fn drawn_in_trace(entries: &[TraceEntry], out: &mut Vec<String>) {
    for e in entries {
        match e {
            TraceEntry::LetNewSubject { subject, .. } => out.push(format!("{subject:?}")),
            TraceEntry::For { iterations, .. } => {
                for iteration in iterations {
                    drawn_in_trace(&iteration.trace, out);
                }
            }
            _ => {}
        }
    }
}

#[test]
fn subjects_are_drawn_in_statement_and_collection_order_depth_first() {
    let p = parse_program(DRAWS).unwrap();
    let t = p.transformation("mint").unwrap();
    let transition = test_transition(
        t,
        vec![
            coll(vec![subj("i1"), subj("i2")]),
            coll(vec![subj("j1"), subj("j2")]),
        ],
    );
    let traced = propose_with_trace(
        t,
        &transition,
        &State::default(),
        &p.invariants,
        &p.definitions,
        &mut sequence(7),
    );
    let morpholog_core::TracedProposal::Completed {
        outcome: Outcome::Accepted {
            asserted_claims, ..
        },
        trace,
    } = traced
    else {
        panic!("minting is unconditional: {traced:?}");
    };
    let expected = [
        claim_instance("Top", &[subj("s0")]),
        claim_instance("Outer", &[subj("i1"), subj("s1")]),
        claim_instance("Inner", &[subj("i1"), subj("j1"), subj("s2")]),
        claim_instance("Inner", &[subj("i1"), subj("j2"), subj("s3")]),
        claim_instance("Outer", &[subj("i2"), subj("s4")]),
        claim_instance("Inner", &[subj("i2"), subj("j1"), subj("s5")]),
        claim_instance("Inner", &[subj("i2"), subj("j2"), subj("s6")]),
    ];
    for claim in &expected {
        assert!(
            asserted_claims.contains(claim),
            "{claim:?} in {asserted_claims:?}"
        );
    }
    assert_eq!(asserted_claims.len(), expected.len());
    let mut drawn = Vec::new();
    drawn_in_trace(&trace, &mut drawn);
    let in_order: Vec<String> = (0..7)
        .map(|i| {
            format!(
                "{:?}",
                morpholog_core::EvalValue::Subject(Subject::from(format!("s{i}")))
            )
        })
        .collect();
    assert_eq!(drawn, in_order, "the trace records the draws in order");
}

/// A source shorter than the execution's draws is the kernel's typed error,
/// never a panic: a replayed sequence cut short is refused by name.
#[test]
fn a_source_that_runs_out_is_a_typed_error() {
    let p = parse_program(DRAWS).unwrap();
    let t = p.transformation("mint").unwrap();
    let transition = test_transition(
        t,
        vec![
            coll(vec![subj("i1"), subj("i2")]),
            coll(vec![subj("j1"), subj("j2")]),
        ],
    );
    let result = morpholog_core::propose(
        t,
        &transition,
        &State::default(),
        &p.invariants,
        &p.definitions,
        &mut sequence(3),
    );
    assert!(
        matches!(result, Err(EvalError::SubjectSourceExhausted)),
        "{result:?}"
    );
}

/// Execution that stops draws nothing after the point it stopped.
#[test]
fn a_refusal_draws_nothing_after_it_stops() {
    let p = parse_program(DRAWS).unwrap();
    let t = p.transformation("stops").unwrap();
    let mut source = sequence(3);
    let result = morpholog_core::propose(
        t,
        &test_transition(t, vec![subj("x")]),
        &State::default(),
        &p.invariants,
        &p.definitions,
        &mut source,
    );
    assert!(matches!(result, Ok(Outcome::Rejected { .. })), "{result:?}");
    assert_eq!(
        source.next(),
        Some(Subject::from("s1")),
        "only the first was drawn"
    );
}
