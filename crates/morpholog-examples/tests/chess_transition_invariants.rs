//! Integration tests for the chess transition invariants example
//! (`examples/07_chess_transition_invariants/`).
//!
//! The key claim: a transition invariant (one using `pre(...)`) catches a
//! bug that a state invariant cannot. Also covers the census invariants,
//! the one `move_piece` that covers quiet moves and captures, and square
//! colour, `(file + rank) % 2`, used by `bishops_on_opposite_square_colors`.

#![allow(clippy::unwrap_used, clippy::expect_used)]

mod common;

use common::{Example, dec, dec_str, refused_by, subj};
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
            "pieces_stand_on_the_board",
            "exactly_one_white_king",
            "exactly_one_black_king",
            "bishops_on_opposite_square_colors",
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
    // present.
    let pre = State::from_claims(vec![
        claim_instance("PieceAt", &[dec(1), dec(1), subj("rook"), subj("white")]),
        claim_instance("PieceAt", &[dec(5), dec(1), subj("king"), subj("white")]),
        claim_instance("PieceAt", &[dec(1), dec(8), subj("king"), subj("black")]),
        claim_instance("PieceAt", &[dec(8), dec(8), subj("rook"), subj("black")]),
        claim_instance("CurrentTurn", &[subj("white")]),
        claim_instance("MoveCount", &[dec(10)]),
    ]);

    refused_by(
        &ex().must_reject(
            &chess_transition_invariants::move_piece(),
            squares([1, 1, 1, 8]),
            &pre,
        ),
        "exactly_one_black_king",
    );
}

/// `single_capture_per_move` rejects a move that leaves a stray piece on
/// the board: the board after holds one more piece than the board before.
#[test]
fn a_move_that_adds_a_piece_is_rejected() {
    // Built in Rust IR: the real move minus one statement, to prove the
    // invariant has teeth.
    use morpholog_core::ir_builder;

    let mut program = chess_transition_invariants::program();
    let state = run_start_game(&program);

    // A move that forgets to retract the piece from its source.
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

    // Knight b1 -> c3 (c3 is empty in the opening); the knight ends up on
    // both squares.
    refused_by(
        &ex().must_reject(
            drifting,
            vec![dec(2), dec(1), dec(3), dec(3), subj("black")],
            &state,
        ),
        "single_capture_per_move",
    );
}

/// `single_capture_per_move` also rejects a move that takes two pieces at
/// once: the board after holds two fewer.
#[test]
fn a_move_that_takes_two_pieces_is_rejected() {
    use morpholog_core::ir_builder;

    let mut program = chess_transition_invariants::program();
    let state = run_start_game(&program);

    let square = |f: &str, r: &str| {
        vec![
            ir_builder::var(f),
            ir_builder::var(r),
            ir_builder::wildcard(),
            ir_builder::wildcard(),
        ]
    };
    // Clears two squares in one move and advances the game as usual.
    let double_take = ir_builder::transformation(
        "double_take",
        ir_builder::params(&["a_f", "a_r", "b_f", "b_r"]),
        vec![
            ir_builder::bind_one(ir_builder::claim(
                "CurrentTurn",
                vec![ir_builder::var("turn")],
            )),
            ir_builder::bind_one(ir_builder::claim("MoveCount", vec![ir_builder::var("m")])),
            ir_builder::let_(
                "next_m",
                ir_builder::add(
                    ir_builder::term(ir_builder::var("m")),
                    ir_builder::term(ir_builder::dec("1")),
                ),
            ),
            ir_builder::retract("PieceAt", square("a_f", "a_r")),
            ir_builder::retract("PieceAt", square("b_f", "b_r")),
            ir_builder::retract("CurrentTurn", vec![ir_builder::var("turn")]),
            ir_builder::retract("MoveCount", vec![ir_builder::var("m")]),
            ir_builder::assert_("CurrentTurn", vec![ir_builder::subj("black")]),
            ir_builder::assert_("MoveCount", vec![ir_builder::var("next_m")]),
        ],
    );
    program.transformations.push(double_take);
    let double_take = program.transformation("double_take").expect("just pushed");

    // The a7 and b7 pawns, gone in one move.
    refused_by(
        &ex().must_reject(double_take, squares([1, 7, 2, 7]), &state),
        "single_capture_per_move",
    );
}

