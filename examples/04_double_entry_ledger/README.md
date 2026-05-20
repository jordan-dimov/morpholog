# Double-Entry Ledger

The accounting example. A textbook business model expressed end to end in Morpholog: posted entries must balance, periods can be closed against further normal posting, closed periods are corrected by restatement that preserves the original record, and the **trial balance** - the canonical read-side report - is a governed view computed from admitted state by the same kernel that admitted it. The report cannot drift from the source.

## The scenario

A business runs an accounting period - say, April 2026 - during which it posts journal entries. Every entry is required to satisfy the fundamental rule of double-entry bookkeeping: the sum of its line debits equals the sum of its line credits. If a posting would violate that rule, it cannot be admitted.

At month-end, finance closes the period. From that moment, no further entries dated within the period may be posted by ordinary means. Errors discovered after close - a miscoded expense, a corrected revenue figure, a tax adjustment - are handled by *restatement*: a separate transformation that admits a new journal entry and records a `Supersedes` link to the original. **The original entry is not erased**; it remains in admitted state as the record of what was filed at the time.

At any moment, an auditor or a controller can ask for the trial balance - debits and credits totalled per account. In a conventional system the trial balance comes out of a reporting tool that lives elsewhere, governed by whichever query language the report writer happened to know. Morpholog answers it differently: the trial balance is a *governed view* declared alongside the invariants and transformations, computed on demand by the kernel from the same admitted claims. The report cannot reflect a journal line that an invariant would have rejected, because the journal line is not there. And because the runtime can reconstruct admitted state at any past `transition_id`, the trial balance can be evaluated *as it stood* at any prior moment in the audit log - without a single bitemporal column in the schema.

When an external auditor asks *"what did you report at quarter-end, and what did you subsequently restate, and why?"*, the answers are structured queries over claims and one derived view, not detective work in change-log tables.

## The program

See [`ledger.morph`](ledger.morph) for the (illustrative) surface syntax.

### Claims

| Predicate | Role |
| --- | --- |
| `JournalEntry(entry_id, posting_date, period)` | The header for a posted entry. **Append-only.** |
| `JournalLine(entry_id, account, debit, credit)` | One line of an entry. The supplied transformations always emit either a debit amount with `credit = 0` or a credit amount with `debit = 0`; per-line shape (e.g. "exactly one side is non-zero") is not enforced by an invariant. **Append-only.** |
| `PeriodClosed(period)` | Marks that a period has been closed against further normal posting. **Append-only and terminal in v0** - there is no `reopen_period`. |
| `Supersedes(new_entry_id, prior_entry_id)` | Restatement lineage. **Append-only.** |

The append-only discipline is total here. Every predicate is content, terminal state, or lineage - none are retractable pointers. Callers that want "the current version of this entry" walk the `Supersedes` chain from any starting entry. Adding a `CurrentEntry` pointer is a future option, not a v0 requirement.

### Invariants

| Invariant | Says |
| --- | --- |
| `balanced_posted_entry` | For every `JournalEntry`, the sum of line debits equals the sum of line credits. The fundamental accounting equation, evaluated as `Eq(Sum, Sum)` over the entry's `JournalLine` claims. |
| `journal_entry_has_lines` | Every `JournalEntry` must have at least one matching `JournalLine`. Without this, a zero-line entry would trivially satisfy `balanced_posted_entry` (both sums = 0). Closes the gap explicitly, because the runtime's contract is "candidate state is admissible under invariants," not "our transformations happen to be well behaved." |
| `at_most_one_direct_successor` | A `JournalEntry` can be superseded by at most one direct restatement (no parallel restatements). |

**What is deliberately not an invariant:** there is no rule saying "no `JournalEntry` exists with a closed-period `PeriodClosed`". Period-close gating lives in `require` on the posting transformations, not in an invariant. If it were an invariant, closing a period would force either (a) rejection of the close (because pre-close historical postings now violate the rule), or (b) cascade-retraction of those historical postings (which destroys the record). The `require` formulation matches the real-world semantics: *postings made before close stay valid; postings attempted after close are rejected at admission time*. Same lesson as the rest of the project: authority and admission gates live in `require`, not in invariants.

### Transformations

