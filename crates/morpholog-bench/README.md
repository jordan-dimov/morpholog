# morpholog-bench

Synthetic scale-pressure benchmark for the Morpholog runtime. Two scenarios that share the same fixture builder; both target the double-entry ledger example because it is the first program that has both non-trivial write-side invariants and a read-side derived claim.

## Status

Exploratory. The numbers this binary prints are not regression assertions and are not checked into the repo as expected values. The intent is to surface bottlenecks ahead of speculative optimisation, per the project's forced-by-examples discipline.

## What it measures

| Scenario | Setup | Hot path | What it stresses |
|---|---|---|---|
| `write` | N synthetic journal entries inserted via direct SQL across K accounts | one `propose_against_pg(post_simple_entry, ...)` call | `load_state` + invariant evaluation over the full candidate state + commit |
| `read`  | same fixture | inline `list_claims` + `State::from_claims` + `enumerate_derived` with each phase timed separately | the three layers of the read path, so the dominant cost is visible directly |

The fixture distributes journal lines across `K` distinct accounts (default `K = 2`, configurable via `--accounts`). Entry `i` debits `account_{i mod K}` and credits `account_{(i+1) mod K}` for the same amount, so every entry is self-balancing and the `balanced_posted_entry` invariant holds. The trial-balance derived claim produces one row per distinct account that received at least one line; assertions on the read scenario check the loose bound `0 < rows <= K`.

Larger `K` stresses two things at once on the read path: `enumerate_derived`'s grouping (the BTreeSet that orders the K key tuples) and the per-account `Sum` lookups (one per account, each narrowed by the argument-position index on `JournalLine[1] = account`). The write path is largely independent of `K`; varying it mostly changes fixture characteristics.

## Running

```bash
DATABASE_URL=postgres:///morpholog_dev \
  cargo run -p morpholog-bench --release -- write 1000 --reset

DATABASE_URL=postgres:///morpholog_dev \
  cargo run -p morpholog-bench --release -- read 10000 --accounts 100 --reset
```

`--release` matters; debug builds add an order of magnitude that obscures the algorithmic signal.

The bench **truncates the entire `morpholog` schema before each run**. The required `--reset` flag is the acknowledgement: without it the binary refuses to start, so the `DATABASE_URL` env-var fallback cannot silently destroy a database a shell already happens to point at. Do not point it at a database with anything you want to keep.

## Observations (2026-05-17, local PostgreSQL 17)

Run on a developer workstation against a local `morpholog_dev` database. Indicative, not benchmark-grade; reproduce locally for any decision that depends on the numbers.

### Read path phase split

The read scenario now reports `list_claims`, `build_state`, and `enumerate` separately. At `N = 100 000` (300 000 claims):

| K | list_claims | build_state | enumerate | derived rows |
|--:|--:|--:|--:|--:|
| 2 | 1 151 ms | 239 ms | 364 ms | 2 |
| 100 | 1 171 ms | 230 ms | 282 ms | 100 |

Observations:

- `list_claims` dominates (~65% of read time). That's the PostgreSQL fetch + JSONB decode for every claim in the table. **This is the next forced optimization.** The structurally-aware fix is predicate-scoped loading: load only the claims for predicates the derived claim's body actually references. A `SELECT ... WHERE predicate_name = ANY($1)` for the relevant set would skip most of the table for narrow workloads.
- `build_state` is ~14% (~240 ms for 300 000 claims). Builds both indexes from scratch. Linear in claim count and constant per claim.
- `enumerate` is ~20%. Slightly *cheaper* at K=100 than at K=2, which is a positive signal about the argument-position index: the per-account `Sum` for each of 100 accounts touches roughly `2N/K` lines (so K=100 sums each touch ~6 000 lines instead of K=2 sums each touching ~300 000 lines). The argument-position index on `JournalLine[1] = account` is what makes this scale; without it, K=100 would be ~100x slower than K=2 for the enumerate phase.

### Write path

| N | K | fixture_build | propose_one |
|--:|--:|--:|--:|
| 1 000 | 2 | 30 ms | ~15 ms |
| 10 000 | 2 | ~300 ms | ~240 ms |
| 10 000 | 100 | ~520 ms | ~240 ms |
| 100 000 | 100 | ~5 200 ms | ~2 300 ms |

Linear in `N` and essentially independent of `K`. The remaining cost on `propose_one` is `load_state` + the indexed kernel work for `propose` + `INSERT` for claims/audit/outbox + COMMIT. Improvements here likely come from the same `load_state` work as the read path; the kernel itself is no longer the bottleneck at these sizes.

### History

The write path used to be **structurally quadratic**: an early version of this bench surfaced 31 seconds per propose at `N = 10 000`. The predicate-and-argument-position indexed `State` PR that followed brought it down by ~200x. The full history is preserved in the PRs themselves: `#22` introduced this bench and recorded the original quadratic; `#23` indexed `State` and recorded the fix; the next PR (this one) added the `--accounts K` axis and the read-path phase split, plus a small clone-elision in `State::from_claims` worth ~5-10 ms at N=100 000.

The next bench enhancement worth doing would be a workload with many distinct predicates (the current ledger has only `JournalEntry` and `JournalLine`), so the predicate index's narrowing effect becomes visible separately from the argument-position index. Until that scenario exists, the predicate index's value is theoretical for this bench - the argument-position index does all the visible work.
