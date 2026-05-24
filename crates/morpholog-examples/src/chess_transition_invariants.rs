//! Chess transition invariants - the forcing example for
//! [`morpholog_core::Expr::Pre`]. Inspired by Murat Demirbas's
//! [Chess invariants](https://muratbuffalo.blogspot.com/2026/05/chess-invariants.html)
//! post, which splits safety rules into state invariants (over one
//! state) and transition invariants (over a pre/post pair). The
//! kernel could only express the first kind; this example forces
//! the second.
//!
//! Surface form: `examples/07_chess_transition_invariants/chess.morph`.
//! Business framing: the README, plus
//! [`crate::insurance_claim_settlement`] for the canonical business
//! use of `pre()`.

use morpholog_core::dsl::*;
use morpholog_core::{Invariant, Transformation};

// ============================================================
// State invariants
// ============================================================

/// At most one piece per square: two `PieceAt` claims for the same
/// square must agree on type and colour.
pub fn at_most_one_piece_per_square() -> Invariant {
    Invariant {
        name: "at_most_one_piece_per_square".to_string(),
        version: 1,
        body: implies(
            and(vec![
                claim("PieceAt", vec![var("sq"), var("t_a"), var("c_a")]),
                claim("PieceAt", vec![var("sq"), var("t_b"), var("c_b")]),
            ]),
            and(vec![
                eq(term(var("t_a")), term(var("t_b"))),
                eq(term(var("c_a")), term(var("c_b"))),
            ]),
        ),
    }
}

/// Exactly one white king. Counting the king claims of a colour and
/// pinning the count to 1 is strictly stronger than "at most one": it
/// also forbids the count falling to 0, so a king can never be
/// captured. Stated per colour (chess has exactly two) rather than
/// inventing a colour set.
pub fn exactly_one_white_king() -> Invariant {
    Invariant {
        name: "exactly_one_white_king".to_string(),
        version: 1,
        body: eq(
            sum(
                dec("1"),
                claim("PieceAt", vec![wildcard(), subj("king"), subj("white")]),
            ),
            term(dec("1")),
        ),
    }
}

/// Exactly one black king. The mirror of [`exactly_one_white_king`].
pub fn exactly_one_black_king() -> Invariant {
    Invariant {
        name: "exactly_one_black_king".to_string(),
        version: 1,
        body: eq(
            sum(
                dec("1"),
                claim("PieceAt", vec![wildcard(), subj("king"), subj("black")]),
            ),
            term(dec("1")),
        ),
    }
}

/// The piece counter must equal the actual number of pieces on the
/// board. `sum(1 | PieceAt(...))` counts the matching claims; pinning
/// `PieceCount` to that count stops the hand-maintained counter ever
/// drifting from the board it is supposed to summarise.
pub fn piece_count_matches_board() -> Invariant {
    Invariant {
        name: "piece_count_matches_board".to_string(),
        version: 1,
        body: implies(
            claim("PieceCount", vec![var("n")]),
            eq(
                term(var("n")),
                sum(
                    dec("1"),
                    claim("PieceAt", vec![wildcard(), wildcard(), wildcard()]),
                ),
            ),
        ),
    }
}

/// At most eight pawns per colour. A pawn count can only fall in this
/// model (there is no promotion), so this is a structural sanity bound;
/// it is here to show counting inside a comparator, with the colour
/// `c` bound by the antecedent and reused to scope the count.
pub fn at_most_eight_pawns_per_color() -> Invariant {
    Invariant {
        name: "at_most_eight_pawns_per_color".to_string(),
        version: 1,
        body: implies(
            claim("PieceAt", vec![wildcard(), subj("pawn"), var("c")]),
            le(
                sum(
                    dec("1"),
                    claim("PieceAt", vec![wildcard(), subj("pawn"), var("c")]),
                ),
                term(dec("8")),
            ),
        ),
    }
}

// ============================================================
// Transition invariants - the pre()/post comparison family.
// ============================================================

/// `moveCount' = moveCount + 1` per transition. Vacuously true
/// against an empty pre-state (no `MoveCount` to constrain).
pub fn move_count_strictly_increases() -> Invariant {
    Invariant {
        name: "move_count_strictly_increases".to_string(),
        version: 1,
        body: implies(
            and(vec![
                claim("MoveCount", vec![var("n")]),
                pre(claim("MoveCount", vec![var("m")])),
            ]),
            eq(term(var("n")), add(term(var("m")), term(dec("1")))),
        ),
    }
}