| Transformation | Effect |
| --- | --- |
| `post_simple_entry` | Posts a two-line balanced entry (one debit, one credit, same amount). Structurally guaranteed to balance. Rejected if the period is closed. |
| `post_split_entry` | Posts a three-line entry (one debit, two credits with independent amounts). The balance invariant catches arithmetic mismatches on the candidate state. Rejected if the period is closed. |
| `close_period` | Asserts `PeriodClosed(period)`. Rejects double-closing. |
| `restate_entry` | Admits a new entry with new lines, plus a `Supersedes(new, prior)` link. Requires the prior entry to exist and not already be superseded. Does *not* check `PeriodClosed` - restatement is the path for closed periods. |

The split between `post_simple_entry` (guaranteed-balanced) and `post_split_entry` (must be checked) is intentional: the first exercises the period-close gate cleanly; the second exercises the balance invariant in earnest.

### Read-side: the trial balance as a derived claim

```
derived TrialBalanceRow(account, balance)
    where balance ==
        sum { d | JournalLine(_, account, d, _) }
      - sum { c | JournalLine(_, account, _, c) }
    over JournalLine(_, account, _, _)
```

The shape is **keys, values, domain**:

- `keys: [account]` - one row per distinct account.
- `values: [balance = sum(debits) - sum(credits)]` - one computed value per row.
- `domain: JournalLine(_, account, _, _)` - the expression whose satisfying bindings define the set of distinct keys.

The kernel evaluator enumerates the domain, deduplicates by key tuple, and runs the value expression once per distinct key. No materialised storage, no precomputed indices, no SQL generation - the same machinery that evaluates invariants enumerates the derived view.

The corresponding IR is in [`crates/morpholog-core/src/examples/double_entry_ledger.rs`](../../crates/morpholog-core/src/examples/double_entry_ledger.rs) as `trial_balance_row()`, registered on the program through the `derived_claims` field.

## How to run it

The same scenario is proven at two layers - in-memory through the sync kernel, and durably through the PostgreSQL adapter. The read-side projection is exercised at both layers too.

### In-memory (sync kernel)

```bash
cargo test -p morpholog-core --test double_entry_ledger
```

In-memory tests:

1. **`simple_entry_balances_and_commits`** - happy path: cash debit 100, revenue credit 100. Final state has one `JournalEntry` and two `JournalLine`s.
2. **`split_entry_balances_and_commits`** - cash debit 100, revenue credit 70 + deferred revenue credit 30. Balance holds.
3. **`unbalanced_entry_rejected_by_invariant`** - debits = 100, credits = 70 + 25 = 95. The `balanced_posted_entry` invariant catches the 5-unit mismatch on the candidate state. Atomic rollback.
4. **`closed_period_rejects_normal_posting`** - close period, try to post an entry into it. `require not PeriodClosed` catches it at admission.
5. **`double_close_rejected`** - close the same period twice. Second close rejected.
6. **`restatement_into_closed_period_preserves_original`** - **the load-bearing test.** Post -> close -> restate. The original entry header and lines remain in admitted state. The new entry is present at the corrected amount. `Supersedes(new, prior)` is recorded. The period stays closed.
7. **`lone_journal_entry_without_lines_violates_invariant`** - evaluates `journal_entry_has_lines` directly against a hand-crafted state that no legitimate transformation could produce.
8. **`cannot_restate_already_restated_entry`** - the at-most-one-direct-successor restriction blocks parallel restatement chains.

Plus tests pinning `TrialBalanceRow`: the keys/values/domain shape, the enumeration semantics, the v0 boundary that derived claims do not pollute admitted state.

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
    double_entry ledger_closed_period list_derived
