# Chess transition invariants

A worked example that exists for one reason: to force [`Expr::Pre`](../../crates/morpholog-core/src/ir.rs) into the kernel. Where the other examples are business shapes that earn their primitives by getting close to a real audit or trading rule, this one is honest about being a teaching demo - the cleanest small domain where transition invariants pay for themselves on their own terms.

The inspiration is Murat Demirbas's [Chess invariants](https://muratbuffalo.blogspot.com/2026/05/chess-invariants.html) post, which lays out a TLA+ model of basic chess and splits its safety rules into two families: **state invariants** (predicates over a single state) and **transition invariants** (predicates over a `<state, next-state>` pair). State invariants are what Morpholog already had. Transition invariants are what `pre(...)` makes expressible.

## The doctrinal point

Up to PR #69, an invariant could only say "the world is in an admissible shape." It could not say "the world changed in an admissible way." The two are distinct.

The canonical business case is conservation of an admitted balance: "this payment is not merely below the cap; it must consume exactly its amount of the remaining entitlement." Example 05 (`insurance_claim_settlement`) carries that case via `PolicyHeadroom` and the `headroom_consumed_by_payment` transition invariant - that is where `pre(...)` earns its place in a real audit story.

Chess is the same shape with the business stripped out. `PieceCountNonIncreasing` is conservation of pieces over a single move: pieces do not appear out of thin air, only disappear via capture. `MoveCountStrictlyIncreases` is the chess analogue of "every transaction increments the audit counter by one." `TurnAlternates` is the two-party version of "the next actor is not the same actor." None of these can be expressed as a predicate over a single state. All three are textbook in the chess paper, and all three force `pre(...)` cleanly.

## The scoped domain

Chess is full of features that would force chess-specific kernel changes - castling moves four squares per turn, en passant captures a piece not on the destination, promotion morphs a piece type, the legal-moves clause is a scheduling concern more than a safety one. None of those help Morpholog's audience.

This example keeps the chess that maps to general transition rules and drops the rest. It models:

- A board, as a set of `PieceAt(square, piece_type, color)` claims.
- A move counter (`MoveCount`), a piece counter (`PieceCount`), and a turn marker (`CurrentTurn`).
- The opening position - 32 pieces, white to move, counter at zero.
- A non-capturing move (`quiet_move`) and a capturing move (`capturing_move`).

It deliberately does not model castling, en passant, promotion, check, checkmate, stalemate, the legal-moves precondition, or any piece-specific movement rules. Those each force their own design call, and none are needed to demonstrate `pre(...)`.

## The program

See [`chess.morph`](chess.morph) for the surface form.

### Claims

| Predicate | Role |
| --- | --- |
| `PieceAt(square, piece_type, color)` | One piece on one square. Append-and-retractable: a move retracts the source claim and asserts a destination one. |
| `MoveCount(n)` | The running move count. Retracted and re-asserted each move. |
| `PieceCount(n)` | The running total of pieces on the board. Retracted and re-asserted on captures. |
| `CurrentTurn(color)` | Whose turn it is. Retracted and re-asserted each move. |

Subjects used as constants: `#white`, `#black`; piece types `#pawn` `#knight` `#bishop` `#rook` `#queen` `#king`; squares `#a1` through `#h8`. None are special-cased by the kernel - they are opaque subject identifiers like every other Morpholog subject.

### State invariants

| Invariant | Says |
| --- | --- |
| `at_most_one_piece_per_square` | Two `PieceAt` claims for the same square must agree on type and colour. In practice, they must be the same claim. |
| `one_king_per_color` | Two `PieceAt` claims for kings of the same colour must be at the same square. Pairs with the above to mean "exactly one king per colour, at most." |

These do not force any new IR. They are the structural backdrop against which the transition rules become meaningful.

### Transition invariants

| Invariant | Says |
| --- | --- |
| `move_count_strictly_increases` | `MoveCount(n) and pre(MoveCount(m)) implies n = m + 1`. Every transition bumps the counter by exactly one. |
| `turn_alternates` | `CurrentTurn(t_now) and pre(CurrentTurn(t_prev)) implies t_now != t_prev`. The turn flips every move. |
| `single_capture_per_move` | `PieceCount(after) and pre(PieceCount(before)) implies (after = before) or (after = before - 1)`. At most one capture per move. This is the rule that pays for the `Or` kernel primitive that landed in the prior PR. |

Each one would be unprovable as a state invariant - none of them is a property of any single state.

### Transformations

| Transformation | Purpose |
| --- | --- |
| `start_game()` | Initialise the opening position: 32 pieces, `MoveCount(0)`, `PieceCount(32)`, white to move. |
| `quiet_move(src, dst, new_turn)` | Move a piece from `src` to `dst`; `dst` must be empty; `MoveCount` advances; `PieceCount` is unchanged; `CurrentTurn` flips. |
| `capturing_move(src, dst, new_turn)` | Move from `src` to `dst` where `dst` is occupied by an enemy piece; the captured piece is retracted; `PieceCount` decrements by one. |

The transformations carry their own `require` clauses - the moving piece must belong to the player whose turn it is, the new turn must differ from the current. Those are *admission gates*, not invariants. The transition invariants verify the same properties from the outside, so a future programmer who introduced a bug into the transformation body would still be caught.

## Genesis behaviour

Before any `MoveCount` is admitted - the state before `start_game()` runs against an empty database - `pre(MoveCount(m))` matches nothing in pre-state. Under `implies`, the rule is vacuously true: there is no `m` to constrain `n` against. After `start_game()` admits `MoveCount(0)`, every subsequent move has both a pre-value and a post-value, and the rule constrains them.

This is the deliberate semantics of the wrapper. Genesis is not a special case in the kernel; it falls out of `implies` vacuity, the same way it would in TLA+. Authors who want a different genesis story write it explicitly with an `or` branch.

## What this example is not

It is not a chess engine. It does not enforce legal moves (a bishop moving sideways is admitted, as long as `at_most_one_piece_per_square` and the transition rules hold). It does not detect check, checkmate, stalemate, threefold repetition, or the fifty-move rule. It does not model castling, en passant, promotion, or the touch-move rule.

It is the smallest setting in which a runtime's transition invariants become the load-bearing safety mechanism, with chess as a backdrop the reader already knows. The same kernel primitive that makes the chess invariants expressible is what the insurance example's `headroom_consumed_by_payment` invariant uses to enforce per-policy entitlement consumption - identical mechanism, different domain narrative.
