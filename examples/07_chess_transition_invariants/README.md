# Chess transition invariants

A small chess world that demonstrates one specific Morpholog feature: rules that constrain not just *what state is allowed*, but *how state is allowed to change*.

The inspiration is Murat Demirbas's [Chess invariants](https://muratbuffalo.blogspot.com/2026/05/chess-invariants.html) post, which models chess in TLA+ and observes that some chess rules are properties of a single board position ("at most one king per colour"), while others are properties of a move ("the move counter goes up by exactly one"). Morpholog already supported the first kind. This example shows the second kind, made possible by a wrapper called `pre(...)` that lets a rule refer to the board *before* the move alongside the board *after*.

If you already use Morpholog to encode rules about admitted state, this example shows what becomes expressible once `pre(...)` is available. If you are evaluating Morpholog for a business use case (an audit log, a settlement system, an inventory rule), the same kind of "what just changed?" rule is what [`examples/05_insurance_claim_settlement`](../05_insurance_claim_settlement/) demonstrates with a real domain - this one keeps the kernel idea visible with chess as a familiar backdrop.

## Two kinds of rule

In Morpholog, an invariant is a rule that must always hold over admitted state. Up to now, every invariant in the worked examples has been a property of a single state:

- *Every journal entry's debits equal its credits.*
- *Every paid settlement has a matching authorisation.*
- *At most one current verification per asset.*

These check the state of the world as a snapshot. They don't say anything about *how* you got there.

Some real-world rules are not properties of any single snapshot. They are properties of a step:

- *Every move advances the move counter by exactly one.*
- *Every payment consumes exactly its amount of available capacity.*
- *Every transition flips whose turn it is.*

You cannot express these as conditions on one state. The information needed to check them lives in two states: the one before the change and the one after.

`pre(some_predicate)` is the Morpholog wrapper that says "evaluate this part against the previous state, not the current one." When an invariant uses `pre(...)`, it can compare values from before and after the proposed change.

## What this example models

A deliberately small slice of chess:

- A board, represented as a set of `PieceAt(square, piece_type, color)` claims.
- Counters that track game-level claims: `MoveCount` (how many moves have been played), `PieceCount` (how many pieces are on the board), and `CurrentTurn` (whose move it is).
- An initial-setup transformation that places the standard opening pieces.
- A quiet move (no capture) and a capturing move.

The example does not model castling, en passant, promotion, check, checkmate, stalemate, the threefold-repetition rule, or any piece-specific movement rules. It is not a chess engine; it does not stop you from moving a bishop sideways. Its purpose is to demonstrate transition invariants, and that purpose is served by the simpler subset.

## The program

The full surface form is in [`chess.morph`](chess.morph). The IR companion is in [`crates/morpholog-examples/src/chess_transition_invariants.rs`](../../crates/morpholog-examples/src/chess_transition_invariants.rs).

### The predicates

| Predicate | What it represents |
| --- | --- |
| `PieceAt(square, piece_type, color)` | A specific piece sits on a specific square. A move retracts the claim at the source square and admits a new one at the destination. |
| `MoveCount(n)` | How many moves have been played so far. Retracted and re-admitted each move. |
| `PieceCount(n)` | Total pieces currently on the board. Retracted and re-admitted when a capture happens. |
| `CurrentTurn(color)` | Whose turn it is. Retracted and re-admitted each move. |

Constants used as subjects: `#white`, `#black`; the six piece types (`#pawn`, `#knight`, `#bishop`, `#rook`, `#queen`, `#king`); the 64 squares (`#a1` through `#h8`). Morpholog does not treat these specially - they are just opaque identifiers like any other subject in any other example.

### Rules about state

Two rules that hold over any single board position:

| Invariant | What it says |
| --- | --- |
| `at_most_one_piece_per_square` | A square can hold at most one piece. If two `PieceAt` claims share a square they must describe the same piece. |
| `one_king_per_color` | Each colour has at most one king. Two king claims for the same colour must point at the same square. |

Both are familiar from chess. Neither needs `pre(...)`.

### Rules about transitions

Rules that require comparing the board before a move with the board after:

| Invariant | What it says |
| --- | --- |
| `move_count_strictly_increases` | The move counter must go up by exactly one each move. `MoveCount(n) and pre(MoveCount(m)) implies n = m + 1`. |
| `turn_alternates` | The next move belongs to the other colour. `CurrentTurn(now) and pre(CurrentTurn(prev)) implies now != prev`. |
| `single_capture_per_move` | A move either captures one piece or captures none. `PieceCount(after) and pre(PieceCount(before)) implies (after = before) or (after = before - 1)`. |

None of these can be expressed as a property of a single board. "The counter went up by one" requires knowing both counter values.

### The transformations

| Transformation | What it does |
| --- | --- |
| `start_game()` | Sets up the opening position: 32 pieces in their starting squares, `MoveCount(0)`, `PieceCount(32)`, white to move. |
| `quiet_move(src, dst, new_turn)` | Moves a piece from `src` to `dst`. `dst` must be empty. The move counter goes up, the turn flips, the piece count is unchanged. |
| `capturing_move(src, dst, new_turn)` | Same as a quiet move, but `dst` holds an enemy piece, which is retracted. The piece count goes down by one. |

The transformations enforce their own preconditions through `require` clauses: the moving piece must belong to the player whose turn it is, and the supplied `new_turn` must differ from the current turn. These are admission gates: they reject illegal moves up front, with a clear "this would violate chess turn order" message rather than a generic invariant failure.

The transition invariants check the same properties from the outside. If a future change introduced a bug into one of the move transformations - say, forgetting to increment `MoveCount` - the transition invariant would still catch it on the candidate state, regardless of how the bug got there. This pairing - admission gate in the transformation, conservation rule in the invariant - is a recurring Morpholog pattern.

## Why the first move works

`pre(...)` looks up something in the previous state. What happens on the very first move, when there is no previous state to look in?

Morpholog handles this by making any rule that depends on a missing past claim vacuously true. Before `start_game()` runs, there is no `MoveCount` claim at all, so `pre(MoveCount(m))` matches nothing, and `move_count_strictly_increases` cannot fail. After `start_game()` admits `MoveCount(0)`, the next move has both a `pre` value and a current value, and the rule constrains them.

If you ever need to distinguish "this is the very first time" from "this is a normal update", you write the two cases as separate branches of an `or`.

## What this example does not try to do

This is not a chess engine and was never intended to be one. It does not enforce legal moves - a bishop moving sideways is admitted, as long as the basic structural and transition rules hold. It does not detect check, checkmate, stalemate, or any other game-ending condition. It does not model castling, en passant, promotion, or the touch-move rule. It does not handle multiple games at once.

What it does try to do is show, in the smallest space possible, what becomes expressible when invariants can refer to both a before and an after. The mechanism is the same one used by the insurance example to enforce that every payment consumes exactly its amount of policy capacity. The chess setting is just easier to picture.
