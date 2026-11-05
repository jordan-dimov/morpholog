# Chess: a referee that won't bend

What if a rule is enforced perfectly, but it is not quite the rule you meant?

This example is a chess referee built from Morpholog rules. It refuses every move that breaks a rule, and it tells you which rule. You will:

1. watch a rule refuse a move;
2. find a hole in that same rule;
3. close the hole;
4. ask which past moves the new rule would have refused.

Chess works well here because everyone knows what the rules should be. The same story happens with trading limits, credit checks and approvals. There, the right rule is much harder to see.

## How the referee sees the game

The referee doesn't store a board. It stores *claims*: statements it has agreed to accept. There is one claim per piece, `PieceAt(file, rank, piece_type, color)`. A square is two numbers from 1 to 8: the file (the column) and the rank (the row). So the square chess players call c1 is file 3, rank 1.

Three more claims track the game: the number of moves so far, the number of pieces left, and whose turn it is.

A move is a proposal to change these claims. The referee checks the result against every rule. If they all still hold, it commits the move. If one fails, it refuses the move and changes nothing.

Some rules look at one position, like "each side has exactly one king". Others compare the board before and after a move, like "the move count goes up by exactly one". Those use `pre(...)`, which means "as it was before this move". The idea comes from Murat Demirbas's [Chess invariants](https://muratbuffalo.blogspot.com/2026/05/chess-invariants.html) post, which models chess in TLA+.

The referee knows almost nothing about how pieces move. A rook may move diagonally. A pawn may jump across the board. This is on purpose, and it will come in handy. [`chess.morph`](chess.morph) explains every rule in plain comments.

## Try it

You need:

- a `morpholog` binary;
- a checkout of this repository at the same version;
- PostgreSQL 18 or later.

[`docs/install.md`](../../docs/install.md) shows how to get them. You don't need Rust. Run every command from the root of the checkout.

### 1. Make a new database

Use a new, empty database. Never use one you care about.

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

The answer says `"status": "committed"`. It lists the 35 claims the referee accepted: 32 pieces, a move count of 0, a piece count of 32, and white to move.

`--actor you` says who made the proposal. Here it is just a name you typed. Nobody checked it.

### 3. Watch a rule work

Each side has two bishops. One moves on light squares, the other on dark squares. A bishop never changes colour.

The referee doesn't store square colours. It works them out: `(file + rank) % 2` is 0 on a dark square and 1 on a light one. The rule `bishops_on_opposite_square_colors` says a side's two bishops always stand on different colours.

Try to move white's dark-squared bishop from c1 to d3, a light square. White's other bishop is still on f1, also a light square.

```sh
morpholog propose "$M" quiet_move --actor you --args-named '{"src_f": "3", "src_r": "1", "dst_f": "4", "dst_r": "3", "new_turn": "black"}'
```

The answer says `"status": "rejected"` and names the rule `bishops_on_opposite_square_colors`. Its `witness` shows the two bishops the rule compared, on files 4 and 6. Nothing changed. Check where the white bishops are:

```sh
morpholog inspect claims --named "$M" --predicate PieceAt --where piece_type=bishop --where color=white
```

They are still on c1 and f1.

### 4. Find the hole

Now play three moves. None of them is legal chess. The bishop can't pass its own pawn on d2. A pawn on f7 can't take a piece on f1. A bishop can't step sideways. Our referee allows all three, because it doesn't know how pieces move. That isn't the hole we're after. Watch what the third move does to a rule the referee *does* check.

```sh
morpholog propose "$M" quiet_move --actor you --args-named '{"src_f": "3", "src_r": "1", "dst_f": "5", "dst_r": "3", "new_turn": "black"}'
morpholog propose "$M" capturing_move --actor you --args-named '{"src_f": "6", "src_r": "7", "dst_f": "6", "dst_r": "1", "new_turn": "white"}'
morpholog propose "$M" quiet_move --actor you --args-named '{"src_f": "5", "src_r": "3", "dst_f": "4", "dst_r": "3", "new_turn": "black"}'
```

Here is what happens:

1. White's bishop goes from c1 to e3. Both are dark squares.
2. Black takes white's other bishop, on f1.
3. White's bishop steps from e3 to d3, a light square.

All three are committed. Check again:

```sh
morpholog inspect claims --named "$M" --predicate PieceAt --where piece_type=bishop --where color=white
```

White has one bishop left, on d3. A dark-squared bishop now stands on a light square. In step 3, the referee refused to put it there.

The rule did exactly what it says. It compares a side's two bishops, and white now has only one. A rule that looks at one position can't see where the bishop was before the move.

### 5. Fix the rule

Stop and think for a moment. Could you make the bishop rule stricter? What would it compare, with only one bishop left?

What we want isn't about the position. It's about the move: *a bishop keeps the colour of its square*. So the check belongs on the move itself, where the start and end squares are known. In Morpholog, a check on a move is a `require`. We call it a gate.

First, copy the programme, so the original stays as it is:

```sh
cp "$M" /tmp/chess_fixed.morph
```