```

The corresponding integration tests in `crates/morpholog-postgres/tests/integration.rs`:

- `double_entry_full_chain_through_pg` - the full post -> close -> restate sequence through `propose_against_pg`. Verifies the final DB state: claim set, audit rows, outbox intents in causal order, original entry preserved, restatement entry present, `Supersedes` link recorded.
- `ledger_closed_period_rejects_new_entry_and_writes_nothing` - pre-state with `PeriodClosed` admitted; a new posting against the closed period is rejected and leaves all three tables at their pre-state row counts.
- Tests covering `list_derived` against the same ledger state, including the predicate-scoped read path (only `JournalLine` rows are fetched when evaluating `TrialBalanceRow`).

### CLI

The `morpholog inspect` command exposes the read side directly:

```bash
morpholog inspect derived double_entry_ledger TrialBalanceRow
```

returns the current trial balance, one row per account. Add `--as-of <transition_id>` to get the trial balance *as it stood right after that transition*:

```bash
morpholog inspect derived double_entry_ledger TrialBalanceRow \
    --as-of 019231ab-...-...-...-...-...
```

The runtime replays the audit log up to the named transition and evaluates the derived claim against the reconstructed state. No bitemporal columns in the schema; the audit log is enough.

---

## Design notes

### What this example proves about the doctrine

- **Balance is `Eq(Sum, Sum)`** - the existing aggregation and comparison primitives compose to express the fundamental accounting equation. No accounting-specific machinery needed.
- **Period close is admission-gating via `require`**, not via an invariant. Same lesson as Examples 2 and 3: an invariant tying decisions to live state would either reject the close or cascade-retract history.
- **Restatement reuses `Supersedes`** with no shape changes - the same lineage primitive that handles bank-recognition restatement handles journal-entry restatement.
- **Append-only / retractable / append-only** holds: every predicate is content (`JournalEntry`, `JournalLine`), terminal state (`PeriodClosed`), or lineage (`Supersedes`). No retractable pointers.
- **Read-side projections are governed**. `TrialBalanceRow` is part of the program, not an external query. The same kernel that admitted the underlying claims enumerates the derived view, so a journal line an invariant would have rejected cannot appear in the trial balance - the journal line is not there to be summed.
- **As-of evaluation falls out of derived claims for free**. Because the derived claim is a function of admitted state, evaluating it against a reconstructed historical state produces the historical report. No new write-side machinery; the audit log already contains everything needed.

### What this example deliberately does not cover

1. **N-line journal entries via collection iteration.** Real journal entries can have arbitrarily many lines (e.g., a payroll posting with hundreds of employee lines). The Example 1 settlement-netting transformation iterates over a collection via `Stmt::For`; the same pattern would extend `post_simple_entry`/`post_split_entry` into a single `post_entry(entry_id, period, line_ids)` form. Deferred because the current variants already exercise the load-bearing invariants.
2. **Current-version pointer for restated entries.** Callers walk the `Supersedes` chain. A `CurrentEntry(original_id, current_id)` pointer claim could be added but duplicates information already in the lineage.
3. **Admissibility-for-purpose** (Example 3's pattern). A real ledger has multiple reporting purposes - statutory accounts, tax computation, management reporting, group consolidation - and the same posted entry can be admissible for some but not others. The `AdmissibleFor` pattern would layer cleanly on top.
4. **Period-order awareness.** "This period precedes that period" is not modelled. Real systems often need ordering (e.g., "you cannot close Q2 until Q1 is closed"). A `PeriodFollows` claim and an invariant over it would handle this; deferred until forced.
5. **Account-balance derived claim keyed on date.** `TrialBalanceRow` keys on `account` only. A balance-as-of-date variant would key on `(account, date)` and aggregate only lines posted on or before the given date. Deferred until forced; the current `--as-of <transition_id>` CLI already answers the related "as of which transition?" question.
6. **Reopen of closed periods.** `PeriodClosed` is terminal in v0. Real systems sometimes need to reopen for grossly material errors; this would require a `PeriodReopened` claim or a separate authority pattern.
7. **Materialised trial balance.** `enumerate_derived` recomputes on every call. For long audit logs and frequent queries, a materialised snapshot will become forced. None of the current tests run slowly enough to make this pressing.
8. **Recursion through derived claims.** One derived claim's body cannot reference another. The shape that would support it is well understood; an example that needs it is not yet here.

### Restating an open-period entry

The `restate_entry` transformation deliberately does *not* check whether the period is closed. Restating an entry posted earlier in an open period is also a valid use case (e.g., correcting a same-period error before close). The `at_most_one_direct_successor` invariant ensures only one restatement chain per original entry, regardless of whether the period was open or closed when restatement happened.
