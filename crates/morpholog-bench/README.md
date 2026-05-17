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
| 1 000 | 1.0 | ~20 ms | ~30 ms | ~25 ms |
| 10 000 | 1.0 | ~170 ms | **~4 600 ms** | ~2 600 ms |
| 10 000 | 0.5 | ~170 ms | ~690 ms | ~520 ms |

**This asserts-only replay has a quadratic membership-check pathology.** Going from `N=1 000` to `N=10 000` (10x) makes `reconstruct` ~140x slower; halving the replay depth at `N=10 000` (`--at 0.5`) makes it ~6.8x faster - both consistent with O(N^2). The cause is the per-row dedupe inside `reconstruct_inner`: `claims.iter().any(|c| c == a)` over a growing `Vec<ClaimInstance>` for every asserted claim, summing to O(N^2) over the full replay. Same family of pathology as the original write-path quadratic that PR #22 surfaced and PR #23 fixed.

The fixture is asserts-only, so this finding is precise about that case. A retraction-heavy audit log would also stress `claims.retain(|c| c != r)`, which is independently O(|claims|) per retraction; that variant has not been benchmarked yet but the same structural fix would help.

This bench scenario is the evidence that forces the next optimisation. Candidates, in roughly increasing complexity:

- **Replay working set with O(1) membership.** Replace the `Vec<ClaimInstance>` with a set-backed structure - either `Vec<ClaimInstance>` + `HashSet<ClaimInstance>` for membership, or a tombstoned `Vec + HashMap<ClaimInstance, usize> + live: Vec<bool>` that also gives O(1) retraction. Requires `Hash` on `ClaimInstance` (the children already derive it after PR #23). The cheapest fix; mirrors PR #23's `State` index work applied to the replay loop.
- Streaming `sqlx::query::fetch` instead of `fetch_all`, to bound peak memory (Copilot #5 from PR #27).
- Incremental snapshots: periodically materialise `State` at well-known transition points so replay only walks the tail.
- Full materialisation: maintain a snapshot at the latest transition continuously.

The replay-working-set fix is the smallest and least architecturally invasive; the others are progressively bigger commitments. The bench will measure each.

### History

The write path used to be **structurally quadratic**: an early version of this bench surfaced 31 seconds per propose at `N = 10 000`. The predicate-and-argument-position indexed `State` PR that followed brought it down by ~200x. The PRs that produced the current state: `#22` introduced this bench and recorded the original quadratic; `#23` indexed `State` and recorded the fix; `#24` added the `--accounts K` axis and the read-path phase split; `#25` wired the read path to load only the predicates the derived claim references; the next PR added the `as-of` scenario and surfaced the replay-loop quadratic.
