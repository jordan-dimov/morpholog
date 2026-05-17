# morpholog-bench

Synthetic scale-pressure benchmark for the Morpholog runtime. Two scenarios that share the same fixture builder; both target the double-entry ledger example because it is the first program that has both non-trivial write-side invariants and a read-side derived claim.

## Status

Exploratory. The numbers this binary prints are not regression assertions and are not checked into the repo as expected values. The intent is to surface bottlenecks ahead of speculative optimisation, per the project's forced-by-examples discipline.

## What it measures

| Scenario | Setup | Hot path | What it stresses |
|---|---|---|---|
| `write` | N synthetic journal entries inserted via direct SQL across K accounts | one `propose_against_pg(post_simple_entry, ...)` call | `load_state` + invariant evaluation over the full candidate state + commit |
| `read`  | same fixture | inline `list_claims` + `State::from_claims` + `enumerate_derived` with each phase timed separately | the three layers of the read path, so the dominant cost is visible directly |
| `as-of` | N fabricated audit rows (direct SQL, bypassing the kernel) | one `reconstruct_state_at` and one `list_derived_at` against a target transition | audit-log replay as a function of N (number of rows) and `--at <fraction>` (how far through the log the target sits) |

The `write` and `read` fixture distributes journal lines across `K` distinct accounts (default `K = 2`, configurable via `--accounts`). Entry `i` debits `account_{i mod K}` and credits `account_{(i+1) mod K}` for the same amount, so every entry is self-balancing and the `balanced_posted_entry` invariant holds. The trial-balance derived claim produces one row per distinct account that received at least one line; assertions on the read scenario check the loose bound `0 < rows <= K`.

Larger `K` stresses two things at once on the read path: `enumerate_derived`'s grouping (the BTreeSet that orders the K key tuples) and the per-account `Sum` lookups (one per account, each narrowed by the argument-position index on `JournalLine[1] = account`). The write path is largely independent of `K`; varying it mostly changes fixture characteristics.

The `as-of` fixture is different: it fabricates audit rows directly via SQL (one `INSERT ... SELECT ... FROM generate_series`), bypassing `propose_against_pg`. The bench is measuring replay cost, not write cost - going through the kernel for every fabricated transition would make fixture build the dominant cost and obscure the replay signal. Each fabricated row carries the same 3-claim payload (1 JournalEntry + 2 JournalLines on `account_cash` / `account_revenue`) and a strictly-monotone `committed_at` so replay order is deterministic.

## Running

```bash
DATABASE_URL=postgres:///morpholog_dev \
  cargo run -p morpholog-bench --release -- write 1000 --reset

DATABASE_URL=postgres:///morpholog_dev \
  cargo run -p morpholog-bench --release -- read 10000 --accounts 100 --reset

DATABASE_URL=postgres:///morpholog_dev \
  cargo run -p morpholog-bench --release -- as-of 10000 --at 1.0 --reset
```

`--release` matters; debug builds add an order of magnitude that obscures the algorithmic signal.

The bench **truncates the entire `morpholog` schema before each run**. The required `--reset` flag is the acknowledgement: without it the binary refuses to start, so the `DATABASE_URL` env-var fallback cannot silently destroy a database a shell already happens to point at. Do not point it at a database with anything you want to keep.

## Observations (2026-05-17, local PostgreSQL 17)

Run on a developer workstation against a local `morpholog_dev` database. Indicative, not benchmark-grade; reproduce locally for any decision that depends on the numbers.

### Read path phase split

The read scenario reports `list_scoped`, `build_state`, and `enumerate` separately. `list_scoped` fetches only claims for predicates the derived claim's body references (computed via `predicates_referenced_by_derived`); for `trial_balance_row` that means JournalLine only. At `N = 100 000` (300 000 total claims, 200 000 JournalLine):

| K | list_scoped | build_state | enumerate | derived rows |
|--:|--:|--:|--:|--:|
| 2 | ~1 100-1 400 ms | ~210 ms | ~460 ms | 2 |
| 100 | ~1 200 ms | ~210 ms | ~410 ms | 100 |

