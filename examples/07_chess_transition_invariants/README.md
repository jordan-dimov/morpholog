# Chess transition invariants

A small chess world that demonstrates three Morpholog ideas: rules that constrain not just *what state is allowed* but *how state is allowed to change*; *counting* - a rule that fixes how many things of a kind may exist; and a *computed property* - a square's colour, which is stored nowhere and worked out from where the square sits, using the remainder operator `%`.

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

- A board, represented as a set of `PieceAt(file, rank, piece_type, color)` claims. A square is named by two numbers: its file (column, 1 to 8) and its rank (row, 1 to 8), so the square players call c1 is file 3, rank 1.
- Counters that track game-level claims: `MoveCount` (how many moves have been played), `PieceCount` (how many pieces are on the board), and `CurrentTurn` (whose move it is).
- An initial-setup transformation that places the standard opening pieces.
- A quiet move (no capture) and a capturing move.

The example does not model castling, en passant, promotion, check, checkmate, stalemate, the threefold-repetition rule, or any piece-specific movement rules. It is not a chess engine; it does not stop you from moving a rook diagonally. Its purpose is to demonstrate transition invariants and the square-colour rule, and that purpose is served by the simpler subset.

## The program

The full surface form is in [`chess.morph`](chess.morph). The IR companion is in [`crates/morpholog-examples/src/chess_transition_invariants.rs`](../../crates/morpholog-examples/src/chess_transition_invariants.rs).

### The predicates

| Predicate | What it represents |
| --- | --- |
| `PieceAt(file, rank, piece_type, color)` | A specific piece sits on a specific square, named by file and rank (each a number 1 to 8). A move retracts the claim at the source square and admits a new one at the destination. |
| `MoveCount(n)` | How many moves have been played so far. Retracted and re-admitted each move. |
| `PieceCount(n)` | Total pieces currently on the board. Retracted and re-admitted when a capture happens. |
| `CurrentTurn(color)` | Whose turn it is. Retracted and re-admitted each move. |

Constants used as subjects: `#white`, `#black`; the six piece types (`#pawn`, `#knight`, `#bishop`, `#rook`, `#queen`, `#king`). File and rank are decimals, not subjects, precisely so a square's colour can be computed from them. Morpholog does not treat any of these specially - they are just values like any other in any other example.

### Rules about state

Rules that hold over any single board position - none of these needs `pre(...)`:

| Invariant | What it says |
| --- | --- |
| `at_most_one_piece_per_square` | A square can hold at most one piece. If two `PieceAt` claims share a square they must describe the same piece. |
| `exactly_one_white_king` / `exactly_one_black_king` | Each colour has *exactly* one king. |
| `piece_count_matches_board` | When a `PieceCount` counter exists, it must equal the actual number of pieces on the board. |
| `board_with_pieces_has_a_counter` | A board with any pieces on it must carry a `PieceCount`. |
| `at_most_eight_pawns_per_color` | A colour has at most eight pawns. |

Most of these are *counting* rules, and they bring out a second language idea (the first being `pre(...)`). `sum(1 | PieceAt(_, #king, #white))` adds `1` for every white-king claim on the board - that is, it counts them. Pinning that count to `1` says "exactly one white king", which is strictly stronger than the "at most one" rule it replaces: it also forbids the count falling to zero, so **a king can never be captured**. `piece_count_matches_board` uses the same trick to tie the hand-maintained counter to reality - admit a stray piece without updating `PieceCount` and the count no longer matches, so the move is refused. A `sum` whose target is the literal `1` rather than a variable is how Morpholog counts.

The last two go together. `piece_count_matches_board` only checks a counter that is *present*; on its own it would shrug at a buggy move that dropped the counter entirely. `board_with_pieces_has_a_counter` requires the counter to exist whenever pieces do. Together they say "the counter exists and is correct" - the same presence-plus-consistency pairing the [insurance example](../05_insurance_claim_settlement/) uses for policy headroom.

### Rules about transitions

Rules that require comparing the board before a move with the board after:

| Invariant | What it says |
| --- | --- |
| `move_count_strictly_increases` | The move counter must go up by exactly one each move. `MoveCount(n) and pre(MoveCount(m)) implies n = m + 1`. |
| `turn_alternates` | The next move belongs to the other colour. `CurrentTurn(now) and pre(CurrentTurn(prev)) implies now != prev`. |
| `single_capture_per_move` | A move either captures one piece or captures none. `PieceCount(after) and pre(PieceCount(before)) implies (after = before) or (after = before - 1)`. |

None of these can be expressed as a property of a single board. "The counter went up by one" requires knowing both counter values.

### A rule about square colour

The third idea is a property that is *computed*, not stored. On a chessboard every square has a colour, but nothing records it - the colour follows from where the square is. A square is dark when `file + rank` is even and light when it is odd. Morpholog writes that test with the remainder operator: `(file + rank) % 2` is `0` for dark squares and `1` for light ones.

That single piece of arithmetic lets the example state a famous fact as an invariant:

| Invariant | What it says |
| --- | --- |
| `bishops_on_opposite_square_colors` | The two bishops of a colour always stand on opposite-coloured squares. Whenever two *different* squares each hold a bishop of the same colour, `(f_a + r_a) % 2` must differ from `(f_b + r_b) % 2`. |

This is a single-snapshot rule - it looks only at the current board - but it has a transition-like consequence. Because a real bishop never changes its square colour, the pair stays one-light-one-dark for the whole game; and since the model does not otherwise stop a bishop from sliding onto the wrong colour, this invariant is what turns such a move away. Move white's dark-squared bishop onto a light square while its light-squared partner is still on the board and the runtime refuses the move: it would put two white bishops on light squares. Slide it to another dark square and the move commits.

### The transformations

| Transformation | What it does |
| --- | --- |
| `start_game()` | Sets up the opening position: 32 pieces in their starting squares, `MoveCount(0)`, `PieceCount(32)`, white to move. |
| `quiet_move(src_f, src_r, dst_f, dst_r, new_turn)` | Moves a piece from the source square `(src_f, src_r)` to the destination `(dst_f, dst_r)`. The destination must be empty. The move counter goes up, the turn flips, the piece count is unchanged. |
| `capturing_move(src_f, src_r, dst_f, dst_r, new_turn)` | Same as a quiet move, but the destination holds an enemy piece, which is retracted. The piece count goes down by one. |

The transformations enforce their own preconditions through `require` clauses: the moving piece must belong to the player whose turn it is, and the supplied `new_turn` must differ from the current turn. These are admission gates: they reject illegal moves up front, with a clear "this would violate chess turn order" message rather than a generic invariant failure.

The transition invariants check the same properties from the outside. If a future change introduced a bug into one of the move transformations - say, forgetting to increment `MoveCount` - the transition invariant would still catch it on the candidate state, regardless of how the bug got there. This pairing - admission gate in the transformation, conservation rule in the invariant - is a recurring Morpholog pattern.

## Why the first move works

`pre(...)` looks up something in the previous state. What happens on the very first move, when there is no previous state to look in?

Morpholog handles this by making any rule that depends on a missing past claim vacuously true. Before `start_game()` runs, there is no `MoveCount` claim at all, so `pre(MoveCount(m))` matches nothing, and `move_count_strictly_increases` cannot fail. After `start_game()` admits `MoveCount(0)`, the next move has both a `pre` value and a current value, and the rule constrains them.

If you ever need to distinguish "this is the very first time" from "this is a normal update", you write the two cases as separate branches of an `or`.

## What this example does not try to do

This is not a chess engine and was never intended to be one. It does not enforce legal moves - a bishop moving sideways is admitted, as long as the basic structural and transition rules hold. It does not detect check, checkmate, stalemate, or any other game-ending condition. It does not model castling, en passant, promotion, or the touch-move rule. It does not handle multiple games at once.

What it does try to do is show, in the smallest space possible, what becomes expressible when invariants can refer to both a before and an after, and when a rule can compute a property like square colour rather than store it. The before-and-after mechanism is the same one the insurance example uses to enforce that every payment consumes exactly its amount of policy capacity. The chess setting is just easier to picture.