// ============================================================
// Full-chain propose: start_game and move_piece.
// ============================================================

/// `start_game` on an empty state admits 34 claims (32 pieces plus
/// `MoveCount` and `CurrentTurn`). The transition invariants hold
/// vacuously: there is no earlier `MoveCount`.
#[test]
fn start_game_admits_opening_position() {
    let program = chess_transition_invariants::program();
    let next = run_start_game(&program);
    assert_eq!(
        next.claims().len(),
        34,
        "opening position is 32 pieces + MoveCount + CurrentTurn"
    );
    refused_by(
        &ex().must_reject(&chess_transition_invariants::start_game(), vec![], &next),
        "game_not_yet_started",
    );
}

/// A quiet move on the opening position succeeds. The pawn at e2 moves to
/// e4, MoveCount goes from 0 to 1, CurrentTurn flips from white to black,
/// and no piece is taken.
#[test]
fn quiet_move_after_opening_succeeds() {
    let program = chess_transition_invariants::program();
    let state = run_start_game(&program);

    let next = run_move(&program, [5, 2, 5, 4], state);

    assert!(has(&next, "MoveCount", &[dec(1)]), "MoveCount must be 1");
    assert!(
        has(&next, "CurrentTurn", &[subj("black")]),
        "CurrentTurn must be black after white moves"
    );
    assert!(
        has(
            &next,
            "PieceAt",
            &[dec(5), dec(4), subj("pawn"), subj("white")]
        ),
        "pawn must be at e4 after the move"
    );
    assert!(
        !has(
            &next,
            "PieceAt",
            &[dec(5), dec(2), subj("pawn"), subj("white")]
        ),
        "pawn must no longer be at e2"
    );
    assert_eq!(pieces(&next), 32, "a quiet move takes nothing");
}

/// The same `move_piece` takes a piece when the destination holds one: e4
/// takes d5, the black pawn is gone, and the board holds 31 pieces.
#[test]
fn a_move_onto_an_enemy_piece_takes_it() {
    let program = chess_transition_invariants::program();
    let state = run_start_game(&program);
    let state = run_move(&program, [5, 2, 5, 4], state);
    let state = run_move(&program, [4, 7, 4, 5], state);
    let state = run_move(&program, [5, 4, 4, 5], state);

    assert!(has(
        &state,
        "PieceAt",
        &[dec(4), dec(5), subj("pawn"), subj("white")]
    ));
    assert!(!has(
        &state,
        "PieceAt",
        &[dec(5), dec(4), subj("pawn"), subj("white")]
    ));
    assert_eq!(pieces(&state), 31);
}

/// The turn passes between white and black on its own, and the gates name
/// what a wrong move breaks: moving the other side's piece, or landing on
/// your own.
#[test]
fn the_move_gates_name_what_is_wrong() {
    let program = chess_transition_invariants::program();
    let state = run_start_game(&program);
    let state = run_move(&program, [5, 2, 5, 4], state);
    assert!(has(&state, "CurrentTurn", &[subj("black")]));
    let state = run_move(&program, [5, 7, 5, 5], state);
    assert!(has(&state, "CurrentTurn", &[subj("white")]));

    let mv = chess_transition_invariants::move_piece();
    refused_by(
        &ex().must_reject(&mv, squares([4, 7, 4, 5]), &state),
        "your_own_piece",
    );
    refused_by(
        &ex().must_reject(&mv, squares([4, 1, 4, 2]), &state),
        "not_onto_your_own_piece",
    );
}

// ============================================================
// The board: whole squares from 1 to 8.
// ============================================================

