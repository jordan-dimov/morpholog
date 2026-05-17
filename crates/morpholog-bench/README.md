# morpholog-bench

Synthetic scale-pressure benchmark for the Morpholog runtime. Two scenarios that share the same fixture builder; both target the double-entry ledger example because it is the first program that has both non-trivial write-side invariants and a read-side derived claim.

## Status

Exploratory. The numbers this binary prints are not regression assertions and are not checked into the repo as expected values. The intent is to surface bottlenecks ahead of speculative optimisation, per the project's forced-by-examples discipline.

## What it measures

| Scenario | Setup | Hot path | What it stresses |
|---|---|---|---|
| `write` | N synthetic journal entries inserted via direct SQL | one `propose_against_pg(post_simple_entry, ...)` call | `load_state` + invariant evaluation over the full candidate state + commit |
| `read`  | same fixture | one `list_derived(trial_balance_row)` call | `load_state` + `enumerate_derived` (find_matches scans + grouping) |

The fixture is uniform on purpose: every entry debits `account_cash` and credits `account_revenue` for the same amount, so trial balance always produces exactly two rows. That holds `enumerate_derived`'s grouping cost small and concentrates the read-path cost in load + sum sweeps. A future scenario that distributes lines across many accounts would shift the dominant cost into grouping; not in scope for v1.

## Running

```bash
DATABASE_URL=postgres:///morpholog_dev cargo run -p morpholog-bench --release -- write 1000 --reset
DATABASE_URL=postgres:///morpholog_dev cargo run -p morpholog-bench --release -- read 10000 --reset
```

`--release` matters; debug builds add an order of magnitude that obscures the algorithmic signal.

The bench **truncates the entire `morpholog` schema before each run**. The required `--reset` flag is the acknowledgement: without it the binary refuses to start, so the `DATABASE_URL` env-var fallback cannot silently destroy a database a shell already happens to point at. Do not point it at a database with anything you want to keep.

## Initial observations (2026-05-17, local PostgreSQL 17)

Run on a developer workstation against a local `morpholog_dev` database. Indicative, not benchmark-grade; reproduce locally for any decision that depends on the numbers.

| N | fixture_build | write: propose_one | read: list_derived |
|--:|--:|--:|--:|
| 100 | 13 ms | 6 ms | 3 ms |
| 1 000 | 34 ms | 310 ms | 24 ms |
| 10 000 | 313 ms | 30 945 ms | 332 ms |
| 100 000 | 3 509 ms | (not measured) | 1 659 ms |

Two patterns are visible:

- **Read scales roughly linearly.** `list_derived` cost is dominated by `load_state` (fetch every row, decode JSONB) and `enumerate_derived`'s two `Sum` sweeps over the loaded claims (one per derived value side). Linear in `N`, ~30 microseconds per entry at 10K-100K. Survivable for mid-size states; uncomfortable past 1M.
- **Write looks ~quadratic.** N=100 -> 6 ms, N=1000 -> 310 ms, N=10000 -> 30 945 ms. The cause is structural: `propose_against_pg` re-evaluates every invariant against the full candidate state, and `balanced_posted_entry` is `forall entry: sum(debits) == sum(credits)`. Each pre-existing entry contributes a forall iteration; each iteration does a sum sweep over the full JournalLine set. Cost is approximately O(invariants * entries * journal_lines), and with two journal lines per entry that's O(N^2) for the balanced check alone. 31 seconds for one proposal at 10K entries is the lower bound; at 100K it would be untenable.

The bench was the smallest scaffold needed to see that. The next move is the optimisation work that the numbers force: predicate-indexed `State` to cut the per-lookup cost, predicate-scoped `load_state` to avoid pulling unrelated claims into memory, and relevant-invariant pruning to skip invariants whose predicate footprint does not overlap with the proposed transformation.
