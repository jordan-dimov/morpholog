# Double-Entry Ledger

The fourth worked example. Demonstrates Morpholog against the canonical accounting domain: posted entries must balance, periods can be closed against further normal posting, and closed periods are corrected by restatement that preserves the original record.

## The scenario

A business runs an accounting period - say, April 2026 - during which it posts journal entries. Every entry is required to satisfy the fundamental rule of double-entry bookkeeping: the sum of its line debits equals the sum of its line credits. If a posting would violate that rule, it cannot be admitted.

At month-end, finance closes the period. From that moment, no further entries dated within the period may be posted by ordinary means. Errors discovered after close - a miscoded expense, a corrected revenue figure, a tax adjustment - are handled by *restatement*: a separate transformation that admits a new journal entry and records a `Supersedes` link to the original. **The original entry is not erased**; it remains in admitted state as the record of what was filed at the time.

When an external auditor asks *"what did you report at quarter-end, and what did you subsequently restate, and why?"* the answers are structured queries over claims, not detective work in change-log tables.

## The program

See [`ledger.morph`](ledger.morph) for the (illustrative) surface syntax.

### Claims

| Predicate | Role |
| --- | --- |
| `JournalEntry(entry_id, posting_date, period)` | The header for a posted entry. **Append-only.** |
| `JournalLine(entry_id, account, debit, credit)` | One line of an entry. The supplied transformations always emit either a debit amount with `credit = 0` or a credit amount with `debit = 0`; per-line shape (e.g. "exactly one side is non-zero") is not currently enforced by an invariant. **Append-only.** |
| `PeriodClosed(period)` | Marks that a period has been closed against further normal posting. **Append-only and terminal in v0** - there is no `reopen_period`. |
| `Supersedes(new_entry_id, prior_entry_id)` | Restatement lineage. **Append-only.** |

The append-only discipline (per [`docs/forced-by-examples.md`](../../docs/forced-by-examples.md)) is total here. Every predicate is content, terminal state, or lineage - none are retractable pointers. The Example 2 retractable-pointer pattern (`CurrentBankRecognition`) is deliberately *not* used: callers that want "the current version of this entry" can walk the `Supersedes` chain from any starting entry. Adding a `CurrentEntry` pointer is a future option, not a v0 requirement.

### Invariants

| Invariant | Says |
| --- | --- |
| `balanced_posted_entry` | For every `JournalEntry`, the sum of line debits equals the sum of line credits. The fundamental accounting equation, evaluated as `Eq(Sum, Sum)` over the entry's `JournalLine` claims. |
| `journal_entry_has_lines` | Every `JournalEntry` must have at least one matching `JournalLine`. Without this, a zero-line entry would trivially satisfy `balanced_posted_entry` (both sums = 0). Closes the gap explicitly because the runtime's contract is "candidate state is admissible under invariants," not "our transformations happen to be well behaved." |
| `at_most_one_direct_successor` | A `JournalEntry` can be superseded by at most one direct restatement (no parallel restatements). Same shape as Example 2. |

**Note what is deliberately not an invariant:** there is no rule saying "no `JournalEntry` exists with a closed-period `PeriodClosed`". This is the same load-bearing design choice from Example 3 - period-close gating lives in `require` on the posting transformations, not in an invariant. If it were an invariant, closing a period would force either (a) rejection of the close (because pre-close historical postings now violate the rule), or (b) cascade-retraction of those historical postings (which destroys the record). The `require` formulation matches the real-world semantics: *postings made before close stay valid; postings attempted after close are rejected at admission time*.

### Transformations

| Transformation | Effect |
| --- | --- |
| `post_simple_entry` | Posts a two-line balanced entry (one debit, one credit, same amount). Structurally guaranteed to balance. Rejected if the period is closed. |
| `post_split_entry` | Posts a three-line entry (one debit, two credits with independent amounts). The balance invariant catches arithmetic mismatches on the candidate state. Rejected if the period is closed. |
| `close_period` | Asserts `PeriodClosed(period)`. Rejects double-closing. |
| `restate_entry` | Admits a new entry with new lines, plus a `Supersedes(new, prior)` link. Requires the prior entry to exist and to not already be superseded. Does *not* check `PeriodClosed` - restatement is the path for closed periods. |

The split between `post_simple_entry` (guaranteed-balanced) and `post_split_entry` (must be checked) is intentional: the first exercises the period-close gate cleanly; the second exercises the balance invariant in earnest. Real systems would generalise to N-line entries (see Design notes below); two transformations is sufficient for the worked example.

## How to run it

The same scenario is proven at two layers - in-memory through the sync kernel, and durably through the PostgreSQL adapter.

### In-memory (sync kernel)

```bash
cargo test -p morpholog-core --test double_entry_ledger
```

Eight tests:

