# Chess transition invariants

A small chess world that shows three Morpholog ideas at once: rules that constrain not just *what state is allowed* but *how state is allowed to change* (transition invariants); *counting* - a rule fixing how many things of a kind may exist; and a *computed property* - a square's colour, stored nowhere and worked out from where the square sits.

It follows Murat Demirbas's [Chess invariants](https://muratbuffalo.blogspot.com/2026/05/chess-invariants.html) post, which models chess in TLA+ and notes that some chess rules are properties of a single board position ("at most one king per colour") while others are properties of a *move* ("the move counter goes up by exactly one"). Morpholog already supported the first kind; this example adds the second, through `pre(...)` - a wrapper that lets an invariant refer to the board *before* the move alongside the board after. The same "what just changed?" shape appears with a real domain in [`examples/05_insurance_claim_settlement`](../05_insurance_claim_settlement/); chess just keeps the idea visible against a familiar backdrop. [`chess.morph`](chess.morph) teaches it construct by construct.

## What it models

A deliberately small slice of chess: a board as a set of `PieceAt(file, rank, piece_type, color)` claims (a square named by two numbers, file and rank, each 1 to 8); game-level counters `MoveCount`, `PieceCount`, `CurrentTurn`; an opening-setup transformation; and a quiet move and a capturing move. It is not a chess engine - no castling, en passant, promotion, check, or piece-specific movement, and it will happily move a rook diagonally. The point is transition invariants and computed square colour, and the small subset serves it.

## The program

The full surface form, with the guided tour of the domain, is in [`chess.morph`](chess.morph).

### Claims

| Predicate | What it represents |
| --- | --- |
| `PieceAt(file, rank, piece_type, color)` | A piece on a square (file and rank are numbers 1 to 8). A move retracts the source claim and admits one at the destination. |
| `MoveCount(n)` | Moves played so far; retracted and re-admitted each move. |
| `PieceCount(n)` | Pieces on the board; re-admitted when a capture happens. |
| `CurrentTurn(color)` | Whose turn it is; re-admitted each move. |

Subjects are `#white` / `#black` and the six piece types. File and rank are decimals, not subjects, precisely so a square's colour can be computed from them.

### Invariants over a single board (no `pre`)

| Invariant | What it says |
| --- | --- |
| `at_most_one_piece_per_square` | A square holds at most one piece. |
| `exactly_one_white_king` / `exactly_one_black_king` | Each colour has *exactly* one king - so a king can never be captured. |
| `piece_count_matches_board` | A `PieceCount` counter must equal the actual pieces on the board. |
| `board_with_pieces_has_a_counter` | A board with pieces must carry a `PieceCount` - the presence half that makes the count check bite. |
| `at_most_eight_pawns_per_color` | A colour has at most eight pawns. |

### Invariants over a transition (`pre` compares before and after)

| Invariant | What it says |
| --- | --- |
| `move_count_strictly_increases` | `MoveCount(n) and pre(MoveCount(m)) implies n = m + 1`. |
| `turn_alternates` | `CurrentTurn(now) and pre(CurrentTurn(prev)) implies now != prev`. |
| `single_capture_per_move` | `PieceCount(after) and pre(PieceCount(before)) implies (after = before) or (after = before - 1)`. |

On the very first move there is no prior `MoveCount` to compare against, so a `pre`-dependent rule is vacuously satisfied; once `start_game()` has admitted `MoveCount(0)`, every later move has both values and the rule bites.

### A computed property: square colour

| Invariant | What it says |
| --- | --- |
| `bishops_on_opposite_square_colors` | The two bishops of a colour stand on opposite-coloured squares - `(file + rank) % 2` differs between them. Since a bishop never changes its square colour, this also turns away a move that would slide one onto the wrong colour. |

### Transformations

| Transformation | What it does |
| --- | --- |
| `start_game()` | The opening position: 32 pieces, `MoveCount(0)`, `PieceCount(32)`, white to move. |
| `quiet_move(src_f, src_r, dst_f, dst_r, new_turn)` | Moves a piece to an empty destination; counter up, turn flips, piece count unchanged. |
| `capturing_move(src_f, src_r, dst_f, dst_r, new_turn)` | Same, but the destination holds an enemy piece, retracted; piece count down one. |

Each move gates its own preconditions with `require` (the mover owns the turn; the new turn differs), while the transition invariants check the same conservation from the outside - so a future bug in a move transformation is caught on the candidate state, however it got there.

## What it deliberately does not cover

Not a chess engine: no legal-move enforcement (a bishop may move sideways), no check / checkmate / stalemate, no castling / en passant / promotion, no multiple games at once. It does not even enforce board bounds - file and rank are decimals, and nothing stops a piece landing on file 99 or 3.5. Keeping a piece on a real square would be one more ordinary invariant (`PieceAt(f, r, _, _) implies (1 <= f and f <= 8 and 1 <= r and r <= 8)`), left out because this example is about transition invariants and computed colour, not board legality.
