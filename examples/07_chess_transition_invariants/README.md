# Chess: a referee that cannot be argued with

What happens when a rule is enforced perfectly, but the rule is not quite the one you meant?

This example is a chess referee written as Morpholog rules. It refuses any move that would break one of them, every time, with the rule's name. In about fifteen minutes you will watch a rule refuse a move, find a hole in that same rule, close the hole, and then ask the question that matters most once rules change: which of the moves already played would the new rule have refused?

Chess is here because everyone already knows what the rules should be. The same story plays out with trading limits, credit decisions or approval controls, where the right rule is much harder to see.

## What the referee knows

The board is a set of *claims*, one per piece: `PieceAt(file, rank, piece_type, color)`. A square is two numbers, the file (column) and rank (row), each 1 to 8, so the square chess players call c1 is file 3, rank 1. Three more claims track the game: how many moves have been played, how many pieces are left, and whose turn it is. A move is a proposal to change those claims, and the referee commits it only if every rule still holds afterwards.

The rules come in two kinds. Some judge a single position: "each side has exactly one king". Others judge a change, and compare the board before a move with the board after it: "the move counter goes up by exactly one". The second kind is written with `pre(...)`, which reads as "as it was before this move". The idea follows Murat Demirbas's [Chess invariants](https://muratbuffalo.blogspot.com/2026/05/chess-invariants.html) post, which models chess in TLA+.

The referee knows almost nothing about how pieces move. A rook may move diagonally and a pawn may jump the board. That is deliberate, and below it becomes useful. [`chess.morph`](chess.morph) teaches every rule from scratch, in its comments.

## Try it

You need a `morpholog` binary and a checkout of this repository at the same version, and PostgreSQL 18 or later ([`docs/install.md`](../../docs/install.md)). No Rust. Run everything from the checkout's root.

### 1. A fresh database

Make a new, empty database for this. Never point it at one you care about.

```sh
createdb morpholog_chess
export DATABASE_URL=postgres:///morpholog_chess
morpholog init
M=examples/07_chess_transition_invariants/chess.morph
```

### 2. Start a game

```sh
morpholog propose "$M" start_game --actor you --args-named '{}'
```

The answer says `"status": "committed"` and lists the 35 claims it admitted: 32 pieces, the two counters at 0 and 32, and white to move. `--actor` names who proposed it; here it is a name you assert, not a login anyone checked.

### 3. A rule at work

Every player has two bishops, one on light squares and one on dark squares, and a bishop never changes colour. A square's colour is computed, never stored: `(file + rank) % 2` is 0 on a dark square and 1 on a light one. The referee's rule `bishops_on_opposite_square_colors` says a side's two bishops always stand on opposite colours.

Try moving white's dark-squared bishop from c1 to d3, a light square, while the light-squared bishop still stands on f1:

```sh
morpholog propose "$M" quiet_move --actor you --args-named '{"src_f": "3", "src_r": "1", "dst_f": "4", "dst_r": "3", "new_turn": "black"}'
```

The answer says `"status": "rejected"`, names the rule `bishops_on_opposite_square_colors`, and gives the witness: the two bishops it compared, on files 4 and 6. Nothing changed. Ask the referee where the white bishops are:

```sh
morpholog inspect claims --named "$M" --predicate PieceAt --where piece_type=bishop --where color=white
```

Still on c1 and f1.

### 4. A hole in the rule

None of these three moves is legal chess: the bishop cannot pass its own pawn on d2, a pawn on f7 cannot take on f1, and a bishop never moves sideways. Our referee accepts all three because it does not know how pieces move, and that is not the hole we are after. Watch what the third move does to a rule the referee *does* enforce:

```sh
morpholog propose "$M" quiet_move --actor you --args-named '{"src_f": "3", "src_r": "1", "dst_f": "5", "dst_r": "3", "new_turn": "black"}'
morpholog propose "$M" capturing_move --actor you --args-named '{"src_f": "6", "src_r": "7", "dst_f": "6", "dst_r": "1", "new_turn": "white"}'
morpholog propose "$M" quiet_move --actor you --args-named '{"src_f": "5", "src_r": "3", "dst_f": "4", "dst_r": "3", "new_turn": "black"}'
```

The bishop goes c1 to e3 (dark to dark), black takes white's light-squared bishop on f1, and then the dark-squared bishop steps from e3 to d3, a light square. All three commit. Ask again:

```sh
morpholog inspect claims --named "$M" --predicate PieceAt --where piece_type=bishop --where color=white
```

One white bishop, on d3: a dark-squared bishop on a light square. In step 3 the referee refused to put that bishop on d3.

The rule did exactly what it says. It compares a side's two bishops, and white now has one. A rule about a single position cannot see where that bishop was before the move.

### 5. How would you fix it?

Stop for a moment and try. Could the bishop rule be made stricter? What would it compare, with one bishop left?

