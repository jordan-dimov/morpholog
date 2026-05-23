//! Chess transition invariants - the forcing example for
//! [`morpholog_core::Expr::Pre`].
//!
//! The chess paper at <https://muratbuffalo.blogspot.com/2026/05/chess-invariants.html>
//! frames a system's safety rules as two families: state invariants
//! (predicates over a single state) and transition invariants
//! (predicates over a `<<state, next-state>>` pair). The kernel
//! evaluates invariants against a candidate state by default; this
//! example forces the second family by adding `pre(...)` - a wrapper
//! that flips the wrapped subtree to evaluate against pre-state.
//!
//! The chess domain is deliberate. It makes the conservation rules
//! textbook-clean (`MoveCountStrictlyIncreases`, `TurnAlternates`,
//! `SingleCapturePerMove`) without the cross-cutting concerns of a
//! business example, and avoids the genesis-vs-update awkwardness
//! that would force `or` into the conceptual centre. The same
//! `Expr::Pre` primitive that powers these chess invariants is what
//! a future per-account-delta ledger invariant (`pre(AccountBalance)
//! and AccountBalance implies after = before + posted_delta`) will
//! reach for - identical kernel mechanism, different domain narrative.
//!
//! Surface form: `examples/07_chess_transition_invariants/chess.morph`.
//! Business framing: see the example README.

use morpholog_core::dsl::*;
use morpholog_core::{Invariant, Transformation};

// ============================================================
// State invariants - admissible-state shape, no transition needed.
// ============================================================

/// At most one piece may occupy a square. Two `PieceAt` claims for
/// the same square must agree on type and colour - in practice they
/// must be the same claim.
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

/// Each colour has at most one king. Two `PieceAt` claims for kings
/// of the same colour must be at the same square. Pairs with
/// [`at_most_one_piece_per_square`]: together they say "at most one
/// king per colour at most one square."
pub fn one_king_per_color() -> Invariant {
    Invariant {
        name: "one_king_per_color".to_string(),
        version: 1,
        body: implies(
            and(vec![
                claim("PieceAt", vec![var("sq_a"), subj("king"), var("c")]),
                claim("PieceAt", vec![var("sq_b"), subj("king"), var("c")]),
            ]),
            eq(term(var("sq_a")), term(var("sq_b"))),
        ),
    }
}

// ============================================================
// Transition invariants - the `pre(...)` family.
//
// Each compares a value in the candidate (post) state to its
// counterpart in pre-state, expressing a delta rule that a single-
// state invariant cannot.
// ============================================================

/// The move counter advances by exactly one per transition. From the
/// chess paper: `MoveCountStrictlyIncreases ==
/// moveCount' = moveCount + 1`.
///
/// Genesis behaviour: before any `MoveCount` is admitted (the very
/// first move from an uninitialised state would be the only case),
/// `pre(MoveCount(m))` matches nothing and the `implies` is
/// vacuously true. After `start_game` admits `MoveCount(0)`, the
/// rule kicks in.
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

/// The turn flips every transition. From the chess paper:
/// `TurnAlternates == turn' = Opponent(turn)`. Expressed here
/// without naming the opponent function: post-state turn must
/// differ from pre-state turn.
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

/// Each transition captures at most one piece. From the chess
/// paper: `SingleCapturePerMove == PieceCount' = PieceCount or
/// PieceCount' = PieceCount - 1`. The disjunction is the load-
/// bearing reason `Or` was added as a kernel primitive ahead of
/// this PR.
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

/// Initialise the game: 32 pieces in the standard opening
/// position, `MoveCount(0)`, `PieceCount(32)`, white to move.
/// Idempotent only on the empty pre-state; a second call against
/// any populated board would violate the structural invariants and
/// be rejected.
///
/// The initial position is verbose by design: a worked example
/// should look like the domain. A future generator over a
/// rank/file enumeration would compress it, but `forall` over
/// surface-level subject sets is not in v0 - and the verbosity
/// makes the actual board readable in the diff.
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

/// A non-capturing move. Reads the moving piece at `src`, requires
/// `dst` is empty and the moving piece belongs to the player whose
/// turn it is, then retracts and re-asserts to relocate the piece.
/// `PieceCount` is unchanged; `MoveCount` advances; `CurrentTurn`
/// flips.
///
/// The transition invariants verify the contract from the outside:
/// even if a future programmer wrote a quiet_move that forgot to
/// bump `MoveCount`, the `move_count_strictly_increases` invariant
/// would reject the transition.
pub fn quiet_move() -> Transformation {
    Transformation {
        name: "quiet_move".to_string(),
        parameters: params(&["src", "dst", "new_turn"]),
        body: vec![
            // Read pre-state: which piece is at src, who is current
            // turn, what is the move counter.
            bind_one(claim(
                "PieceAt",
                vec![var("src"), var("piece_type"), var("piece_color")],
            )),
            bind_one(claim("CurrentTurn", vec![var("current_turn")])),
            bind_one(claim("MoveCount", vec![var("m")])),
            // The moving piece must belong to the player whose turn
            // it is. Foundational chess rule; without this any
            // colour could move on any turn.
            require(eq(term(var("piece_color")), term(var("current_turn")))),
            // The new turn must differ from the current; the
            // `turn_alternates` invariant would catch this anyway
            // but a `require` reports it lawfully rather than as an
            // invariant violation.
            require(neq(var("new_turn"), var("current_turn"))),
            // dst must be empty.
            require(not(exists(
                "anything",
                claim(
                    "PieceAt",
                    vec![var("dst"), var("any_type"), var("any_color")],
                ),
            ))),
            // Compute the next move count.
            let_("next_m", add(term(var("m")), term(dec("1")))),
            // Stage the transition: piece relocates, counters
            // advance, turn flips.
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

/// A capturing move. Like [`quiet_move`] but `dst` is occupied by
/// an enemy piece; the captured piece is retracted and
/// `PieceCount` decrements by one. The
/// `single_capture_per_move` invariant verifies the count moves
/// by exactly one.
pub fn capturing_move() -> Transformation {
    Transformation {
        name: "capturing_move".to_string(),
        parameters: params(&["src", "dst", "new_turn"]),
        body: vec![
            // Read pre-state: moving piece, captured piece, current
            // turn, counters.
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
            // Same colour gates as quiet_move.
            require(eq(term(var("piece_color")), term(var("current_turn")))),
            require(neq(var("new_turn"), var("current_turn"))),
            // A capture must be of an enemy piece.
            require(neq(var("captured_color"), var("current_turn"))),
            // Compute next counters.
            let_("next_m", add(term(var("m")), term(dec("1")))),
            let_("next_p", sub(term(var("p")), term(dec("1")))),
            // Stage: the moving piece replaces the captured one;
            // counters update; turn flips.
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
        one_king_per_color(),
        move_count_strictly_increases(),
        turn_alternates(),
        single_capture_per_move(),
    ]
}

/// The chess transition-invariants example as a
/// [`morpholog_core::Program`]: three transformations
/// (`start_game`, `quiet_move`, `capturing_move`), two state
/// invariants, and three transition invariants that exercise
/// [`morpholog_core::Expr::Pre`]. Stable identifier:
/// `"chess_transition_invariants"`.
pub fn program() -> morpholog_core::Program {
    morpholog_core::Program {
        name: "chess_transition_invariants".to_string(),
        predicates: all_predicates(),
        invariants: all_invariants(),
        transformations: vec![start_game(), quiet_move(), capturing_move()],
        derived_claims: vec![],
    }
}
