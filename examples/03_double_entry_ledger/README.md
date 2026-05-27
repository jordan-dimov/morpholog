# Double-Entry Ledger

A textbook accounting model expressed end to end: posted entries must balance, periods close against further posting, closed periods are corrected by restatement that preserves the original record, and the **trial balance** is a governed view computed by the same kernel that admitted the underlying claims.

## The scenario

A business posts journal entries through a period. Every entry must satisfy double-entry bookkeeping: sum of line debits equals sum of line credits. A posting that would violate that rule is refused at admission.

At month-end, finance closes the period. From that moment, no further entries dated within the period may be posted by ordinary means. Errors discovered after close are handled by *restatement*: a separate transformation admits a new entry and records a `Supersedes` link to the original. The original entry is not erased; it remains in admitted state as the record of what was filed.

At any moment, an auditor can ask for the trial balance - debits and credits totalled per account. It is not an external query; it is a *governed view* declared alongside the rules, computed on demand by the same kernel from the same admitted claims. A line an invariant would have refused cannot appear in the trial balance, because that line is not there. And because the runtime can reconstruct admitted state at any past `transition_id`, the trial balance can be evaluated *as it stood* at any prior moment - without a bitemporal column in the schema.

## The program

See [`ledger.morph`](ledger.morph) for the surface syntax.

### Claims

| Predicate | Role |
| --- | --- |
| `JournalEntry(entry_id, posting_date, period)` | The header for a posted entry. Append-only. |
| `JournalLine(entry_id, account, debit, credit)` | One line of an entry. Append-only. Per-line shape (e.g. "exactly one side is non-zero") is not enforced. |
| `PeriodClosed(period)` | The period is closed against further normal posting. Append-only and **terminal in v0** - no `reopen_period`. |
| `Supersedes(new_entry_id, prior_entry_id)` | Restatement lineage. Append-only. |

Every predicate here is content, terminal state, or lineage - no retractable pointers. Callers who want "the current version" walk the `Supersedes` chain.

### Invariants

| Invariant | Says |
| --- | --- |
| `balanced_posted_entry` | For every `JournalEntry`, sum of line debits equals sum of line credits. `Eq(Sum, Sum)` over the entry's lines. |
| `journal_entry_has_lines` | Every `JournalEntry` must have at least one `JournalLine`. Without this, a zero-line entry would trivially satisfy `balanced_posted_entry`. |
| `at_most_one_direct_successor` | A `JournalEntry` can be superseded by at most one direct restatement; parallel chains are forbidden. |

**No invariant ties `JournalEntry` to `PeriodClosed`.** Period-close gating lives in `require` on the posting transformations, not in an invariant. As an invariant it would force either rejecting close (because pre-close postings would now violate the rule) or cascade-retracting historical postings. The `require` formulation matches the business: postings made before close stay valid; postings attempted after close are rejected at admission time.

### Transformations

| Transformation | Effect |
| --- | --- |
| `post_simple_entry` | Two-line balanced entry (one debit, one credit, same amount). Structurally balanced. Rejected if the period is closed. |
| `post_split_entry` | Three-line entry (one debit, two credits with independent amounts). The balance invariant catches arithmetic mismatches on the candidate state. Rejected if the period is closed. |
| `close_period` | Asserts `PeriodClosed(period)`. Rejects double-closing. |
| `restate_entry` | Admits a new entry + `Supersedes(new, prior)`. Requires the prior entry to exist and not already be superseded. Does *not* check `PeriodClosed` - restatement is the path for closed periods. |

The split between `post_simple_entry` (guaranteed-balanced) and `post_split_entry` (must be checked) is deliberate: the first exercises the period-close gate cleanly; the second exercises the balance invariant in earnest.

### Read-side: the trial balance as a derived claim

```
derived TrialBalanceRow(account, balance)
    where balance ==
        sum { d | JournalLine(_, account, d, _) }
      - sum { c | JournalLine(_, account, _, c) }
    over JournalLine(_, account, _, _)
```