The property we want is not about the position. It is about the move: *a bishop keeps the colour of its square*. That is a gate on the act of moving, checked when the move is made, with the move's own squares in hand. In Morpholog a gate is a `require`.

Copy the programme so the original stays as it is:

```sh
cp "$M" /tmp/chess_fixed.morph
```

In the copy, add this condition anywhere at the top level, for example just above `transformation start_game():`

```
define keeps_square_colour(piece_type, src_f, src_r, dst_f, dst_r):
  piece_type != #bishop or (src_f + src_r) % 2 = (dst_f + dst_r) % 2
```

and add this gate to both `quiet_move` and `capturing_move`, just below the line `require piece_color = current_turn`:

```
  require bishop_keeps_its_colour: keeps_square_colour(piece_type, src_f, src_r, dst_f, dst_r)
```

Why twice? An invariant guards every change, however it is made. A gate guards only the act it sits in, so every move that can carry a bishop needs it. The `define` names the condition once, so the two moves cannot drift apart.

```sh
morpholog check /tmp/chess_fixed.morph
```

No output means the programme passed Morpholog's checks: it is well formed, with names, arities, kinds and bindings that agree. That does not mean they are the rules you meant. The programme in step 4 passed them too.

### 6. What about the moves already made?

We found a mistake in our rules. Before running anything, predict: of the four actions the referee has committed, starting the game and three moves, which would the fixed rules have refused?

```sh
morpholog rehearse /tmp/chess_fixed.morph
```

`rehearse` takes every committed action from the record and decides it again under the new rules, against the board as it stood at the time. It changes nothing. Among its answer:

```text
"transitions_replayed": 4,
"totals": {
  "same": 3,
  "different_delta": 0,
  "would_reject": 1,
```

and under `"refusals"` it names `bishop_keeps_its_colour`, the gate you wrote, with the one move it would have refused: e3 to d3. The refused attempt in step 3 is not counted, because the record holds only what was committed.

Now look at the board once more:

```sh
morpholog inspect claims --named "$M" --predicate PieceAt --where piece_type=bishop --where color=white
```

The bishop is still on d3. The game has not changed: that move really was accepted, under the rules in force when it was made. And we now know, by name and by move, which decision the better rules would have refused. Morpholog gives you both answers without rewriting either.

One limit: `rehearse` judges each recorded move on the board the record shows. It does not play out the game that would have followed if the move had been refused, because nobody knows what the players would have done instead.

## What this guarantees, and what it doesn't

**Guarantees.** Every committed move obeyed every rule in the programme it was proposed under. A refused move changed nothing. The record of what was committed stays as it was, and `rehearse` can decide it again under a candidate programme, reporting which decisions would differ, and which actions the candidate cannot replay at all (one it no longer has, or whose arguments it no longer takes).

**Assumes.** `--actor you` is a name you asserted, not an identity anyone authenticated. The moves are what the players meant to play; the referee checks the rules, not the intent.

**Does not guarantee.** Legal chess. There is no movement geometry, no check or checkmate, no castling, en passant or promotion, and one game at a time. And rules can be wrong in ways no referee can notice, as step 4 showed: a perfectly enforced rule is only as good as what it says.

## The rules, for reference

| Rule | What it says |
| --- | --- |
| `piece_at_unique_by_file_rank` | A square holds at most one piece. Generated from `unique by (file, rank)` on `PieceAt`. |
| `pieces_stand_on_the_board` | File and rank are whole numbers from 1 to 8. |
| `turn_belongs_to_a_player` | The turn is always white's or black's. |
| `exactly_one_white_king` / `exactly_one_black_king` | Each side has exactly one king, so a king can never be captured. |
| `bishops_on_opposite_square_colors` | A side's two bishops stand on opposite colours. With one left there is nothing to compare. |
| `piece_count_matches_board` / `board_with_pieces_has_a_counter` | The piece counter exists and equals the pieces on the board. |
| `at_most_eight_pawns_per_color` | A side has at most eight pawns. |
| `move_count_strictly_increases` | `MoveCount(n) and pre(MoveCount(m)) implies n = m + 1`. |
| `turn_alternates` | `CurrentTurn(now) and pre(CurrentTurn(prev)) implies now != prev`. |
| `single_capture_per_move` | The piece count after a move is the same as before, or one less. |

The moves are `start_game()`, `quiet_move(src_f, src_r, dst_f, dst_r, new_turn)` and `capturing_move(...)` with the same arguments. Each gates its own preconditions with `require`: the mover owns the turn, the next turn differs, a quiet move's destination is empty.

The steps above are checked by a test, which empties Morpholog's tables in the database named by `DATABASE_URL` before it starts, so point it at a disposable one: `cargo test -p morpholog-cli --test chess_tutorial_e2e`. It proposes the same moves, applies the same fix and runs the same reads as this page, checks that each command appears here exactly as it runs, and fails if this page and the referee disagree. It does not run the setup or your editor: it uses a prepared test database and writes the fixed copy itself.