/// A piece may land on the edge files: knight g1 to h3 reaches file 8,
/// knight b8 to a6 file 1.
#[test]
fn a_piece_may_land_on_the_edge_of_the_board() {
    let program = chess_transition_invariants::program();
    let state = run_start_game(&program);
    let state = run_move(&program, [7, 1, 8, 3], state);
    let state = run_move(&program, [2, 8, 1, 6], state);
    assert!(has(
        &state,
        "PieceAt",
        &[dec(8), dec(3), subj("knight"), subj("white")]
    ));
    assert!(has(
        &state,
        "PieceAt",
        &[dec(1), dec(6), subj("knight"), subj("black")]
    ));
}

/// Off the board, or between squares, is refused. Each destination keeps
/// the c1 bishop on its own colour or off the colour scale entirely, so
/// only the board rule can say no. A bishop on file 3.5 would otherwise
/// slip past the colour rule: its colour would be 0.5.
#[test]
fn a_piece_off_the_board_is_rejected() {
    let program = chess_transition_invariants::program();
    let state = run_start_game(&program);
    let mv = chess_transition_invariants::move_piece();
    for (file, rank) in [
        (dec(5), dec(9)),
        (dec(4), dec(0)),
        (dec(9), dec(3)),
        (dec(0), dec(4)),
        (dec_str("3.5"), dec(3)),
        (dec(5), dec_str("3.5")),
    ] {
        refused_by(
            &ex().must_reject(&mv, vec![dec(3), dec(1), file, rank], &state),
            "pieces_stand_on_the_board",
        );
    }
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
    refused_by(
        &ex().must_reject(
            &chess_transition_invariants::move_piece(),
            squares([3, 1, 4, 3]),
            &state,
        ),
        "bishops_on_opposite_square_colors",
    );
}

/// The same bishop may move within its own colour: c1 to e3
/// (`(5+3) % 2 = 0`, dark) keeps the bishops on opposite colours.
#[test]
fn bishop_keeping_square_color_is_allowed() {
    let program = chess_transition_invariants::program();
    let state = run_start_game(&program);
    let next = run_move(&program, [3, 1, 5, 3], state);
    assert!(
        has(
            &next,
            "PieceAt",
            &[dec(5), dec(3), subj("bishop"), subj("white")]
        ),
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

    // A move that does everything except advance MoveCount.
    let buggy_move = ir_builder::transformation(
        "buggy_move",
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

    let buggy = program.transformation("buggy_move").expect("just pushed");
    let outcome = ex()
        .propose(
            buggy,
            vec![dec(5), dec(2), dec(5), dec(4), subj("black")],
            &state,
        )
        .expect("kernel must not error");

    match outcome {
        Outcome::Rejected { reason } => refused_by(&reason, "move_count_strictly_increases"),
        Outcome::Accepted { .. } => {
            panic!(
                "a buggy move that fails to bump MoveCount must be rejected by the transition invariant"
            )
        }
    }
}

// ============================================================
// Helpers
// ============================================================

/// Run `start_game` against the empty state and return the result.
fn run_start_game(program: &Program) -> State {
    let t = program.transformation("start_game").expect("exists");
    ex().must_accept(t, vec![], State::default())
}

/// `[src_f, src_r, dst_f, dst_r]` as `move_piece` arguments.
fn squares(squares: [i64; 4]) -> Vec<EvalValue> {
    squares.into_iter().map(dec).collect()
}

/// `move_piece` from one square to another and `must_accept` the result.
fn run_move(program: &Program, from_to: [i64; 4], state: State) -> State {
    let t = program.transformation("move_piece").expect("exists");
    ex().must_accept(t, squares(from_to), state)
}

fn has(state: &State, predicate: &str, args: &[EvalValue]) -> bool {
    state
        .claims()
        .iter()
        .any(|c| c.predicate.as_str() == predicate && c.args == args)
}

fn pieces(state: &State) -> usize {
    state
        .claims()
        .iter()
        .filter(|c| c.predicate.as_str() == "PieceAt")
        .count()
}
