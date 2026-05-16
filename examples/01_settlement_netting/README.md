# Settlement Netting

The first worked example. Demonstrates Morpholog's core promise: a transformation that would produce invalid business state cannot commit.

## The scenario

Two parties have several payable and receivable settlement lines between them. They want to combine these into one *net* settlement — but only if every line is approved, none has already been netted, and the net amount equals exactly the signed sum of the underlying lines.

In a conventional system you would write application code to check those rules, then a reconciliation script to re-check them the next morning. Morpholog makes the rules the law of admission: a netting that doesn't satisfy them cannot enter state at all.

## The program

See [`netting.morph`](netting.morph) for the (illustrative) surface syntax.

Three invariants:

| Invariant | Says |
| --- | --- |
| `net_settlement_has_lines` | Every net settlement must reference at least one settlement line. |
| `net_amount_equals_lines` | The netted amount must equal the signed sum of its lines. |
| `no_double_netting` | A line cannot be netted into more than one settlement. |

One transformation, `create_net_settlement(party_a, party_b, lines)`. It checks that every input line is approved and not yet netted, mints a new settlement subject, computes the total, asserts the net plus each settlement-line link, marks each line netted, and emits a post-commit `NetSettlementCreated` intent.

## How to run it

The program is currently executed as Rust IR (no parser yet):

```bash
cargo test -p morpholog-core propose_
```

That runs three tests:

1. **Happy path** — approved, non-netted lines with a correct sum. Commits.
2. **`require` failure** — a line is already netted. Rolls back *before* any assertions stage. No claims change.
3. **Invariant failure on the candidate state** — pre-state contains an orphan `SettlementLine` for one of the inputs (inconsistent legacy data, no `Netted` claim). The `require` checks all pass, but the candidate state would have two `SettlementLine` claims for the same line under different nets. `no_double_netting` catches it. Atomic rollback. No claims change. No audit. No outbox.

The third test is the load-bearing one. It proves invariants check the *candidate* state, not just the pre-state — so a transformation that *would* amplify inconsistency cannot commit, even when its preconditions individually look fine.

## How it would persist

[`schema.sql`](schema.sql) sketches the PostgreSQL schema (`claims`, `audit`, `outbox`). It applies cleanly to PostgreSQL 17:

```bash
createdb morpholog_dev
psql morpholog_dev -f schema.sql
```

The runtime is not yet wired to the database; the Rust kernel currently runs against in-memory state. Wiring up PostgreSQL is the next milestone — the schema is the agreed shape that wiring will target.