/// The turn flips every transition: post-turn must differ from
/// pre-turn (without naming the opponent function).
pub fn turn_alternates() -> Invariant {
    Invariant {
        name: "turn_alternates".to_string(),
        version: 1,
        body: implies(
            and(vec![
                claim("CurrentTurn", vec![var("t_now")]),
                pre(claim("CurrentTurn", vec![var("t_prev")])),
            ]),
            neq(var("t_now"), var("t_prev")),
        ),
    }
}

/// At most one capture per transition: `PieceCount' = PieceCount
/// or PieceCount' = PieceCount - 1`. The disjunction is what `Or`
/// earns its place for.
pub fn single_capture_per_move() -> Invariant {
    Invariant {
        name: "single_capture_per_move".to_string(),
        version: 1,
        body: implies(
            and(vec![
                claim("PieceCount", vec![var("after")]),
                pre(claim("PieceCount", vec![var("before")])),
            ]),
            or(vec![
                eq(term(var("after")), term(var("before"))),
                eq(term(var("after")), sub(term(var("before")), term(dec("1")))),
            ]),
        ),
    }
}

// ============================================================
// Transformations
// ============================================================

/// Initialise the opening position: 32 pieces, `MoveCount(0)`,
/// `PieceCount(32)`, white to move. Rejected by the structural
/// invariants against any non-empty pre-state.
pub fn start_game() -> Transformation {
    // Helper: build an assert for one piece at one square.
    fn piece_at(square: &str, piece_type: &str, color: &str) -> morpholog_core::Stmt {
        assert_("PieceAt", vec![subj(square), subj(piece_type), subj(color)])
    }

    Transformation {
        name: "start_game".to_string(),
        parameters: params(&[]),
        body: vec![
            // White back rank (rank 1)
            piece_at("a1", "rook", "white"),
            piece_at("b1", "knight", "white"),
            piece_at("c1", "bishop", "white"),
            piece_at("d1", "queen", "white"),
            piece_at("e1", "king", "white"),
            piece_at("f1", "bishop", "white"),
            piece_at("g1", "knight", "white"),
            piece_at("h1", "rook", "white"),
            // White pawns (rank 2)
            piece_at("a2", "pawn", "white"),
            piece_at("b2", "pawn", "white"),
            piece_at("c2", "pawn", "white"),
            piece_at("d2", "pawn", "white"),
            piece_at("e2", "pawn", "white"),
            piece_at("f2", "pawn", "white"),
            piece_at("g2", "pawn", "white"),
            piece_at("h2", "pawn", "white"),
            // Black pawns (rank 7)
            piece_at("a7", "pawn", "black"),
            piece_at("b7", "pawn", "black"),
            piece_at("c7", "pawn", "black"),
            piece_at("d7", "pawn", "black"),
            piece_at("e7", "pawn", "black"),
            piece_at("f7", "pawn", "black"),
            piece_at("g7", "pawn", "black"),
            piece_at("h7", "pawn", "black"),
            // Black back rank (rank 8)
            piece_at("a8", "rook", "black"),
            piece_at("b8", "knight", "black"),
            piece_at("c8", "bishop", "black"),
            piece_at("d8", "queen", "black"),
            piece_at("e8", "king", "black"),
            piece_at("f8", "bishop", "black"),
            piece_at("g8", "knight", "black"),
            piece_at("h8", "rook", "black"),
            assert_("MoveCount", vec![dec("0")]),
            assert_("PieceCount", vec![dec("32")]),
            assert_("CurrentTurn", vec![subj("white")]),
            emit("GameStarted", vec![]),
        ],
    }
}

