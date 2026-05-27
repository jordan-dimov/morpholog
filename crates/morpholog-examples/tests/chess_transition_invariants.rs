//! Integration tests for the chess transition invariants example
//! (`examples/07_chess_transition_invariants/`).
//!
//! The example's first reason to exist is to force `Prop::Pre` into the
//! kernel. These tests pin the load-bearing claim: a transition
//! invariant catches a bug that a state invariant cannot. It is also the
//! forcing home for `ArithOp::Mod`: squares are `(file, rank)`
//! coordinates and a square's colour is `(file + rank) % 2`, which the
//! `bishops_on_opposite_square_colors` invariant uses.
//!
//! Layers covered: IR-shape sanity, full-chain `propose()` over the
//! initialisation and movement transformations, the parity invariant
//! (a bishop changing square colour is rejected, keeping it is allowed),
//! and a hand-built broken transformation that violates `move_count_
//! strictly_increases` to prove the transition invariants actually gate.

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
            "bishops_on_opposite_square_colors",
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

    // A minimal mid-game position: a white rook on a1 (file 1, rank 1)
    // poised to take the black king on a8 (file 1, rank 8), both kings
    // present, the counter consistent with the four pieces on the board.
    let pre = State::from_claims(vec![
        claim_instance("PieceAt", &[dec(1), dec(1), subj("rook"), subj("white")]),
        claim_instance("PieceAt", &[dec(5), dec(1), subj("king"), subj("white")]),
        claim_instance("PieceAt", &[dec(1), dec(8), subj("king"), subj("black")]),
        claim_instance("PieceAt", &[dec(8), dec(8), subj("rook"), subj("black")]),
        claim_instance("CurrentTurn", &[subj("white")]),
        claim_instance("MoveCount", &[dec(10)]),
        claim_instance("PieceCount", &[dec(4)]),
    ]);

    let program = chess_transition_invariants::program();
    let capturing = program.transformation("capturing_move").expect("exists");
    let outcome = propose_with_test_actor(
        capturing,
        vec![dec(1), dec(1), dec(1), dec(8), subj("black")],
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
    // Adversarial (IR-builder) test: constructs the real transition minus
    // one statement to prove an invariant has teeth - a kernel-teeth test,
    // not a business story, so the Rust IR builder is the right tool here,
    // not `.morph`.
    use morpholog_core::ir_builder;

    let mut program = chess_transition_invariants::program();
    let state = run_start_game(&program);

    // A knight move that admits the piece at the destination but forgets
    // to retract it from the source - so the board gains a piece while
    // PieceCount stays at 32. MoveCount and CurrentTurn are handled
    // correctly, so the census invariant is the one that must fire.
    let drifting_move = ir_builder::transformation(
        "drifting_move",
        ir_builder::params(&["src_f", "src_r", "dst_f", "dst_r", "new_turn"]),
        vec![
            ir_builder::bind_one(ir_builder::claim(
                "PieceAt",
                vec![
                    ir_builder::var("src_f"),
                    ir_builder::var("src_r"),
                    ir_builder::var("pt"),
                    ir_builder::var("pc"),
                ],
            )),
            ir_builder::bind_one(ir_builder::claim(
                "CurrentTurn",
                vec![ir_builder::var("turn")],
            )),
            ir_builder::bind_one(ir_builder::claim("MoveCount", vec![ir_builder::var("m")])),
            ir_builder::require(ir_builder::neq(
                ir_builder::var("new_turn"),
                ir_builder::var("turn"),
            )),
            ir_builder::let_(
                "next_m",
                ir_builder::add(
                    ir_builder::term(ir_builder::var("m")),
                    ir_builder::term(ir_builder::dec("1")),
                ),
            ),
            ir_builder::retract("CurrentTurn", vec![ir_builder::var("turn")]),
            ir_builder::retract("MoveCount", vec![ir_builder::var("m")]),
            // Conspicuously missing: retract of the piece at the source square.
            ir_builder::assert_(
                "PieceAt",
                vec![
                    ir_builder::var("dst_f"),
                    ir_builder::var("dst_r"),
                    ir_builder::var("pt"),
                    ir_builder::var("pc"),
                ],
            ),
            ir_builder::assert_("CurrentTurn", vec![ir_builder::var("new_turn")]),
            ir_builder::assert_("MoveCount", vec![ir_builder::var("next_m")]),
        ],
    );
    program.transformations.push(drifting_move);
    let drifting = program
        .transformation("drifting_move")
        .expect("just pushed");

    // Knight b1 -> c3 (file 2 rank 1 -> file 3 rank 3; c3 is empty in the
    // opening); the knight ends up on both squares.
    let outcome = propose_with_test_actor(
        drifting,
        vec![dec(2), dec(1), dec(3), dec(3), subj("black")],
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
    // Adversarial (IR-builder) test: constructs the real transition minus
    // one statement to prove an invariant has teeth - a kernel-teeth test,
    // not a business story, so the Rust IR builder is the right tool here,
    // not `.morph`.
    use morpholog_core::ir_builder;

    let mut program = chess_transition_invariants::program();
    let state = run_start_game(&program);

    let counterless_move = ir_builder::transformation(
        "counterless_move",
        ir_builder::params(&["new_turn"]),
        vec![
            ir_builder::bind_one(ir_builder::claim(
                "CurrentTurn",
                vec![ir_builder::var("turn")],
            )),
            ir_builder::bind_one(ir_builder::claim("MoveCount", vec![ir_builder::var("m")])),
            ir_builder::bind_one(ir_builder::claim("PieceCount", vec![ir_builder::var("p")])),
            ir_builder::require(ir_builder::neq(
                ir_builder::var("new_turn"),
                ir_builder::var("turn"),
            )),
            ir_builder::let_(
                "next_m",
                ir_builder::add(
                    ir_builder::term(ir_builder::var("m")),
                    ir_builder::term(ir_builder::dec("1")),
                ),
            ),
            ir_builder::retract("CurrentTurn", vec![ir_builder::var("turn")]),
            ir_builder::retract("MoveCount", vec![ir_builder::var("m")]),
            ir_builder::retract("PieceCount", vec![ir_builder::var("p")]),
            // Conspicuously missing: re-admit of PieceCount.
            ir_builder::assert_("CurrentTurn", vec![ir_builder::var("new_turn")]),
            ir_builder::assert_("MoveCount", vec![ir_builder::var("next_m")]),
        ],
    );
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
        vec![dec(5), dec(2), dec(5), dec(4), subj("black")],
        state,
    );

    // The move count advanced by exactly one - the transition
    // invariant `move_count_strictly_increases` enforced this.
    assert!(
        next.claims()
            .iter()
            .any(|c| { c.predicate.as_str() == "MoveCount" && c.args == vec![dec(1)] }),
        "MoveCount must be 1 after one move"
    );
    // The turn flipped.
    assert!(
        next.claims()
            .iter()
            .any(|c| { c.predicate.as_str() == "CurrentTurn" && c.args == vec![subj("black")] }),
        "CurrentTurn must be black after white moves"
    );
    // The pawn relocated from e2 (file 5, rank 2) to e4 (file 5, rank 4).
    assert!(
        next.claims().iter().any(|c| {
            c.predicate.as_str() == "PieceAt"
                && c.args == vec![dec(5), dec(4), subj("pawn"), subj("white")]
        }),
        "pawn must be at e4 after the move"
    );
    assert!(
        !next.claims().iter().any(|c| {
            c.predicate.as_str() == "PieceAt"
                && c.args == vec![dec(5), dec(2), subj("pawn"), subj("white")]
        }),
        "pawn must no longer be at e2"
    );
}

