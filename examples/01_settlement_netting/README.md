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

The program is executed as Rust IR (no parser yet). Two layers of tests prove the same example:

### In-memory (sync kernel)

```bash
cargo test -p morpholog-core propose_
```

Three tests:

1. **Happy path** — approved, non-netted lines with a correct sum. Commits.
2. **`require` failure** — a line is already netted. Rolls back *before* any assertions stage. No claims change.
3. **Invariant failure on the candidate state** — pre-state contains an orphan `SettlementLine` for one of the inputs (inconsistent legacy data, no `Netted` claim). The `require` checks all pass, but the candidate state would have two `SettlementLine` claims for the same line under different nets. `no_double_netting` catches it. Atomic rollback. No claims change. No audit. No outbox.

The third test is the load-bearing one. It proves invariants check the *candidate* state, not just the pre-state — so a transformation that *would* amplify inconsistency cannot commit, even when its preconditions individually look fine.

### Durable (PostgreSQL adapter)

The same scenario, end-to-end through `propose_against_pg`, with the resulting claims, audit row, and outbox intent verified in the database:

```bash
createdb morpholog_dev
psql morpholog_dev -f crates/morpholog-core/sql/schema.sql

DATABASE_URL=postgres:///morpholog_dev \
  cargo test -p morpholog-postgres --test integration -- --test-threads=1 \
    settlement_netting require_failure invariant_violation
```

The three filter words (`settlement_netting`, `require_failure`, `invariant_violation`) each match one of the three settlement-related tests below; cargo's test runner includes any test whose name contains any of the listed substrings.

Three integration tests in `crates/morpholog-postgres/tests/integration.rs`:

- `settlement_netting_happy_path_commits_claims_audit_and_outbox` — the happy path, durable. Verifies that on commit the claim mutations, the audit row, and the outbox intent all land in one PostgreSQL `SERIALIZABLE` transaction.
- `require_failure_writes_nothing` — durable counterpart of the in-memory `require` failure case. All three tables unchanged.
- `invariant_violation_on_candidate_state_writes_nothing` — durable counterpart of the candidate-state invariant failure. All three tables unchanged.

The shared schema (`claims`, `audit`, `outbox`) is **canonical runtime infrastructure**, not example-specific. It lives at [`crates/morpholog-core/sql/schema.sql`](../../crates/morpholog-core/sql/schema.sql).