/// Non-capturing move: relocate the piece from `src` to `dst`
/// (which must be empty), bump `MoveCount`, flip `CurrentTurn`,
/// leave `PieceCount` alone.
pub fn quiet_move() -> Transformation {
    Transformation {
        name: "quiet_move".to_string(),
        parameters: params(&["src", "dst", "new_turn"]),
        body: vec![
            bind_one(claim(
                "PieceAt",
                vec![var("src"), var("piece_type"), var("piece_color")],
            )),
            bind_one(claim("CurrentTurn", vec![var("current_turn")])),
            bind_one(claim("MoveCount", vec![var("m")])),
            require(eq(term(var("piece_color")), term(var("current_turn")))),
            require(neq(var("new_turn"), var("current_turn"))),
            require(not(exists(
                "anything",
                claim(
                    "PieceAt",
                    vec![var("dst"), var("any_type"), var("any_color")],
                ),
            ))),
            let_("next_m", add(term(var("m")), term(dec("1")))),
            retract(
                "PieceAt",
                vec![var("src"), var("piece_type"), var("piece_color")],
            ),
            retract("CurrentTurn", vec![var("current_turn")]),
            retract("MoveCount", vec![var("m")]),
            assert_(
                "PieceAt",
                vec![var("dst"), var("piece_type"), var("piece_color")],
            ),
            assert_("CurrentTurn", vec![var("new_turn")]),
            assert_("MoveCount", vec![var("next_m")]),
            emit("PieceMoved", vec![var("src"), var("dst")]),
        ],
    }
}

/// Capturing move: as [`quiet_move`] but `dst` holds an enemy
/// piece that gets retracted, and `PieceCount` decrements.
pub fn capturing_move() -> Transformation {
    Transformation {
        name: "capturing_move".to_string(),
        parameters: params(&["src", "dst", "new_turn"]),
        body: vec![
            bind_one(claim(
                "PieceAt",
                vec![var("src"), var("piece_type"), var("piece_color")],
            )),
            bind_one(claim(
                "PieceAt",
                vec![var("dst"), var("captured_type"), var("captured_color")],
            )),
            bind_one(claim("CurrentTurn", vec![var("current_turn")])),
            bind_one(claim("MoveCount", vec![var("m")])),
            bind_one(claim("PieceCount", vec![var("p")])),
            require(eq(term(var("piece_color")), term(var("current_turn")))),
            require(neq(var("new_turn"), var("current_turn"))),
            // A capture must be of an enemy piece.
            require(neq(var("captured_color"), var("current_turn"))),
            let_("next_m", add(term(var("m")), term(dec("1")))),
            let_("next_p", sub(term(var("p")), term(dec("1")))),
            retract(
                "PieceAt",
                vec![var("src"), var("piece_type"), var("piece_color")],
            ),
            retract(
                "PieceAt",
                vec![var("dst"), var("captured_type"), var("captured_color")],
            ),
            retract("CurrentTurn", vec![var("current_turn")]),
            retract("MoveCount", vec![var("m")]),
            retract("PieceCount", vec![var("p")]),
            assert_(
                "PieceAt",
                vec![var("dst"), var("piece_type"), var("piece_color")],
            ),
            assert_("CurrentTurn", vec![var("new_turn")]),
            assert_("MoveCount", vec![var("next_m")]),
            assert_("PieceCount", vec![var("next_p")]),
            emit(
                "PieceCaptured",
                vec![var("src"), var("dst"), var("captured_type")],
            ),
        ],
    }
}

pub fn all_predicates() -> Vec<morpholog_core::PredicateDecl> {
    vec![
        predicate("PieceAt")
            .subject("square")
            .subject("piece_type")
            .subject("color")
            .build(),
        predicate("MoveCount").decimal("n").build(),
        predicate("PieceCount").decimal("n").build(),
        predicate("CurrentTurn").subject("color").build(),
    ]
}

pub fn all_invariants() -> Vec<Invariant> {
    vec![
        at_most_one_piece_per_square(),
        exactly_one_white_king(),
        exactly_one_black_king(),
        piece_count_matches_board(),
        at_most_eight_pawns_per_color(),
        move_count_strictly_increases(),
        turn_alternates(),
        single_capture_per_move(),
    ]
}

/// The chess example as a [`morpholog_core::Program`]. Stable
/// identifier: `"chess_transition_invariants"`.
pub fn program() -> morpholog_core::Program {
    morpholog_core::Program {
        name: "chess_transition_invariants".to_string(),
        predicates: all_predicates(),
        intents: vec![
            intent_decl("GameStarted").build(),
            intent_decl("PieceMoved")
                .subject("src")
                .subject("dst")
                .build(),
            intent_decl("PieceCaptured")
                .subject("src")
                .subject("dst")
                .subject("captured_type")
                .build(),
        ],
        invariants: all_invariants(),
        transformations: vec![start_game(), quiet_move(), capturing_move()],
        derived_claims: vec![],
    }
}