// ============================================================
// Square colour: the `(file + rank) % 2` parity invariant.
// ============================================================

/// A bishop that changes square colour is rejected once it would put
/// both bishops of a colour on the same colour. White's dark-squared
/// bishop starts on c1 (file 3, rank 1; `(3+1) % 2 = 0`, dark). Sliding
/// it to d3 (file 4, rank 3; `(4+3) % 2 = 1`, light) would join white's
/// other bishop on f1 (file 6, rank 1; `(6+1) % 2 = 1`, light) - two
/// light-squared white bishops - and `bishops_on_opposite_square_colors`
/// turns the move away. This is the parity arithmetic doing the work.
#[test]
fn bishop_changing_square_color_is_rejected() {
    let program = chess_transition_invariants::program();
    let state = run_start_game(&program);

    let bishop = program.transformation("quiet_move").expect("exists");
    let outcome = propose_with_test_actor(
        bishop,
        vec![dec(3), dec(1), dec(4), dec(3), subj("black")],
        &state,
        &program.invariants,
    )
    .expect("kernel must not error");

    match outcome {
        Outcome::Rejected { reason } => assert!(
            reason.contains("bishops_on_opposite_square_colors"),
            "expected the parity invariant to reject the colour change, got: {reason}"
        ),
        Outcome::Accepted { .. } => {
            panic!("a bishop landing on its partner's square colour must be rejected")
        }
    }
}

