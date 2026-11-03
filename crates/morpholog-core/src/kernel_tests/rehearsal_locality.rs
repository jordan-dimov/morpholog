//! A rehearsal's decisions cost what they touch, not what the state
//! holds. The fold a rehearsal runs, decide then apply the recorded
//! effect, over a small register revised behind a large live set no rule
//! reads: no candidate state is cloned, and the state is not rebuilt
//! where the old per-row compaction policy would have rebuilt it. The
//! structural property behind the bench's `--noise` ruler, held without a
//! clock.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use crate::ir_builder::{
    and, assert_, claim, eq, implies, invariant, params, predicate, program, retract, term,
    transformation, var, wildcard,
};
use crate::kernel_tests::support::{claim_instance, dec, subj, test_actor};
use crate::propose::Decided;
use crate::{PreparedProgram, Program, State, Transition};

/// A register in the TEC shape: a row predicate unique by its key, a
/// kinds predicate pointed by the key, one act that admits or revises a
/// row, and a predicate no rule reads.
fn register() -> Program {
    program("register")
        .predicates(vec![
            predicate("Row").subject("row").decimal("version").build(),
            predicate("RowKinds").subject("row").subject("kind").build(),
            predicate("Noise").subject("key").decimal("figure").build(),
        ])
        .invariants(vec![
            invariant(
                "row_unique_by_row",
                implies(
                    and(vec![
                        claim("Row", vec![var("row"), var("a")]),
                        claim("Row", vec![var("row"), var("b")]),
                    ]),
                    eq(term(var("a")), term(var("b"))),
                ),
            ),
            invariant(
                "kinds_describe_a_row",
                implies(
                    claim("RowKinds", vec![var("row"), wildcard()]),
                    claim("Row", vec![var("row"), wildcard()]),
                ),
            ),
        ])
        .transformations(vec![
            transformation(
                "admit_row",
                params(&["row", "version", "kind"]),
                vec![
                    retract("Row", vec![var("row"), wildcard()]),
                    retract("RowKinds", vec![var("row"), wildcard()]),
                    assert_("Row", vec![var("row"), var("version")]),
                    assert_("RowKinds", vec![var("row"), var("kind")]),
                ],
            ),
            transformation(
                "note_kind",
                params(&["row", "kind"]),
                vec![assert_("RowKinds", vec![var("row"), var("kind")])],
            ),
        ])
        .build()
}

#[test]
fn a_rehearsal_behind_unrelated_state_clones_nothing_and_rebuilds_nothing() {
    let prepared = PreparedProgram::new(register()).expect("validates");
    // The live set no rule reads: large enough that the old per-row
    // policy, compaction once churn passed about the square root of the
    // live set, would have rebuilt it many times over the run below.
    let noise = 150_000usize;
    let mut state = State::from_claims(
        (0..noise)
            .map(|i| claim_instance("Noise", &[subj(&format!("k{i}")), dec(i as i64)]))
            .collect(),
    );
    let keys = 100usize;
    let revisions = 2_000usize;
    let act = |name: &str, args: Vec<crate::EvalValue>| Transition {
        transformation_name: name.into(),
        args,
        actor: test_actor(),
    };
    let decide = |state: &State, t: &Transition| {
        prepared
            .decide(t, state, &mut crate::kernel_tests::support::fresh())
            .expect("no kernel error")
            .expect("the act is declared")
    };

    let before = crate::tally::snapshot();
    let mut admitted = 0;
    for i in 0..revisions {
        let key = subj(&format!("row_{}", i % keys));
        let t = act("admit_row", vec![key.clone(), dec(i as i64), subj("k")]);
        let Decided::Accepted {
            asserted_claims,
            retracted_claims,
            effective,
            ..
        } = decide(&state, &t)
        else {
            panic!("revision {i} is admissible");
        };
        // The effective delta is the revision and nothing else. A key's
        // first row admits the row and its kind; a revision admits the new
        // version and retracts the previous one, and the kind re-admitted
        // unchanged is no change at all.
        let row = |version: usize| claim_instance("Row", &[key.clone(), dec(version as i64)]);
        if i < keys {
            assert_eq!(
                effective.asserted,
                vec![
                    row(i),
                    claim_instance("RowKinds", &[key.clone(), subj("k")])
                ],
                "revision {i}"
            );
            assert!(effective.retracted.is_empty(), "revision {i}");
        } else {
            assert_eq!(effective.asserted, vec![row(i)], "revision {i}");
            assert_eq!(effective.retracted, vec![row(i - keys)], "revision {i}");
        }
        state.apply(&asserted_claims, &retracted_claims);
        admitted += 1;
    }
    // A kind for a row the register does not hold is refused by the rule
    // that reads both predicates, behind the same noise.
    let Decided::Rejected { reason } =
        decide(&state, &act("note_kind", vec![subj("ghost"), subj("k")]))
    else {
        panic!("a kind without its row is refused");
    };
    assert!(
        reason.to_string().contains("kinds_describe_a_row"),
        "{reason}"
    );
    let after = crate::tally::snapshot();

    assert_eq!(admitted, revisions);
    assert_eq!(state.len(), noise + 2 * keys);
    let [clones, entries_copied, rebuilds, claims_rebuilt] =
        std::array::from_fn(|i| after[i] - before[i]);
    assert_eq!(
        (clones, entries_copied),
        (0, 0),
        "a decision clones no state"
    );
    assert_eq!(
        (rebuilds, claims_rebuilt),
        (0, 0),
        "the state's ordinary threshold is not reached by {revisions} revisions"
    );
}
