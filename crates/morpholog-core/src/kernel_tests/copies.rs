//! What a decision copies of the state it is judged against. `propose`
//! returns the candidate state of an admitted act, so it builds that one
//! copy and no other: a refused act copies nothing. `decide`, for a
//! caller that advances its own state, copies nothing either way. Both
//! reach the same verdict and the same change. Held by the state's own
//! copy counter, without a clock.

use crate::ir_builder::{
    and, assert_, claim, eq, implies, invariant, params, predicate, program, term, transformation,
    var,
};
use crate::kernel_tests::support::{claim_instance, dec, fresh, subj, test_actor};
use crate::propose::Decided;
use crate::{Outcome, PreparedProgram, Program, State, Transition};

/// A row unique by its key, and one act that admits a row.
fn rows() -> Program {
    program("rows")
        .predicates(vec![
            predicate("Row").subject("row").decimal("version").build(),
        ])
        .invariants(vec![invariant(
            "row_unique_by_row",
            implies(
                and(vec![
                    claim("Row", vec![var("row"), var("a")]),
                    claim("Row", vec![var("row"), var("b")]),
                ]),
                eq(term(var("a")), term(var("b"))),
            ),
        )])
        .transformations(vec![transformation(
            "add_row",
            params(&["row", "version"]),
            vec![assert_("Row", vec![var("row"), var("version")])],
        )])
        .build()
}

fn add_row(row: &str, version: i64) -> Transition {
    Transition {
        transformation_name: "add_row".into(),
        args: vec![subj(row), dec(version)],
        actor: test_actor(),
        decided_at: None,
    }
}

/// State copies taken while `f` runs.
fn copies<T>(f: impl FnOnce() -> T) -> (T, u64) {
    let before = crate::tally::snapshot()[0];
    let result = f();
    (result, crate::tally::snapshot()[0] - before)
}

#[test]
fn a_refused_act_copies_nothing_and_an_admitted_one_copies_once() {
    let prepared = PreparedProgram::new(rows()).expect("validates");
    let state = State::from_claims(vec![claim_instance("Row", &[subj("r1"), dec(1)])]);

    let refused = add_row("r1", 2);
    let (outcome, taken) = copies(|| prepared.propose(&refused, &state, &mut fresh()));
    assert!(
        matches!(outcome, Ok(Some(Outcome::Rejected { .. }))),
        "{outcome:?}"
    );
    assert_eq!(taken, 0, "propose copied the state for a refused act");
    let (decided, taken) = copies(|| prepared.decide(&refused, &state, &mut fresh()));
    assert!(
        matches!(decided, Ok(Some(Decided::Rejected { .. }))),
        "{decided:?}"
    );
    assert_eq!(taken, 0, "decide copied the state for a refused act");

    let admitted = add_row("r2", 1);
    let (outcome, taken) = copies(|| prepared.propose(&admitted, &state, &mut fresh()));
    let Ok(Some(Outcome::Accepted {
        asserted_claims,
        retracted_claims,
        candidate_state,
        ..
    })) = outcome
    else {
        panic!("{outcome:?}");
    };
    assert_eq!(taken, 1, "propose builds the admitted act's candidate once");
    let (decided, taken) = copies(|| prepared.decide(&admitted, &state, &mut fresh()));
    assert_eq!(taken, 0, "decide copied the state for an admitted act");
    let Ok(Some(Decided::Accepted {
        asserted_claims: decided_asserted,
        retracted_claims: decided_retracted,
        ..
    })) = decided
    else {
        panic!("{decided:?}");
    };
    assert_eq!(decided_asserted, asserted_claims);
    assert_eq!(decided_retracted, retracted_claims);
    // The caller's own advance reaches the state propose would have built.
    let mut advanced = state.clone();
    advanced.apply(&decided_asserted, &decided_retracted);
    assert_eq!(advanced, candidate_state);
}