/// The same bishop sliding to another square of its *own* colour is
/// allowed. c1 (dark, parity 0) to e3 (file 5, rank 3; `(5+3) % 2 = 0`,
/// dark) keeps the two white bishops on opposite colours, so the parity
/// invariant is satisfied and the move commits.
#[test]
fn bishop_keeping_square_color_is_allowed() {
    let program = chess_transition_invariants::program();
    let state = run_start_game(&program);

    let next = run_named(
        &program,
        "quiet_move",
        vec![dec(3), dec(1), dec(5), dec(3), subj("black")],
        state,
    );

    assert!(
        next.claims().iter().any(|c| {
            c.predicate.as_str() == "PieceAt"
                && c.args == vec![dec(5), dec(3), subj("bishop"), subj("white")]
        }),
        "bishop must be at e3 after a same-colour move"
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
    // Adversarial (IR-builder) test: constructs the real transition minus
    // one statement to prove an invariant has teeth - a kernel-teeth test,
    // not a business story, so the Rust IR builder is the right tool here,
    // not `.morph`.
    use morpholog_core::ir_builder;

    let mut program = chess_transition_invariants::program();
    let state = run_start_game(&program);

    // A buggy quiet_move that does everything except advance
    // MoveCount. The move count claim is left untouched in the
    // candidate state. The transition invariant must catch this.
    let buggy_move = ir_builder::transformation(
        "buggy_quiet_move",
        ir_builder::params(&["src_f", "src_r", "dst_f", "dst_r", "new_turn"]),
        vec![
            ir_builder::bind_one(ir_builder::claim(
                "PieceAt",
                vec![
                    ir_builder::var("src_f"),
                    ir_builder::var("src_r"),
                    ir_builder::var("piece_type"),
                    ir_builder::var("piece_color"),
                ],
            )),
            ir_builder::bind_one(ir_builder::claim(
                "CurrentTurn",
                vec![ir_builder::var("current_turn")],
            )),
            ir_builder::require(ir_builder::eq(
                ir_builder::term(ir_builder::var("piece_color")),
                ir_builder::term(ir_builder::var("current_turn")),
            )),
            ir_builder::require(ir_builder::neq(
                ir_builder::var("new_turn"),
                ir_builder::var("current_turn"),
            )),
            ir_builder::retract(
                "PieceAt",
                vec![
                    ir_builder::var("src_f"),
                    ir_builder::var("src_r"),
                    ir_builder::var("piece_type"),
                    ir_builder::var("piece_color"),
                ],
            ),
            ir_builder::retract("CurrentTurn", vec![ir_builder::var("current_turn")]),
            ir_builder::assert_(
                "PieceAt",
                vec![
                    ir_builder::var("dst_f"),
                    ir_builder::var("dst_r"),
                    ir_builder::var("piece_type"),
                    ir_builder::var("piece_color"),
                ],
            ),
            ir_builder::assert_("CurrentTurn", vec![ir_builder::var("new_turn")]),
            // Conspicuously missing: the MoveCount retract + assert.
            ir_builder::emit(
                "PieceMoved",
                vec![
                    ir_builder::var("src_f"),
                    ir_builder::var("src_r"),
                    ir_builder::var("dst_f"),
                    ir_builder::var("dst_r"),
                ],
            ),
        ],
    );
    program.transformations.push(buggy_move);

    let buggy = program
        .transformation("buggy_quiet_move")
        .expect("just pushed");
    let outcome = propose_with_test_actor(
        buggy,
        vec![dec(5), dec(2), dec(5), dec(4), subj("black")],
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
