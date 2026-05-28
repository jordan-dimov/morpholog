# morpholog-bench

Synthetic scale-pressure benchmark for the Morpholog runtime. Two scenarios that share the same fixture builder; both target the double-entry ledger example because it is the first program that has both non-trivial write-side invariants and a read-side derived claim.

## Status

Exploratory. The numbers this binary prints are not regression assertions and are not checked into the repo as expected values. The intent is to surface bottlenecks ahead of speculative optimisation, per the project's design-history discipline.

## What it measures

| Scenario | Setup | Hot path | What it stresses |
|---|---|---|---|
| `write` | N synthetic journal entries inserted via direct SQL across K accounts | one `propose_against_pg(post_simple_entry, ...)` call | scoped `load_state` + invariant evaluation over the candidate state + commit |
| `read`  | same fixture | inline `list_claims` + `State::from_claims` + `enumerate_derived` with each phase timed separately | each layer of the read path, so the dominant cost is visible directly |
| `as-of` | N fabricated audit rows (direct SQL, bypassing the kernel) | one `reconstruct_state_at` and one `list_derived_at` against a target transition | audit-log replay as a function of N (number of rows) and `--at <fraction>` (how far through the log the target sits) |

The `write` and `read` fixture distributes journal lines across `K` distinct accounts (default `K = 2`, configurable via `--accounts`). Entry `i` debits `account_{i mod K}` and credits `account_{(i+1) mod K}` for the same amount, so every entry is self-balancing and the `balanced_posted_entry` invariant holds. The trial-balance derived claim produces one row per distinct account that received at least one line; assertions on the read scenario check the loose bound `0 < rows <= K`.

The `--noise-claims K` flag pre-populates K rows of an `UnrelatedNoise` predicate that no transformation or invariant in the double-entry-ledger programme touches. A correct predicate-scoped `load_state` skips these entirely (server-side, via `WHERE predicate_name = ANY($1)`); an older unscoped loader would fetch and JSONB-decode every one of them. The flag is the bench-side equivalent of the noise-claims regression test in `morpholog-postgres/tests/integration.rs`.

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

## Observations (2026-05-21, local PostgreSQL 17)

Run on a developer workstation against a local `morpholog_bench` database. Indicative, not benchmark-grade; reproduce locally for any decision that depends on the numbers.

### Read path phase split

The read scenario reports `list_scoped`, `build_state`, and `enumerate` separately. `list_scoped` fetches only claims for predicates the derived claim's body references (computed via `predicates_referenced_by_derived`); for `trial_balance_row` that means JournalLine only.

At `N = 100 000`, K=2 (300 000 ledger claims, 200 000 JournalLine):

| noise_claims | list_scoped | build_state | enumerate | derived rows |
|--:|--:|--:|--:|--:|
| 0 | ~909 ms | ~159 ms | ~384 ms | 2 |
| 200 000 | ~1 029 ms | ~160 ms | ~372 ms | 2 |

The read path has been predicate-scoped since PR #25, so adding 200 000 noise claims of an unreferenced predicate has essentially no effect on the read path; the small variation is within run-to-run noise. The `WHERE predicate_name = ANY(['JournalLine'])` query short-circuits past every noise row server-side.

Observations:

- `list_scoped` still dominates (~63% of read time). The next direction would be a deeper PG-side optimisation (e.g. an index on `predicate_name`, or streaming the fetch).
- `build_state` is ~11% (~160 ms for 200 000 claims). Builds both indexes from scratch. Linear in claim count and constant per claim.
- `enumerate` is ~26%. At K=100 the per-account `Sum` index work keeps enumerate cost flat (argument-position index on `JournalLine[1] = account` makes it `O(2N/K)` per account); historical observations on the previous fixture confirmed K=100 was actually *cheaper* than K=2 for enumerate at this N.

### Write path - predicate-scoped loading

The write path is also now predicate-scoped (this PR). `post_simple_entry`'s body reads `PeriodClosed` via its `require not PeriodClosed(period)` gate; the invariants `balanced_posted_entry`, `journal_entry_has_lines`, and `at_most_one_direct_successor` reference `JournalEntry`, `JournalLine`, and `Supersedes`. Anything else in `morpholog.claims` is skipped at the SQL layer.

The bench's `--noise-claims K` flag pre-populates K claims of an `UnrelatedNoise` predicate that no part of the double-entry-ledger programme touches. Comparing scoped (this PR) against an artificially-reverted unscoped `load_state` shows the win:

`N = 100 000` ledger entries (300 000 ledger claims), `accounts K = 2`:

| load_state | noise_claims | fixture_build | propose_one |
|---|--:|--:|--:|
| scoped (this PR) | 0 | ~3 939 ms | ~1 744 ms |
| scoped (this PR) | 200 000 | ~5 919 ms | ~1 623 ms |
| unscoped (pre-PR) | 0 | ~3 824 ms | ~1 667 ms |
| unscoped (pre-PR) | 200 000 | ~6 185 ms | **~2 562 ms** |

- With **no noise**, scoped and unscoped are roughly equal on `propose_one` (~1.7 s). Every claim in the database is in scope, so there's nothing for scoping to skip.
- With **200 000 noise claims** (40% of total rows in the table), unscoped `propose_one` grows by ~54% (1 667 -> 2 562 ms): the extra cost is fetching, JSONB-decoding, and indexing rows the kernel will never look at. Scoped `propose_one` is unchanged (~1.6 s): the SQL filter skips noise rows server-side.

The fixture_build delta is just the extra `INSERT ... SELECT generate_series` for the noise rows; it's the same on both code paths and not part of the optimisation's measurement.

The win compounds with predicate diversity: in a multi-program database where dozens of unrelated predicates co-exist, the noise/signal ratio is much higher than 40%, and scoped `load_state` skips proportionally more.

The remaining cost on `propose_one` is the SQL `INSERT` for claims/audit/outbox + COMMIT plus the (now bounded) `load_state` plus the indexed kernel work for `propose`. The kernel itself is no longer the bottleneck at these sizes.

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