The shape is **keys, values, domain**: one row per distinct account; balance = sum(debits) - sum(credits) per account; the domain expression defines which accounts the row applies to. The kernel evaluator enumerates the domain, deduplicates by key tuple, and runs the value expression once per distinct key. No materialised storage, no precomputed indices - the same machinery that evaluates invariants enumerates the derived view.

## How to run it

```bash
# In-memory
cargo test -p morpholog-examples --test double_entry_ledger
cargo test -p morpholog-examples --test derived_claims

# Durable (PostgreSQL adapter)
DATABASE_URL=postgres:///morpholog_dev \
  cargo test -p morpholog-postgres --test integration -- --test-threads=1 \
    double_entry_full_chain_through_pg \
    ledger_closed_period_rejects_new_entry_and_writes_nothing \
    list_derived_trial_balance_over_pg_ledger_state \
    list_derived_on_empty_state_returns_no_rows \
    list_derived_ignores_claims_outside_its_predicate_footprint
```

In-memory tests cover: happy paths (simple and split entries balance and commit); rejection of unbalanced entries by the candidate-state invariant; period-close gate; double-close rejection; **restatement into closed periods preserves the original** (the load-bearing test); the empty-entry invariant; the at-most-one-direct-successor restriction. Read-side tests pin the `TrialBalanceRow` shape, enumeration semantics, and the v0 boundary that derived claims do not pollute admitted state.

The PG integration test walks the full post -> close -> restate sequence through `propose_against_pg`, verifying claim set, audit rows, outbox intents in causal order, and the supersession lineage. Separate tests cover `list_derived` against the same ledger state.

### CLI

```bash
# Current trial balance
morpholog inspect derived examples/03_double_entry_ledger/ledger.morph TrialBalanceRow

# As it stood at a past transition
morpholog inspect derived examples/03_double_entry_ledger/ledger.morph TrialBalanceRow \
    --as-of 019231ab-...-...-...-...-...
```

The runtime replays the audit log up to the named transition and evaluates the derived claim against the reconstructed state. No bitemporal columns; the audit log is enough.

---

## Design notes

### What this example proves about the doctrine

- **Balance is `Eq(Sum, Sum)`** - existing primitives compose; no accounting-specific machinery needed.
- **Period close is admission-gating via `require`**, not via an invariant - the require-vs-invariant lesson again.
- **Restatement reuses `Supersedes`** - the same lineage primitive that handles verification restatement handles journal-entry restatement.
- **Read-side projections are governed.** `TrialBalanceRow` is part of the program; the same kernel that admitted the underlying claims enumerates the derived view.
- **As-of falls out for free.** Because the derived claim is a function of admitted state, evaluating it against a reconstructed historical state produces the historical report.

### What this example deliberately does not cover

- **N-line journal entries via collection iteration.** Real payroll postings have hundreds of lines. The `Stmt::For` pattern from settlement netting would generalise `post_*_entry` into one `post_entry(entry_id, period, line_ids)`. The current two/three-line variants exercise the load-bearing invariants.
- **Current-version pointer for restated entries.** Callers walk the `Supersedes` chain. A `CurrentEntry(original_id, current_id)` pointer would duplicate information already in the lineage.
- **Admissibility-for-purpose.** A real ledger has statutory, tax, management, and consolidation reporting purposes; the same entry can be admissible for some but not others. The `AdmissibleFor` pattern from [`verified_revenue`](../02_verified_revenue/) would layer cleanly on top.
- **Period-order awareness.** "You cannot close Q2 until Q1 is closed" is not modelled. A `PeriodFollows` claim and invariant would handle this.
- **Account-balance derived claim keyed on date.** `TrialBalanceRow` keys on `account` only. A balance-as-of-date variant would key on `(account, date)`. The current `--as-of <transition_id>` already answers the related "as of which transition?" question.
- **Reopen of closed periods.** `PeriodClosed` is terminal in v0.
- **Materialised trial balance.** `enumerate_derived` recomputes on every call.
- **Recursion through derived claims.** One derived claim's body cannot reference another.

### Restating an open-period entry

`restate_entry` deliberately does *not* check whether the period is closed. Restating an entry posted earlier in an open period is a valid use case (correcting a same-period error before close). The `at_most_one_direct_successor` invariant ensures only one chain per original entry regardless.
