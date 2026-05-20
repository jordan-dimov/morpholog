# Trial Balance as a Derived Claim

The fifth worked example. The first read-side projection in Morpholog: a *governed view* defined alongside the invariants and transformations that govern the underlying claims, enumerated on demand from admitted state.

This example does not introduce its own program. It declares **one derived claim** attached to the [Example 4 (double-entry ledger)](../04_double_entry_ledger/) program. The forcing scenario is the question "what is the balance of every account?" - a read that, before this example, could only be computed in user code outside the governed model.

## The scenario

You have a ledger. You have admitted journal entries through `post_simple_entry` and `post_split_entry`, every entry balanced by the `balanced_posted_entry` invariant, every closed-period violation refused at admission. The question an auditor will inevitably ask is: *what is the balance of each account?*

In a conventional system you write a query. The query lives somewhere outside the rules - in a reporting tool, a BI dashboard, a hand-rolled SQL view. If the rules change, the query may not. The report drifts from the source. Sometimes years later.

In Morpholog the report is admitted state's *governed projection*. The definition lives next to the invariants. The same kernel that admits the underlying claims enumerates the derived ones. The report cannot reflect a journal line that an invariant would have rejected, because the journal line is not there.

## The derived claim

See [`trial_balance.morph`](trial_balance.morph) for the (illustrative) surface syntax.

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

The kernel evaluator enumerates the domain, deduplicates by key tuple, and runs each value expression once per distinct key.

The IR for this declaration lives in [`crates/morpholog-core/src/examples/double_entry_ledger.rs`](../../crates/morpholog-core/src/examples/double_entry_ledger.rs) as `trial_balance_row()`. It is registered on the `double_entry_ledger` program through the `derived_claims` field; that is the only thing that distinguishes Example 5 from Example 4 at the program level.

## What this example proves

- **Read-side projections can be governed by the same model as admitted state.** The derivation is part of the program, not external code.
- **`enumerate_derived` is the smallest possible read primitive.** No materialised storage, no precomputed indices, no SQL generation. The kernel walks admitted claims through the same machinery that evaluates invariants, deduplicates by key, and evaluates each value expression.
- **As-of evaluation falls out for free.** Because the derived claim is a function of admitted state, evaluating it against a *reconstructed* state at a past `transition_id` produces the historical trial balance. The CLI's `--as-of` flag on `inspect derived` shows this in the [README's sixty-second tour](../../README.md).
- **The forcing function for `Expr::Sub`.** Balance is debits *minus* credits; the IR's only arithmetic primitive (`Expr::Sub`) was introduced for this. Addition, multiplication, and division remain deliberately absent until an example demands them.

The full design retrospective - what was considered and rejected, the keys/values/domain shape, the deferred materialisation/recursion/provenance questions - lives in [`docs/forced-by-examples.md`](../../docs/forced-by-examples.md) under the `DerivedClaim` entry.

## How to run it

This example reuses the Example 4 program, so the in-memory and durable tests live there. The dedicated tests for the derived-claim machinery itself:

```bash
# In-memory: pins the keys/values/domain shape, enumeration semantics,
# and the v0 boundary that derived claims do not pollute admitted state.
cargo test -p morpholog-core --test double_entry_ledger derived

# Durable: list_derived against PostgreSQL plus the predicate-scoped
# read optimisation.
DATABASE_URL=postgres:///morpholog_dev \
  cargo test -p morpholog-postgres --test integration -- --test-threads=1 \
    list_derived
```

And from the CLI - this is the form a real user runs:

```bash
morpholog inspect derived double_entry_ledger TrialBalanceRow
morpholog inspect derived double_entry_ledger TrialBalanceRow \
    --as-of <transition_id>
```

The first command returns the current trial balance. The second returns the trial balance *as it stood right after the named transition* - reconstructed from the audit log, with no bitemporal columns anywhere in the schema. That is the most visceral demonstration of why governed read-side projections matter.

---

## Design notes

### Why no dedicated program

Derived claims live on a `Program`, alongside the invariants and transformations they share the predicate vocabulary with. The trial balance is meaningful only against the ledger's `JournalLine` predicate; promoting it to its own program would force re-declaring the ledger's invariants and transformations just to carry the derived claim. The shape we have is the smallest one that the kernel actually requires.

A future example that defines derived claims over predicates from *multiple* existing programs would force a different shape (cross-program derivation, or a "program composition" primitive). That is not on the v0 roadmap.

### Deferred questions

The retrospective in [`docs/forced-by-examples.md`](../../docs/forced-by-examples.md) names six explicitly:

1. **Materialised derived claims.** Today `enumerate_derived` recomputes on every call. For long audit logs and frequent queries, a materialised snapshot will become forced.
2. **Recursion through other derived claims.** One derived claim's body cannot reference another. The shape that would support it is well understood; an example that needs it is not yet here.
3. **Visibility from invariants.** Invariants cannot quantify over derived claims today. The trial-balance use cases do not need it; a future regulatory invariant might.
4. **Provenance.** Which admitted claims contributed to which derived row. A bookkeeping table on the audit side, plus an enumerator extension; deferred until forced.
5. **Persistence.** Derived rows are not persisted; each query recomputes. The materialisation question and the persistence question are the same question.
6. **Predicate-scoped derivation.** Today the read path loads only the predicates a derived claim's body references; this is an adapter-level optimisation, not part of the IR. The CLI's `inspect derived` and the as-of variants both use it transparently.

Each lands when an example genuinely demands it. None is on the next-PR list as of this writing.