In the copy, add this condition at the top level. Just above `transformation start_game():` is a good place.

```
define keeps_square_colour(piece_type, src_f, src_r, dst_f, dst_r):
  piece_type != #bishop or (src_f + src_r) % 2 = (dst_f + dst_r) % 2
```

Then add this gate to both `quiet_move` and `capturing_move`, just below the line `require piece_color = current_turn`:

```
  require bishop_keeps_its_colour: keeps_square_colour(piece_type, src_f, src_r, dst_f, dst_r)
```

Why add it twice? A rule written as an `invariant` checks every change, whatever made it. A gate checks only the move it sits in. So every move that can carry a bishop needs the gate. The `define` writes the condition once, so the two moves can't drift apart.

Now check the copy:

```sh
morpholog check /tmp/chess_fixed.morph
```

No output means the programme passed Morpholog's checks. Its names, kinds and bindings all fit together. That doesn't mean the rules are the ones you meant. The programme in step 4 passed these checks too.

### 6. Look back at the moves already made

We found a mistake in our rules. What about the moves the referee already accepted?

Before you run anything, make a guess. The referee has committed four actions: the start of the game and three moves. Which of them would the fixed rules refuse?

```sh
morpholog rehearse /tmp/chess_fixed.morph
```

`rehearse` reads every committed action. It decides each one again under the new rules, on the board as it was at the time. It changes nothing. Its answer includes:

```text
"transitions_replayed": 4,
"totals": {
  "same": 3,
  "different_delta": 0,
  "would_reject": 1,
```

A "transition" is one committed action. Three would be decided the same way, and one would be refused. Under `"refusals"`, the answer names `bishop_keeps_its_colour`, the gate you wrote, and the move it would refuse: e3 to d3. The refused move from step 3 isn't counted, because `rehearse` reads only committed actions.

Now look at the board once more:

```sh
morpholog inspect claims --named "$M" --predicate PieceAt --where piece_type=bishop --where color=white
```

The bishop is still on d3. The game hasn't changed. That move really was accepted, under the rules the referee had at the time. And now you also know which move the better rules would refuse, by name. Morpholog gives you both answers, and rewrites neither.

One limit: `rehearse` judges each move on the board the record shows. It can't play out the game that would have followed a refusal. Nobody knows what the players would have done instead.

## What this guarantees, and what it doesn't

**It guarantees** that the referee committed a move only if the move broke none of the rules the referee had. A refused move changes nothing. The record of committed actions stays as it is. `rehearse` can decide those actions again under a new programme. It reports which decisions would change. It also reports any action the new programme can't replay at all, for example one it no longer has.

**It assumes** that `--actor you` is honest. It is a name you typed, and nobody checked it. It also assumes the moves are the ones the players meant. The referee checks the rules, not anyone's intent.

**It does not guarantee** legal chess. Pieces can move anywhere. There is no check or checkmate, no castling, no en passant, no promotion, and only one game at a time. Above all, it can't tell you that your rules are right. Step 4 showed a rule enforced perfectly that still let through a move we didn't want.

## The rules

| Rule | What it says |
| --- | --- |
| `piece_at_unique_by_file_rank` | A square holds at most one piece. It comes from `unique by (file, rank)` on `PieceAt`. |
| `pieces_stand_on_the_board` | File and rank are whole numbers from 1 to 8. |
| `turn_belongs_to_a_player` | It is always white's turn or black's. |
| `exactly_one_white_king` / `exactly_one_black_king` | Each side has exactly one king, so a king can never be taken. |
| `bishops_on_opposite_square_colors` | A side's two bishops stand on different colours. With one left, there is nothing to compare. |
| `piece_count_matches_board` / `board_with_pieces_has_a_counter` | The piece count exists, and it matches the pieces on the board. |
| `at_most_eight_pawns_per_color` | A side has at most eight pawns. |
| `move_count_strictly_increases` | Each move adds exactly one to the move count: `MoveCount(n) and pre(MoveCount(m)) implies n = m + 1`. |
| `turn_alternates` | The turn changes hands on every move: `CurrentTurn(now) and pre(CurrentTurn(prev)) implies now != prev`. |
| `single_capture_per_move` | A move takes at most one piece: the piece count stays the same or drops by one. |

There are three actions: `start_game()`, `quiet_move(src_f, src_r, dst_f, dst_r, new_turn)`, and `capturing_move(...)` with the same arguments. Each move checks its own conditions with `require`. The piece must belong to the player whose turn it is. The next turn must be the other player's. A quiet move must land on an empty square.

## How this page is checked

A test checks every step on this page against the real `morpholog` binary. It plays the same moves, makes the same fix and runs the same reads. It also checks that each command appears on this page exactly as it runs. If the page and the referee ever disagree, the test fails. It skips the setup step, and it writes the fixed copy itself.

The test empties Morpholog's tables in the database that `DATABASE_URL` points to. Use a throwaway database.

```sh
cargo test -p morpholog-cli --test chess_tutorial_e2e
```