1. **`simple_entry_balances_and_commits`** - happy path: cash debit 100, revenue credit 100. Final state has 1 `JournalEntry` and 2 `JournalLine`s.
2. **`split_entry_balances_and_commits`** - cash debit 100, revenue credit 70 + deferred revenue credit 30. Balance holds; final state has 1 `JournalEntry` and 3 `JournalLine`s.
3. **`unbalanced_entry_rejected_by_invariant`** - debits = 100, credits = 70 + 25 = 95. The `balanced_posted_entry` invariant catches the 5-unit mismatch on the candidate state. Atomic rollback.
4. **`closed_period_rejects_normal_posting`** - close period, try to post an entry into it. `require not PeriodClosed` catches it at admission.
5. **`double_close_rejected`** - close the same period twice. Second close rejected.
6. **`restatement_into_closed_period_preserves_original`** - **the load-bearing test.** Post → close → restate. The original entry header and lines remain in admitted state. The new entry is present at the corrected amount. `Supersedes(new, prior)` is recorded. The period stays closed. Final state has 8 claims.
7. **`lone_journal_entry_without_lines_violates_invariant`** - evaluates `journal_entry_has_lines` directly against a hand-crafted state that no legitimate transformation could produce, to pin the contract that a `JournalEntry` cannot exist without lines.
8. **`cannot_restate_already_restated_entry`** - the at-most-one-direct-successor restriction blocks parallel restatement chains.

### Durable (PostgreSQL adapter)

First-time setup (skip if the schema is already applied):

```bash
createdb morpholog_dev
psql morpholog_dev -f crates/morpholog-core/sql/schema.sql
```

Then:

```bash
DATABASE_URL=postgres:///morpholog_dev \
  cargo test -p morpholog-postgres --test integration -- --test-threads=1 \
    double_entry ledger_closed_period
```

Two integration tests in `crates/morpholog-postgres/tests/integration.rs`:

- `double_entry_full_chain_through_pg` - the full post → close → restate sequence through `propose_against_pg`. Verifies final DB state: 8 claims, 3 audit rows, 3 outbox intents in causal order, original entry and lines preserved, restatement entry and lines present, `Supersedes` link recorded.
- `ledger_closed_period_rejects_new_entry_and_writes_nothing` - pre-state with `PeriodClosed` admitted; a new posting against the closed period is rejected and leaves all three tables at their pre-state row counts.

---

## Design notes

### What this example proves about the doctrine

The retrospective doc [`docs/forced-by-examples.md`](../../docs/forced-by-examples.md) predicted this example would *reuse* existing affordances and *not* force new IR primitives. That prediction held:

- **Balance is `Eq(Sum, Sum)`.** The existing `Expr::Sum` and `Expr::Eq` compose directly. `eval_value` already handles `Expr::Sum`; `Eq` evaluates both sides through `eval_value`; the comparison is decimal-vs-decimal equality. No new aggregation, comparison, or arithmetic primitive needed.
- **Period close is admission-gating via `require`**, exactly as the require-vs-invariant section of `forced-by-examples.md` prescribes.
- **Restatement reuses `Supersedes`** from Example 2 with no shape changes.
- **The append-only / retractable / append-only three-bucket classification** holds: every predicate in this example is content (`JournalEntry`, `JournalLine`), terminal state (`PeriodClosed`), or lineage (`Supersedes`). No retractable pointers.

The example earns its place by exercising these patterns under a domain that is universally recognisable to anyone who has worked with accounting software.

### What this example deliberately does not cover

1. **N-line journal entries via collection iteration.** Real journal entries can have arbitrarily many lines (e.g., a payroll posting with hundreds of employee lines). The Example 1 settlement-netting transformation iterates over a collection via `Stmt::For` and pre-staged per-line claims; the same pattern would extend `post_simple_entry`/`post_split_entry` into a single `post_entry(entry_id, period, line_ids)` form. Deferred because the two-and-three-line variants already exercise the load-bearing invariants.
2. **Current-version pointer for restated entries.** Callers that want "the current version of an entry" walk the `Supersedes` chain from any starting point. A `CurrentEntry(original_id, current_id)` pointer claim (Example 2's pattern) could be added, but it duplicates information already in the lineage and is not load-bearing for this example.
3. **Admissibility-for-purpose** (Example 3's pattern). A real ledger has multiple reporting purposes - statutory accounts, tax computation, management reporting, group consolidation - and the same posted entry can be admissible for some but not others. The Example 3 `AdmissibleFor` pattern would layer cleanly on top of this example. Deferred for scope; the close + restatement story is the focus here.
4. **Period-order awareness.** "This period precedes that period" is not modelled. In v0, periods are opaque subjects with no temporal relationship encoded. Real systems often need ordering (e.g., "you cannot close Q2 until Q1 is closed"). A `PeriodFollows(p_next, p_prev)` claim and an invariant over it would handle this; deferred until a real example forces it.
5. **Account-balance derived claims.** "What is the balance of account X as of date D?" is a derived-claim question. Derived claims are named as a future affordance in [`docs/scope-and-ambition.md`](../../docs/scope-and-ambition.md) and are deliberately not built yet.
6. **Reopen of closed periods.** `PeriodClosed` is terminal in v0. Real systems sometimes need to reopen for grossly material errors; this would require a `PeriodReopened` claim or a separate authority pattern. Deferred until a real example forces it.

### Restating an open-period entry

The `restate_entry` transformation deliberately does *not* check whether the period is closed. Restating an entry posted earlier in an open period is also a valid use case (e.g., correcting a same-period error before close). The `at_most_one_direct_successor` invariant ensures only one restatement chain per original entry, regardless of whether the period was open or closed when restatement happened.

### Where this fits in the arc

Examples 1-3 each forced a specific semantic or kernel addition (the runtime, then bitemporal correction, then standing plus `Value::Subject`). Example 4 forces nothing. That is itself informative: the accumulated affordances are now sufficient to express a textbook accounting workflow. The next semantic frontier - derived claims for read-side projections like trial balance - is what Example 5 will push on.