`list_scoped` fetches 200 000 rows instead of 300 000 (~33% fewer) since the scoped query is `WHERE predicate_name = ANY(['JournalLine'])`. The wall-clock saving is more modest than the row-count saving because JournalLine rows are bigger (4 args vs JournalEntry's 3), so the JSONB decode share per row is larger; skipping the smaller JournalEntry rows saves rows but proportionally less time. The ledger fixture is also nearly the worst case for this optimisation - only two distinct predicate types exist in the data, and the derived needs one of them. In a real workload with many unrelated predicates (e.g. claims from other examples co-existing in one database), the win compounds: the noise-claims correctness test in `morpholog-postgres/tests/integration.rs` verifies that 200 unrelated claims do not affect the answer; in production it would be the bench-visible time difference.

Observations:

- `list_scoped` still dominates (~65% of read time even after scoping). The next direction would be either a `--noise-claims K` axis on the bench to make the predicate-scoping win obvious at workload scales typical of multi-program databases, or a deeper PG-side optimisation (e.g. an index on `predicate_name`, or streaming the fetch).
- `build_state` is ~14% (~210 ms for 200 000 claims). Builds both indexes from scratch. Linear in claim count and constant per claim.
- `enumerate` is ~20%. Slightly *cheaper* at K=100 than at K=2, which is a positive signal about the argument-position index: the per-account `Sum` for each of 100 accounts touches roughly `2N/K` lines (so K=100 sums each touch ~6 000 lines instead of K=2 sums each touching ~300 000 lines). The argument-position index on `JournalLine[1] = account` is what makes this scale; without it, K=100 would be ~100x slower than K=2 for the enumerate phase.

### Write path

| N | K | fixture_build | propose_one |
|--:|--:|--:|--:|
| 1 000 | 2 | 30 ms | ~15 ms |
| 10 000 | 2 | ~300 ms | ~240 ms |
| 10 000 | 100 | ~520 ms | ~240 ms |
| 100 000 | 100 | ~5 200 ms | ~2 300 ms |

Linear in `N` and essentially independent of `K`. The remaining cost on `propose_one` is `load_state` + the indexed kernel work for `propose` + `INSERT` for claims/audit/outbox + COMMIT. Improvements here likely come from the same `load_state` work as the read path; the kernel itself is no longer the bottleneck at these sizes.

### As-of replay path

`reconstruct_state_at` and `list_derived_at` against fabricated audit logs. The interesting axis is `--at`: `1.0` walks the full log; `0.5` walks half.

| N | --at | fixture_build | reconstruct | list_derived_at |
|--:|--:|--:|--:|--:|
| 1 000 | 1.0 | ~20 ms | ~11 ms | ~10 ms |
| 10 000 | 1.0 | ~170 ms | **~140 ms** | ~190 ms |
| 100 000 | 1.0 | ~1 700 ms | **~1 500 ms** | ~2 500 ms |

**Replay now scales linearly.** Going from N=1 000 to N=10 000 (10x) costs ~13x in `reconstruct`; from N=10 000 to N=100 000 (10x) costs ~11x. This used to be ~140x per 10x N - the asserts-only quadratic that the original `Vec` + `iter().any` dedupe loop exhibited.

The fix was a `ReplaySet` (Vec + HashMap + live bits) inside `reconstruct_inner`: assertions and retractions are now amortised `O(1)`, with a single compaction step at the end that walks the recorded claims once. `ClaimInstance` derives `Hash` to make this possible (children already did, after PR #23). The structural shape mirrors the index work that fixed the original write-path quadratic in PR #23, applied to audit replay instead of state indexing.

Bench numbers before the fix: N=10 000 took ~4 600 ms on `reconstruct`; N=100 000 was untested because the projection (~8 minutes) was untenable.

### Still on the deferred list

- **Streaming `sqlx::query::fetch` instead of `fetch_all`.** Memory still scales with the number of audit rows fetched. Probably not pressing until someone runs the bench past N=1M; the per-row payload is small (~200-500 bytes), so even N=1M is on the order of a few hundred megabytes. Copilot #5 from PR #27 documented the concern.
- **Incremental snapshots.** If a workload pushes the linear-in-N cost to where 1.5 s at N=100K is unacceptable for an interactive query, the next move is to materialise `State` at periodic transition points and replay forward from the nearest snapshot. Not forced today.
- **Full materialisation.** Maintain a snapshot at the latest transition continuously, so current-state reads don't replay anything. Requires invalidation discipline; the predicate-scoped derived reads (PR #25) already cover the "current trial balance" use case without needing this.

A retraction-heavy audit log is also not benchmarked yet; the `ReplaySet` design's `retract` is `O(1)` so the structural fix would carry over, but it's worth confirming empirically once a worked example produces enough retractions to matter.

### History

The write path used to be **structurally quadratic**: an early version of this bench surfaced 31 seconds per propose at `N = 10 000`. The predicate-and-argument-position indexed `State` PR (#23) brought it down by ~200x. Then the as-of `reconstruct_inner` was found to have its own quadratic in PR #28's new scenario; the `ReplaySet` work that produced the numbers above fixed it. The PRs: `#22` introduced this bench; `#23` indexed `State`; `#24` added the `--accounts K` axis and the read-path phase split; `#25` wired the read path to load only the predicates a derived claim references; `#28` added the `as-of` scenario and surfaced the replay quadratic; the next PR (this one's predecessor in chronology) added the `ReplaySet` and turned as-of replay linear.
