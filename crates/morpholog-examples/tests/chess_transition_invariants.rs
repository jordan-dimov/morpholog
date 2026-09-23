//! Integration tests for the chess transition invariants example
//! (`examples/07_chess_transition_invariants/`).
//!
//! The key claim: a transition invariant (one using `pre(...)`) catches a
//! bug that a state invariant cannot. Also covers the census invariants and
//! square colour, `(file + rank) % 2`, used by
//! `bishops_on_opposite_square_colors`.

#![allow(clippy::unwrap_used, clippy::expect_used)]

mod common;

use common::{Example, dec, subj};
use morpholog_core::{EvalValue, Outcome, Program, State};
use morpholog_examples::chess_transition_invariants;
use std::sync::OnceLock;

fn ex() -> &'static Example {
    static EX: OnceLock<Example> = OnceLock::new();
    EX.get_or_init(|| Example::new(&chess_transition_invariants::program()))
}

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
            "piece_at_unique_by_file_rank",
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

/// Capturing a king is impossible: `exactly_one_black_king` pins the count
/// of black kings to one, so a move that takes the last one is rejected.
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
    let reason = ex().must_reject(
        capturing,
        vec![dec(1), dec(1), dec(1), dec(8), subj("black")],
        &pre,
    );
    assert!(
        reason.to_string().contains("exactly_one_black_king"),
        "expected the king-count invariant to reject the capture, got: {reason}"
    );
}

/// `piece_count_matches_board` rejects a move that leaves a stray piece on
/// the board, because `PieceCount` no longer equals `sum(1 | PieceAt(...))`.
#[test]
fn piece_count_drift_is_rejected() {
    // Built in Rust IR: the real transition minus one statement, to prove
    // the invariant has teeth.
    use morpholog_core::ir_builder;

    let mut program = chess_transition_invariants::program();
    let state = run_start_game(&program);

    // A knight move that forgets to retract the piece from its source. The
    // board gains a piece while PieceCount stays at 32, so only the census
    // invariant can fire.
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
    let reason = ex().must_reject(
        drifting,
        vec![dec(2), dec(1), dec(3), dec(3), subj("black")],
        &state,
    );
    assert!(
        reason.to_string().contains("piece_count_matches_board"),
        "expected the census invariant to reject the drift, got: {reason}"
    );
}

/// Dropping the counter is also caught. `piece_count_matches_board` is
/// vacuous with no `PieceCount`, so `board_with_pieces_has_a_counter`
/// requires a non-empty board to carry one.
#[test]
fn dropping_the_piece_counter_is_rejected() {
    // Built in Rust IR: the real transition minus one statement, to prove
    // the invariant has teeth.
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

    let reason = ex().must_reject(counterless, vec![subj("black")], &state);
    assert!(
        reason
            .to_string()
            .contains("board_with_pieces_has_a_counter"),
        "expected the presence invariant to reject the dropped counter, got: {reason}"
    );
}

// ============================================================
// Full-chain propose: start_game and a legal move.
// ============================================================

/// `start_game` on an empty state admits 35 claims (32 pieces plus
/// `MoveCount`, `PieceCount`, `CurrentTurn`). The transition invariants
/// hold vacuously: there is no earlier `MoveCount`.
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

    // Enforced by `move_count_strictly_increases`.
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

/// A bishop may not end up on the same colour as its partner. c1
/// (`(3+1) % 2 = 0`, dark) to d3 (`(4+3) % 2 = 1`, light) would join the
/// f1 bishop (`(6+1) % 2 = 1`, light), so
/// `bishops_on_opposite_square_colors` rejects it.
#[test]
fn bishop_changing_square_color_is_rejected() {
    let program = chess_transition_invariants::program();
    let state = run_start_game(&program);

    let bishop = program.transformation("quiet_move").expect("exists");
    let reason = ex().must_reject(
        bishop,
        vec![dec(3), dec(1), dec(4), dec(3), subj("black")],
        &state,
    );
    assert!(
        reason
            .to_string()
            .contains("bishops_on_opposite_square_colors"),
        "expected the parity invariant to reject the colour change, got: {reason}"
    );
}

/// The same bishop may move within its own colour: c1 to e3
/// (`(5+3) % 2 = 0`, dark) keeps the bishops on opposite colours.
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
// pre(...) catches a buggy transformation.
//
// A move that forgets to advance MoveCount. `MoveCount(0)` is a valid
// state on its own; only comparing it with `pre(MoveCount(0)) + 1`
// shows the bug, so `move_count_strictly_increases` must reject it.
// ============================================================

#[test]
fn transition_invariant_catches_missing_move_count_bump() {
    // Built in Rust IR: the real transition minus one statement, to prove
    // the invariant has teeth.
    use morpholog_core::ir_builder;

    let mut program = chess_transition_invariants::program();
    let state = run_start_game(&program);

    // A quiet_move that does everything except advance MoveCount.
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
    let outcome = ex()
        .propose(
            buggy,
            vec![dec(5), dec(2), dec(5), dec(4), subj("black")],
            &state,
        )
        .expect("kernel must not error");

    match outcome {
        Outcome::Rejected { reason } => {
            assert!(
                reason.to_string().contains("move_count_strictly_increases"),
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

/// Run `start_game` against the empty state and return the result.
fn run_start_game(program: &Program) -> State {
    run_named(program, "start_game", vec![], State::default())
}

/// Propose `transformation_name` from `program` against `state` and
/// `must_accept` the result. The rules always come from `ex()`.
fn run_named(
    program: &Program,
    transformation_name: &str,
    args: Vec<EvalValue>,
    state: State,
) -> State {
    let t = program
        .transformation(transformation_name)
        .unwrap_or_else(|| panic!("transformation `{transformation_name}` not found"));
    ex().must_accept(t, args, state)
}
