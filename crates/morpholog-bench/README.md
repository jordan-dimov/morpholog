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

## Observations (2026-05-17, local PostgreSQL 17)

Run on a developer workstation against a local `morpholog_dev` database. Indicative, not benchmark-grade; reproduce locally for any decision that depends on the numbers.

| N | fixture_build | write: propose_one | read: list_derived |
|--:|--:|--:|--:|
| 100 | 13 ms | < 5 ms | 3 ms |
| 1 000 | 34 ms | 14 ms | 24 ms |
| 10 000 | ~300 ms | 142 ms | 346 ms |
| 100 000 | ~3 500 ms | 1 601 ms | 1 878 ms |

Both paths scale roughly linearly in `N`. The write path used to be quadratic; an early version of this bench surfaced that pathology, and the predicate-and-argument-position indexed `State` PR that followed brought it down by ~200x at N=10000. The history of those numbers is preserved in the PRs themselves (`#22` introduced this bench and recorded the original quadratic; `#23` indexed `State` and recorded the fix).

The remaining linear cost on the write path is `load_state` (one `SELECT ... FROM morpholog.claims` over the entire table, one JSONB decode per row), plus `find_claim_matches` over the indexed state for the few JournalLine lookups the invariants actually do. On the read path the cost is the same `load_state` plus `enumerate_derived`'s `Sum` sweeps over the loaded claims; the per-account sums benefit from argument-position indexing, but the domain enumeration scans the full JournalLine bucket once and materialises one `Bindings` HashMap per match, which is what the read-path time is mostly spent on.

The next bottleneck the bench would surface is the read path: either streaming `find_matches` instead of materialising `Vec<Bindings>`, or scoping `load_state` so the kernel only pulls claims for predicates the invariants and the derived claim actually touch. Neither is forced yet at these sizes; the bench is the regression test that will reveal when either becomes acute.
