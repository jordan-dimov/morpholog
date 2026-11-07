//! An act's `for` over a collection, held to one observable record across
//! any change in how the loop keeps its bindings: what each case admits,
//! refuses or errors on, the same with and without a trace, and the trace
//! itself against a golden. The cases are the ones a cheaper binding copy
//! could break: nested loops, the loop's own collection read inside it, a
//! name each iteration binds afresh, a refusal partway through, and an
//! error partway through. `UPDATE_GOLDENS=1` regenerates the golden.

#![allow(clippy::unwrap_used, clippy::expect_used)]

mod common;

use std::path::PathBuf;

use common::{coll, dec, subj};
use morpholog_core::{
    ClaimInstance, EvalValue, Outcome, PreparedProgram, State, TracedProposal, Transition,
};
use morpholog_surface::parse_program;

const PROGRAMME: &str = "program loop_bindings

predicate Node(x: Subject, v: Decimal)
predicate Pair(x: Subject, y: Subject)
predicate Seen(x: Subject, v: Decimal)
predicate Count(x: Subject, n: Decimal)
predicate Ratio(x: Subject, r: Decimal)

transformation pairs(xs, ys):
    for x in xs:
        for y in ys:
            admit Pair(x, y)

transformation counts(xs):
    for x in xs:
        require x in xs
        let n = sum(1 | y in xs)
        admit Count(x, n)

transformation mark(xs):
    for x in xs:
        bind Node(x, v)
        admit Seen(x, v)

transformation below(xs, limit):
    for x in xs:
        bind Node(x, v)
        require v < limit
        admit Seen(x, v)

transformation ratios(xs):
    for x in xs:
        bind Node(x, v)
        let r = 100 / v
        admit Ratio(x, r)
";

fn claim(predicate: &str, args: Vec<EvalValue>) -> ClaimInstance {
    ClaimInstance {
        predicate: predicate.into(),
        args,
    }
}

fn subjects(names: &[&str]) -> EvalValue {
    coll(names.iter().map(|n| subj(n)).collect())
}

/// Nodes a, b, c, d with values 1, 2, 3 and 0.
fn nodes() -> State {
    State::from_claims(
        [("a", 1), ("b", 2), ("c", 3), ("d", 0)]
            .into_iter()
            .map(|(x, v)| claim("Node", vec![subj(x), dec(v)]))
            .collect(),
    )
}

/// Each case: a name, the act and its arguments.
fn cases() -> Vec<(&'static str, &'static str, Vec<EvalValue>)> {
    vec![
        (
            "nested",
            "pairs",
            vec![subjects(&["a", "b"]), subjects(&["p", "q", "r"])],
        ),
        (
            "own collection inside",
            "counts",
            vec![subjects(&["a", "b", "c"])],
        ),
        (
            "a name bound afresh each iteration",
            "mark",
            vec![subjects(&["a", "b", "c"])],
        ),
        (
            "refused partway",
            "below",
            vec![subjects(&["a", "b", "c"]), dec(3)],
        ),
        (
            "no claim to bind partway",
            "mark",
            vec![subjects(&["a", "z", "b"])],
        ),
        (
            "an error partway",
            "ratios",
            vec![subjects(&["a", "d", "b"])],
        ),
    ]
}

/// What a case did, in words: the claims admitted in order, the
/// refusal, or the error.
fn summary(outcome: &Result<Outcome, String>) -> String {
    match outcome {
        Ok(Outcome::Accepted {
            asserted_claims, ..
        }) => format!("accepted {asserted_claims:?}"),
        Ok(Outcome::Rejected { reason }) => format!("rejected: {reason}"),
        Err(error) => format!("error: {error}"),
    }
}

#[test]
fn a_loop_keeps_its_bindings_as_recorded() {
    let program = parse_program(PROGRAMME).expect("parses");
    let prepared = PreparedProgram::new(program).expect("validates");
    let pre = nodes();
    let mut record = serde_json::Map::new();
    for (name, act, args) in cases() {
        let transition = Transition {
            transformation_name: act.into(),
            args,
            actor: "tester".into(),
            decided_at: None,
        };
        let plain = prepared
            .propose(&transition, &pre, &mut std::iter::empty())
            .map(|o| o.expect("the act is declared"))
            .map_err(|e| e.to_string());
        let (traced, trace) = match prepared
            .propose_with_trace(&transition, &pre, &mut std::iter::empty())
            .expect("the act is declared")
        {
            TracedProposal::Completed { outcome, trace } => (Ok(outcome), trace),
            TracedProposal::Errored { error, trace } => (Err(error.to_string()), trace),
        };
        assert_eq!(plain, traced, "{name}: traced and untraced part");
        record.insert(
            name.to_string(),
            serde_json::json!({ "outcome": summary(&plain), "trace": trace }),
        );
    }

    // The record in words, so a reader sees what is pinned.
    let outcomes: Vec<(&str, String)> = record
        .iter()
        .map(|(k, v)| (k.as_str(), v["outcome"].as_str().unwrap().to_string()))
        .collect();
    let says = |case: &str, starts: &str| {
        let (_, outcome) = outcomes.iter().find(|(k, _)| *k == case).unwrap();
        assert!(outcome.starts_with(starts), "{case}: {outcome}");
    };
    says("nested", "accepted");
    says("own collection inside", "accepted");
    says("a name bound afresh each iteration", "accepted");
    says("refused partway", "rejected");
    says("no claim to bind partway", "rejected");
    says("an error partway", "error");

    let path: PathBuf = [
        env!("CARGO_MANIFEST_DIR"),
        "tests",
        "golden",
        "loop_bindings.json",
    ]
    .iter()
    .collect();
    let now = serde_json::to_string_pretty(&record).unwrap() + "\n";
    if std::env::var_os("UPDATE_GOLDENS").is_some() {
        std::fs::write(&path, &now).unwrap();
        return;
    }
    let golden =
        std::fs::read_to_string(&path).expect("no golden; generate it with UPDATE_GOLDENS=1");
    assert_eq!(now, golden, "the loop's record moved");
}
