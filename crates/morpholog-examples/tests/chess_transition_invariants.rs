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
            "exactly_one_white_king",
            "exactly_one_black_king",
            "piece_count_matches_board",
            "board_with_pieces_has_a_counter",
            "at_most_eight_pawns_per_color",
            "move_count_strictly_increases",
            "turn_alternates",
            "single_capture_per_move",
        ],
    );
}

/// Capturing a king is now structurally impossible. `exactly_one_
/// black_king` pins the count of black kings to one, so a move that
/// removes the last black king (count -> 0) is rejected - something the
/// old at-most-one rule, which only forbade duplicates, could not do.
#[test]
fn capturing_a_king_is_rejected() {
    use common::claim_instance;

    // A minimal mid-game position: a white rook on a1 poised to take
    // the black king on a8, both kings present, the counter consistent
    // with the four pieces on the board.
    let pre = State::from_claims(vec![
        claim_instance("PieceAt", &[subj("a1"), subj("rook"), subj("white")]),
        claim_instance("PieceAt", &[subj("e1"), subj("king"), subj("white")]),
        claim_instance("PieceAt", &[subj("a8"), subj("king"), subj("black")]),
        claim_instance("PieceAt", &[subj("h8"), subj("rook"), subj("black")]),
        claim_instance("CurrentTurn", &[subj("white")]),
        claim_instance("MoveCount", &[dec(10)]),
        claim_instance("PieceCount", &[dec(4)]),
    ]);

    let program = chess_transition_invariants::program();
    let capturing = program.transformation("capturing_move").expect("exists");
    let outcome = propose_with_test_actor(
        capturing,
        vec![subj("a1"), subj("a8"), subj("black")],
        &pre,
        &program.invariants,
    )
    .expect("kernel must not error");

    match outcome {
        Outcome::Rejected { reason } => assert!(
            reason.contains("exactly_one_black_king"),
            "expected the king-count invariant to reject the capture, got: {reason}"
        ),
        Outcome::Accepted { .. } => {
            panic!("capturing a king must be rejected: a colour always has exactly one king")
        }
    }
}

/// The census invariant `piece_count_matches_board` has teeth: a move
/// that leaves a stray piece on the board without updating `PieceCount`
/// is rejected, because the hand-maintained counter no longer equals
/// `sum(1 | PieceAt(...))`.
#[test]
fn piece_count_drift_is_rejected() {
    use morpholog_core::Transformation;
    use morpholog_core::dsl;

    let mut program = chess_transition_invariants::program();
    let state = run_start_game(&program);

    // A knight move that admits the piece at the destination but forgets
    // to retract it from the source - so the board gains a piece while
    // PieceCount stays at 32. MoveCount and CurrentTurn are handled
    // correctly, so the census invariant is the one that must fire.
    let drifting_move = Transformation {
        name: "drifting_move".to_string(),
        parameters: dsl::params(&["src", "dst", "new_turn"]),
        body: vec![
            dsl::bind_one(dsl::claim(
                "PieceAt",
                vec![dsl::var("src"), dsl::var("pt"), dsl::var("pc")],
            )),
            dsl::bind_one(dsl::claim("CurrentTurn", vec![dsl::var("turn")])),
            dsl::bind_one(dsl::claim("MoveCount", vec![dsl::var("m")])),
            dsl::require(dsl::neq(dsl::var("new_turn"), dsl::var("turn"))),
            dsl::let_(
                "next_m",
                dsl::add(dsl::term(dsl::var("m")), dsl::term(dsl::dec("1"))),
            ),
            dsl::retract("CurrentTurn", vec![dsl::var("turn")]),
            dsl::retract("MoveCount", vec![dsl::var("m")]),
            // Conspicuously missing: retract of the piece at `src`.
            dsl::assert_(
                "PieceAt",
                vec![dsl::var("dst"), dsl::var("pt"), dsl::var("pc")],
            ),
            dsl::assert_("CurrentTurn", vec![dsl::var("new_turn")]),
            dsl::assert_("MoveCount", vec![dsl::var("next_m")]),
        ],
    };
    program.transformations.push(drifting_move);
    let drifting = program
        .transformation("drifting_move")
        .expect("just pushed");

    // Knight b1 -> c3 (c3 is empty in the opening); the knight ends up
    // on both squares.
    let outcome = propose_with_test_actor(
        drifting,
        vec![subj("b1"), subj("c3"), subj("black")],
        &state,
        &program.invariants,
    )
    .expect("kernel must not error");

    match outcome {
        Outcome::Rejected { reason } => assert!(
            reason.contains("piece_count_matches_board"),
            "expected the census invariant to reject the drift, got: {reason}"
        ),
        Outcome::Accepted { .. } => {
            panic!("a move that adds a piece without updating PieceCount must be rejected")
        }
    }
}

/// Dropping the counter entirely is also caught. `piece_count_matches_
/// board` is vacuous with no `PieceCount` present, but `board_with_
/// pieces_has_a_counter` requires a non-empty board to carry a counter,
/// so a move that retracts `PieceCount` without re-admitting it is
/// refused.
#[test]
fn dropping_the_piece_counter_is_rejected() {
    use morpholog_core::Transformation;
    use morpholog_core::dsl;

    let mut program = chess_transition_invariants::program();
    let state = run_start_game(&program);

    let counterless_move = Transformation {
        name: "counterless_move".to_string(),
        parameters: dsl::params(&["new_turn"]),
        body: vec![
            dsl::bind_one(dsl::claim("CurrentTurn", vec![dsl::var("turn")])),
            dsl::bind_one(dsl::claim("MoveCount", vec![dsl::var("m")])),
            dsl::bind_one(dsl::claim("PieceCount", vec![dsl::var("p")])),
            dsl::require(dsl::neq(dsl::var("new_turn"), dsl::var("turn"))),
            dsl::let_(
                "next_m",
                dsl::add(dsl::term(dsl::var("m")), dsl::term(dsl::dec("1"))),
            ),
            dsl::retract("CurrentTurn", vec![dsl::var("turn")]),
            dsl::retract("MoveCount", vec![dsl::var("m")]),
            dsl::retract("PieceCount", vec![dsl::var("p")]),
            // Conspicuously missing: re-admit of PieceCount.
            dsl::assert_("CurrentTurn", vec![dsl::var("new_turn")]),
            dsl::assert_("MoveCount", vec![dsl::var("next_m")]),
        ],
    };
    program.transformations.push(counterless_move);
    let counterless = program
        .transformation("counterless_move")
        .expect("just pushed");

    let outcome = propose_with_test_actor(
        counterless,
        vec![subj("black")],
        &state,
        &program.invariants,
    )
    .expect("kernel must not error");

    match outcome {
        Outcome::Rejected { reason } => assert!(
            reason.contains("board_with_pieces_has_a_counter"),
            "expected the presence invariant to reject the dropped counter, got: {reason}"
        ),
        Outcome::Accepted { .. } => {
            panic!("dropping PieceCount on a non-empty board must be rejected")
        }
    }
}

// ============================================================
// Full-chain propose: start_game and a legal move.
// ============================================================

/// `start_game` on an empty pre-state admits 35 claims (32 pieces
/// plus `MoveCount`, `PieceCount`, `CurrentTurn`) and clears every
/// invariant. The transition invariants are vacuously true on the
/// first transition (no pre-state `MoveCount` exists).
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
