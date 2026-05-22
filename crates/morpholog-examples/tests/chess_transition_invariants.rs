//! Integration tests for the chess transition invariants example
//! (`examples/07_chess_transition_invariants/`).
//!
//! The example's reason to exist is to force `Expr::Pre` into the
//! kernel. These tests pin the load-bearing claim: a transition
//! invariant catches a bug that a state invariant cannot.
//!
//! Layers covered: IR-shape sanity, full-chain `propose()` over the
//! initialisation and movement transformations, and a hand-built
//! broken transformation that violates `move_count_strictly_
//! increases` to prove the transition invariants actually gate.

#![allow(clippy::unwrap_used, clippy::expect_used)]

mod common;

use common::{dec, must_accept, propose_with_test_actor, subj};
use morpholog_core::{EvalValue, Outcome, Program, State};
use morpholog_examples::chess_transition_invariants;

// ============================================================
// IR-shape sanity
// ============================================================

#[test]
fn program_validates() {
    let program = chess_transition_invariants::program();
    program
        .validate()
        .expect("chess_transition_invariants must validate cleanly");
}

#[test]
fn program_has_expected_invariant_set() {
    let program = chess_transition_invariants::program();
    let names: Vec<&str> = program.invariants.iter().map(|i| i.name.as_str()).collect();
    assert_eq!(
        names,
        vec![
            "at_most_one_piece_per_square",
            "one_king_per_color",
            "move_count_strictly_increases",
            "turn_alternates",
            "single_capture_per_move",
        ],
    );
}

// ============================================================
// Full-chain propose: start_game and a legal move.
// ============================================================

/// `start_game` on an empty pre-state admits 36 claims and clears
/// every invariant. The transition invariants are vacuously true on
/// the first transition (no pre-state `MoveCount` exists).
#[test]
fn start_game_admits_opening_position() {
    let program = chess_transition_invariants::program();
    let next = run_start_game(&program);
    assert_eq!(
        next.claims().len(),
        35,
        "opening position is 32 pieces + MoveCount + PieceCount + CurrentTurn"
    );
}

/// A legal quiet move on the opening position succeeds. The pawn
/// at e2 moves to e4, MoveCount goes from 0 to 1, CurrentTurn flips
/// from white to black, PieceCount stays at 32.
#[test]
fn quiet_move_after_opening_succeeds() {
    let program = chess_transition_invariants::program();
    let state = run_start_game(&program);

    let next = run_named(
        &program,
        "quiet_move",
        vec![subj("e2"), subj("e4"), subj("black")],
        state,
    );

    // The move count advanced by exactly one - the transition
    // invariant `move_count_strictly_increases` enforced this.
    assert!(
        next.claims()
            .iter()
            .any(|c| { c.predicate == "MoveCount" && c.args == vec![dec(1)] }),
        "MoveCount must be 1 after one move"
    );
    // The turn flipped.
    assert!(
        next.claims()
            .iter()
            .any(|c| { c.predicate == "CurrentTurn" && c.args == vec![subj("black")] }),
        "CurrentTurn must be black after white moves"
    );
    // The pawn relocated.
    assert!(
        next.claims().iter().any(|c| {
            c.predicate == "PieceAt" && c.args == vec![subj("e4"), subj("pawn"), subj("white")]
        }),
        "pawn must be at e4 after the move"
    );
    assert!(
        !next.claims().iter().any(|c| {
            c.predicate == "PieceAt" && c.args == vec![subj("e2"), subj("pawn"), subj("white")]
        }),
        "pawn must no longer be at e2"
    );
}

// ============================================================
// The load-bearing test: pre(...) catches a buggy transformation.
//
// Construct a transformation body that does the right thing
// EVERYWHERE except the MoveCount bump - it forgets to advance the
// counter. Then propose against it. The `move_count_strictly_
// increases` transition invariant must reject the candidate,
// because `MoveCount(0)` after the transition would not equal
// `pre(MoveCount(0)) + 1 = 1`. A state invariant alone could not
// catch this - `MoveCount(0)` is a perfectly admissible
// state. Only the relationship between pre and post falsifies the
// rule.
// ============================================================

#[test]
fn transition_invariant_catches_missing_move_count_bump() {
    use morpholog_core::Transformation;
    use morpholog_core::dsl;

    let mut program = chess_transition_invariants::program();
    let state = run_start_game(&program);

    // A buggy quiet_move that does everything except advance
    // MoveCount. The move count claim is left untouched in the
    // candidate state. The transition invariant must catch this.
    let buggy_move = Transformation {
        name: "buggy_quiet_move".to_string(),
        parameters: dsl::params(&["src", "dst", "new_turn"]),
        body: vec![
            dsl::bind_one(dsl::claim(
                "PieceAt",
                vec![
                    dsl::var("src"),
                    dsl::var("piece_type"),
                    dsl::var("piece_color"),
                ],
            )),
            dsl::bind_one(dsl::claim("CurrentTurn", vec![dsl::var("current_turn")])),
            dsl::require(dsl::eq(
                dsl::term(dsl::var("piece_color")),
                dsl::term(dsl::var("current_turn")),
            )),
            dsl::require(dsl::neq(dsl::var("new_turn"), dsl::var("current_turn"))),
            dsl::retract(
                "PieceAt",
                vec![
                    dsl::var("src"),
                    dsl::var("piece_type"),
                    dsl::var("piece_color"),
                ],
            ),
            dsl::retract("CurrentTurn", vec![dsl::var("current_turn")]),
            dsl::assert_(
                "PieceAt",
                vec![
                    dsl::var("dst"),
                    dsl::var("piece_type"),
                    dsl::var("piece_color"),
                ],
            ),
            dsl::assert_("CurrentTurn", vec![dsl::var("new_turn")]),
            // Conspicuously missing: the MoveCount retract + assert.
            dsl::emit("PieceMoved", vec![dsl::var("src"), dsl::var("dst")]),
        ],
    };
    program.transformations.push(buggy_move);

    let buggy = program
        .transformation("buggy_quiet_move")
        .expect("just pushed");
    let outcome = propose_with_test_actor(
        buggy,
        vec![subj("e2"), subj("e4"), subj("black")],
        &state,
        &program.invariants,
    )
    .expect("kernel must not error");

    match outcome {
        Outcome::Rejected { reason } => {
            assert!(
                reason.contains("move_count_strictly_increases"),
                "expected rejection to name the transition invariant, got: {reason}"
            );
        }
        Outcome::Accepted { .. } => {
            panic!(
                "a buggy quiet move that fails to bump MoveCount must be rejected by the transition invariant"
            )
        }
    }
}

// ============================================================
// Helpers
// ============================================================

/// Run the example's `start_game` against the empty state. Returns
/// the resulting candidate.
fn run_start_game(program: &Program) -> State {
    run_named(program, "start_game", vec![], State::default())
}

/// Look up `transformation_name` in `program`, propose it with the
/// supplied args against `state`, and `must_accept` the result.
fn run_named(
    program: &Program,
    transformation_name: &str,
    args: Vec<EvalValue>,
    state: State,
) -> State {
    let t = program
        .transformation(transformation_name)
        .unwrap_or_else(|| panic!("transformation `{transformation_name}` not found"));
    must_accept(t, args, state, &program.invariants)
}
