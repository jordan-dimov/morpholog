//! Chess transition invariants example.
//!
//! Authored as surface source at
//! `examples/07_chess_transition_invariants/chess.morph`; this module
//! parses it and exposes the registered program plus the by-name
//! accessors the tests use. There is no hand-built IR.

use std::sync::LazyLock;

use morpholog_core::{Invariant, PredicateDecl, Program, Transformation};

static PROGRAM: LazyLock<Program> = LazyLock::new(|| {
    crate::parse_example(
        "chess_transition_invariants",
        include_str!("../../../examples/07_chess_transition_invariants/chess.morph"),
    )
});

pub fn program() -> Program {
    PROGRAM.clone()
}

pub fn all_predicates() -> Vec<PredicateDecl> {
    PROGRAM.predicates.clone()
}

pub fn all_invariants() -> Vec<Invariant> {
    PROGRAM.invariants.clone()
}

pub fn at_most_one_piece_per_square() -> Invariant {
    crate::invariant(&PROGRAM, "at_most_one_piece_per_square")
}

pub fn exactly_one_white_king() -> Invariant {
    crate::invariant(&PROGRAM, "exactly_one_white_king")
}

pub fn exactly_one_black_king() -> Invariant {
    crate::invariant(&PROGRAM, "exactly_one_black_king")
}

pub fn piece_count_matches_board() -> Invariant {
    crate::invariant(&PROGRAM, "piece_count_matches_board")
}

pub fn board_with_pieces_has_a_counter() -> Invariant {
    crate::invariant(&PROGRAM, "board_with_pieces_has_a_counter")
}

pub fn at_most_eight_pawns_per_color() -> Invariant {
    crate::invariant(&PROGRAM, "at_most_eight_pawns_per_color")
}

pub fn move_count_strictly_increases() -> Invariant {
    crate::invariant(&PROGRAM, "move_count_strictly_increases")
}

pub fn turn_alternates() -> Invariant {
    crate::invariant(&PROGRAM, "turn_alternates")
}

pub fn single_capture_per_move() -> Invariant {
    crate::invariant(&PROGRAM, "single_capture_per_move")
}

pub fn start_game() -> Transformation {
    crate::transformation(&PROGRAM, "start_game")
}

pub fn quiet_move() -> Transformation {
    crate::transformation(&PROGRAM, "quiet_move")
}

pub fn capturing_move() -> Transformation {
    crate::transformation(&PROGRAM, "capturing_move")
}
