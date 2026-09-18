# morpholog-bench

Synthetic scale-pressure benchmark for the Morpholog runtime. Most scenarios target the double-entry ledger example, the first programme with both non-trivial write-side invariants and a read-side derived claim; `wide` synthesises its own predicate because the gallery has nothing at the arity it measures. This README is the instrument manual and holds the current readings; the measurement DISCIPLINE - the frozen suite contract, bench-first rule, and what a performance PR must carry - lives in [`docs/benchmarking.md`](../../docs/benchmarking.md).

## Status

Exploratory. The numbers this binary prints are not regression assertions and are not checked into the repo as expected values. The intent is to surface bottlenecks ahead of speculative optimisation, per the project's design-history discipline.

A minimal-size compatibility smoke test (`cargo test -p morpholog-bench`, gated on `DATABASE_URL`) runs every scenario once at trivial size (plus the suite plumbing over a private tiny plan, and a pure renderer test) and asserts only that each completes - never a timing. It catches schema or API drift in the bench's hand-written SQL on the next PG-backed test run, rather than the next time someone runs the scale bench by hand. It is wired into CI and `scripts/precommit.sh` alongside the other PG-backed suites.

## What it measures

| Scenario | Setup | Hot path | What it stresses |
|---|---|---|---|
| `write` | N journal entries inserted via direct SQL across K accounts | one `propose_against_pg(post_simple_entry, ...)` call | scoped `load_state` + invariant evaluation over the candidate state + commit |
| `read`  | same fixture | inline `list_claims` + `State::from_claims` + `enumerate_derived`, each phase timed separately | each layer of the read path, so the dominant cost is visible directly |
| `as-of` | N fabricated audit rows (direct SQL, bypassing the kernel) | one `reconstruct_state_at` and one `list_derived_at` against a target transition | audit-log replay as a function of N, `--at <fraction>` (how far through the log the target sits), and `--retract-fraction K` |
| `contend` | optional `--prepopulate N`, then W workers post concurrently | `--workers` x `--ops-per-worker` concurrent `propose_against_pg` calls, spread across `--periods P`, optionally `--disjoint` | throughput and the SERIALIZABLE 40001 retry rate under concurrency; whether value-level vs predicate-level partitioning relieves it |
| `kernel` | an in-memory ledger of N entries; no database, no `--reset` | one `propose` with the ledger's invariants, the same with none, and `--acts` sequential proposals each against the candidate the act before it produced | the kernel alone: what invariant evaluation against the book costs (the difference between the two proposals), and the interpreted core of `transact` |
| `replay` | the `as-of` fixture | `coverage_replay` and `score_candidate` of the ledger against itself | the audit-analysis arc: what a replay that observes every audit row costs as the log grows |

`write` also reports where each proposal's time went - `phase_begin_tx`, `phase_load_state`, `phase_kernel`, `phase_finalise` - read from the production path through its timed facade, `propose_against_pg_timed`; the ordinary facade touches no clock. That split is what tells a rebuild from an invariant scan from a commit before anyone estimates.

The `write` and `read` fixture distributes journal lines across `K` distinct accounts (default `K = 2`, via `--accounts`). Entry `i` debits `account_{i mod K}` and credits `account_{(i+1) mod K}` for the same amount, so every entry is self-balancing and the `balanced_posted_entry` invariant holds. The trial-balance derived claim produces one row per distinct account that received a line; the read scenario checks the loose bound `0 < rows <= K`. Larger `K` stresses `enumerate_derived`'s grouping (the BTreeSet ordering the K key tuples) and the per-account `Sum` lookups (each narrowed by the argument-position index on `JournalLine[1] = account`). The write path is largely independent of `K`.

`--noise-claims K` pre-populates K rows of an `UnrelatedNoise` predicate that no transformation or invariant touches. A correct predicate-scoped `load_state` skips these server-side (`WHERE predicate_name = ANY($1)`); an unscoped loader would fetch and JSONB-decode every one. It is the bench-side equivalent of the noise-claims regression test in `morpholog-postgres`.

The `as-of` fixture fabricates audit rows directly via SQL (one `INSERT ... SELECT ... FROM generate_series`), bypassing `propose_against_pg`: the bench measures replay cost, not write cost, and going through the kernel per row would make fixture build dominate. Each row carries a 3-claim payload (1 JournalEntry + 2 JournalLines) and a strictly-monotone `committed_at` so replay order is deterministic. `--retract-fraction K` (0-50) makes every `stride`-th transition retract the immediately-prior entry's payload instead of asserting a fresh one; the stride is at least 2 so a retract always targets a still-live assert. The purely-additive default is best-case for replay, so this axis is what would expose any non-linearity in the `ReplaySet` retract path.

The `contend` fixture posts uniquely-keyed entries concurrently. In the default ledger workload, worker `w` posts into period `w mod P` (`--periods P`, default 1 = one shared period) but always shares the journal-line *predicate* footprint that `load_state` reads, so `--periods` partitions by value. With `--disjoint`, the workload becomes a synthetic one whose whole footprint is a single predicate `Bench_{w mod P}` (built via `ir_builder`, since the ledger's predicates are fixed), so `--periods` partitions by predicate and `--periods >= workers` makes the workers genuinely disjoint. Each proposal carries the retry loop a real embedder owns - the kernel and adapter never retry a 40001 - backing off and retrying up to `--max-retries` before an operation is recorded as failed. The pool is sized to the worker count so connections never serialise the workers ahead of SSI.

## Running

```bash
# Use a throwaway database - the bench TRUNCATES it. Never your dev DB.
createdb morpholog_bench
psql morpholog_bench -f crates/morpholog-core/sql/schema.sql
# the bench refuses a database behind the migration head; record the versions:
cargo run -p morpholog-cli --release -- migrate --database-url postgres:///morpholog_bench

DATABASE_URL=postgres:///morpholog_bench \
  cargo run -p morpholog-bench --release -- write 100000 --reset

# the same write under the compiled invariant route, unprovisioned and
# then with the indexes the compiler names (the bench establishes the
# index condition itself; see docs/benchmarking.md):
DATABASE_URL=postgres:///morpholog_bench \
  cargo run -p morpholog-bench --release -- write 100000 --reset --implementation compiled
DATABASE_URL=postgres:///morpholog_bench \
  cargo run -p morpholog-bench --release -- write 100000 --reset --implementation compiled-indexed

DATABASE_URL=postgres:///morpholog_bench \
  cargo run -p morpholog-bench --release -- read 100000 --accounts 100 --reset

DATABASE_URL=postgres:///morpholog_bench \
  cargo run -p morpholog-bench --release -- as-of 100000 --at 1.0 --retract-fraction 50 --reset

DATABASE_URL=postgres:///morpholog_bench \
  cargo run -p morpholog-bench --release -- contend --workers 16 --ops-per-worker 20 --prepopulate 2000 --periods 16 --reset

# predicate-disjoint workload (value-partition vs predicate-partition contrast):
DATABASE_URL=postgres:///morpholog_bench \
  cargo run -p morpholog-bench --release -- contend --workers 16 --ops-per-worker 20 --disjoint --periods 16 --reset

# cumulative core-import curve (small N - the journey is ~quadratic):
DATABASE_URL=postgres:///morpholog_bench \
  cargo run -p morpholog-bench --release -- import 1000 --repeat 3 --reset

# the argument-count axis (default arity 13):
DATABASE_URL=postgres:///morpholog_bench \
  cargo run -p morpholog-bench --release -- wide 10000 --arity 13 --repeat 5 --reset

# the frozen canonical matrix, one table (quick ~15s; full takes tens
# of minutes on the interpreted runtime):
DATABASE_URL=postgres:///morpholog_bench \
  cargo run -p morpholog-bench --release -- suite --ladder quick --repeat 5 --reset

# the kernel alone (no database, no --reset): a 370-act day over a 10 000-entry book
cargo run -p morpholog-bench --release -- kernel 10000 --acts 370 --repeat 5

# the two replays that walk the whole log:
DATABASE_URL=postgres:///morpholog_bench \
  cargo run -p morpholog-bench --release -- replay 100000 --retract-fraction 50 --repeat 3 --reset

# baseline and candidate side by side, from two `suite --format json` reports
# taken on the same host under the same contract:
cargo run -p morpholog-bench --release -- compare before.json after.json

# end-to-end CLI latency a subprocess embedder pays (not in-process):
DATABASE_URL=postgres:///morpholog_bench ./scripts/embedder_latency.sh 50
```

Every scenario takes `--repeat R`: repeats start from the same logical pre-state (mutating scenarios rebuild their fixture, which is excluded from the timed sample), the first sample reports as `first`, the median over the rest as `steady median`. Planner statistics are refreshed (`ANALYZE`) after every fixture build.

`--release` matters; debug builds add an order of magnitude that obscures the algorithmic signal.

The bench **truncates the entire `morpholog` schema before each run**. The required `--reset` flag is the acknowledgement: without it the binary refuses to start, so the `DATABASE_URL` env-var fallback cannot silently destroy a database a shell already happens to point at.

## Observations

### Three-way verdict on the compiled route (2026-09-18, `suite --repeat 5` requested, PostgreSQL 18.6, one binary)

The first run of the `--implementation` axis: the interpreter, the compiled invariant route with no compiler-required index, and the compiled route with its indexes provisioned, under the unchanged contract-2 ruler. Shapes, not numbers, are the claim; the numbers are here so the shapes can be checked. Two builds of one branch produced them: the full ladder from the binary at c3d7aab, the quick ladder from the binary at cf1506c; the commits between changed only how the bench establishes the index condition and where its fixture timer starts, never the measured execution path, and the quick ladder reproduced the earlier quick run within noise.

The quick ladder, all three configurations, steady median with `--repeat 5` requested (contend and import carry their canonical cap of three samples; every other row five), one proposal (ms):

| case, point | interpreted | compiled, no index | compiled, indexed |
|---|--:|--:|--:|
| write/base 100 | 3.47 | 11.66 | 4.18 |
| write/base 1,000 | 14.92 | 592.88 | 12.60 |
| write/noise 1,000 | 16.72 | 836.11 | 15.17 |
| contend/shared, 4 workers, commits/s | 281.18 | 76.56 | 371.68 |
| contend/shared, 4 workers, retries/commit | 1.96 | 2.19 | 1.73 |
| import/core 500, journey | 1,880 | 27,773 | 21,739 |
| write/base 1,000, fixture build | 28.88 | 29.30 | 38.15 |

The full ladder, interpreter against the indexed compiled route, steady median with `--repeat 5` requested (contend and import three samples; every other row five):

| case, point | interpreted | compiled, indexed | ratio |
|---|--:|--:|--:|
| write/base 1,000, propose (ms) | 15.75 | 13.35 | 0.85 |
| write/base 10,000 | 134.81 | 126.59 | 0.94 |
| write/base 100,000 | 1,460 | 1,407 | 0.96 |
| write/noise 100,000 | 1,291 | 1,270 | 0.98 |
| wide/size 10,000 | 3,688 | 2,519 | 0.68 |
| wide/size 100,000 | 765,488 | 319,378 | 0.42 |
| contend/shared 16 workers, commits/s | 18.97 | 28.24 | 1.49 |
| contend/shared 16 workers, retries/commit | 9.70 | 11.35 | 1.17 |
| contend/disjoint 16 workers, commits/s | 2,000 | 1,773 | 0.89 |
| import/core 1,000, journey (ms) | 6,639 | 59,347 | 8.94 |
| import/core 3,000, journey (ms) | 51,270 | 63,264 | 1.23 |
| read families, all points | flat | flat | 0.96 to 1.07 |

- **Compilation without indexes is strongly superlinear over the measured range and far behind the interpreter** (74x slower at 1,000 entries for a 10x size step). The unindexed configuration is measured at the quick ladder only; one proposal at 100k would take hours. Indexes are necessary but not sufficient: unindexed compilation is far worse than the interpreter, indexed stage 1 restores the ledger path to roughly the interpreter's cost, and stage 2 is what produced the spike's flat scaling.
- **Stage 1 with indexes tracks the interpreter, both linear, same slope.** The spike's flat ~2 ms curve at 100k and its ten-fold retry reduction were stage-2 effects (the case-bound check); production runs stage 1, which proves every invariant over the whole relation on every proposal. Under shared contention throughput rises by half because each proposal is cheaper; retries per commit do not fall.
- **Import gains nothing from indexes on a table growing from empty**, and loses at 1,000: the statistics are the empty table's until something analyses it (verified: the same violation query scans by primary key before one `ANALYZE` and seeks the provisioned indexes after). At 3,000 autovacuum has caught up and the journey is within a quarter. The ruler stays as it is; this is what a fresh deployment sees.
- **Controls:** the read, replay and kernel families are flat across configurations; the indexed configuration's one collateral cost is fixture building, up a fifth to a third at 1,000 entries from index maintenance on bulk inserts, measured with the index condition established outside the timer.


Indicative, not benchmark-grade; reproduce locally for any decision that depends on the numbers.

### Contract-1 baseline (2026-08-22, `suite --ladder full --repeat 5`, PostgreSQL 18.6)

The first full-ladder run under the frozen matrix (`suite_contract=1`), same machine class as the historical tables below. Numbers are never checked in as expected values; the shapes are what matters, and the full 206-row table regenerates with the command above.

- **The laws below all reproduced under the new instrument.** Write/base steady-median propose ~22ms/121ms/1.5s at 1k/10k/100k (linear, in-scope-state-bound); noise stays invisible to the scoped path; as-of replay linear with the 50%-retract log slightly cheaper; the contention A/Bs held exactly: at 16 workers, ledger shared vs value-partitioned retry rates ~11.5 vs ~11.2 (value sharding: no relief), synthetic predicate-shared vs disjoint throughput ~558 vs ~705 commits/s (footprint partitioning: relief).
- **`import/core` plots the cumulative curve for the first time**: last-decile commits ~3.0ms/21ms/76ms at N=100/1k/3k against a ~2ms first decile - per-commit cost growing linearly with the book, so the 0->N journey is quadratic (1k imports in ~11.4s, 3k in ~1.9min). This is the measured shape behind the batch-throughput roadmap lever.
- **New finding - `wide/size` is the suite's steepest curve**: ~58ms/2.6s/~7min at 1k/10k/100k rows, while `wide/arity` at fixed 10k rows is nearly flat (~2.8-3.1s from arity 4 to 13). The precise cause matters: the invariant's head matches every `WideLine` witness, and the grouped sum is re-evaluated once PER witness - a **correlated aggregate**, whose interpreted work is the sum of squared group sizes. The fixture spreads rows evenly over 16 groups, so that is N^2/16: quadratic in row count whenever the group count stays fixed as rows grow. The honest reading is "a correlated grouped aggregate recomputed per head witness is roughly quadratic", not "aggregates over large predicates are inherently super-linear" - and argument width itself is nearly flat by comparison. The 100k cell dominates the full run's wall clock (~35 of its ~75 minutes); recorded rather than trimmed, because it is the sharpest measured pressure yet toward compiled invariant checking, and it says exactly what to compile: collapse the per-witness relational scans into one grouped SQL aggregate.

The detailed tables below were taken on 2026-05-29 against PostgreSQL 17. A 2026-06-27 re-run on the PostgreSQL 18 floor confirmed every shape and law here is unchanged. The contention numbers - SSI-bound, not hardware-bound - reproduced within noise (16-worker shared-predicate retry rate ~9.9 / ~21 commits/s; predicate-disjoint ~2.3 / ~2760 commits/s); the read and write absolute timings moved in *both* directions with the machine (write faster, `list_scoped` slower), which is machine and cache variance, not a version effect - so they are annotated here, not restated as a PG17-to-18 delta. PG18's headline performance features do not touch this hot path: `list_scoped` is a full-prefix index scan on the `(predicate_name, arguments)` primary key plus JSONB decode (CPU-bound), so B-tree skip scan (which needs an *omitted* prefix column) does not apply, and async I/O (which accelerates sequential and bitmap-heap scans and vacuum) lands elsewhere; the contention floor is SSI predicate-lock granularity. The deferred levers below - incremental snapshots, streaming fetch, materialised derived claims - remain the real ones; PG18 does not change which they are.

### Read path phase split (N=100 000, K=2)

| phase | time | share |
|---|--:|--:|
| `list_scoped` (fetch + JSONB-decode 200 000 `JournalLine` rows) | ~843 ms | ~63% |
| `build_state` (build both indexes) | ~153 ms | ~11% |
| `enumerate` (trial balance) | ~352 ms | ~26% |

`list_scoped` dominates and is fetch + decode of the full in-scope set, not index lookup - the primary key `(predicate_name, arguments)` already serves the predicate-scoped read as an index-only scan, so a dedicated `predicate_name` index would not help. The only real levers here are materialised derived claims or streaming the fetch, both deferred. Adding 200 000 unreferenced noise claims leaves the read path unchanged (the `ANY([...])` filter skips them server-side).

### Write path (N=100 000 ledger entries, K=2)

`propose_one` is ~1.6 s, dominated by `load_state` + the per-row claim/audit/outbox writes + COMMIT. With no noise every claim is in scope, so scoping has nothing to skip; with 200 000 unreferenced noise claims the scoped path stays ~1.6 s while an artificially-unscoped loader grows ~54%. The kernel itself is not the bottleneck at these sizes - the cost is loading and re-indexing the full in-scope claim set every transition, which is intrinsic until incremental snapshots land.

### As-of replay (`--at 1.0`, full replay)

| N | reconstruct (asserts only) | reconstruct (50% retract) |
|--:|--:|--:|
| 10 000 | ~157 ms | ~136 ms |
| 100 000 | ~1 649 ms | ~1 384 ms |

**Replay is linear in N, and stays linear under a retraction-heavy log.** 10x N costs ~10x reconstruct in both regimes; the 50%-retract log is no worse (slightly cheaper, because its final live set is smaller, so `list_derived_at` enumerates fewer claims). The `ReplaySet`'s amortised-`O(1)` retract holds empirically. This is the curve the roadmap wanted before sizing any snapshot/lattice work: even maximal churn replays linearly, so that work remains unforced.

### Contention (`contend`, 20 ops/worker, 2 000 prepopulated)

| workers | committed | retries (40001) | retry_rate | throughput |
|--:|--:|--:|--:|--:|
| 1 | 20 | 0 | 0.00 | ~47 commits/s |
| 2 | 40 | 22 | 0.55 | ~43 commits/s |
| 4 | 80 | 171 | 2.14 | ~32 commits/s |
| 8 | 160 | 792 | 4.95 | ~26 commits/s |
| 16 | 320 | 3 038 | 9.49 | ~18 commits/s |

Concurrent posts into one ledger **anti-scale**: the retry rate climbs roughly linearly with worker count and total throughput *falls* (no operation was lost - `failed` stayed 0 throughout, up to `--max-retries`). Every post reads the journal-line *predicate* footprint that every other post writes, so SSI serialises them and the retry overhead makes more workers worse, not better.

The obvious next question - does sharding the writes across periods relieve it? - has a measured answer, and it is no. Holding 16 workers fixed and raising `--periods` from 1 (one shared period) to 16 (a private period per worker):

| periods | retry_rate | throughput |
|--:|--:|--:|
| 1 | ~9.99 | ~17 commits/s |
| 2 | ~10.0 | ~17 commits/s |
| 4 | ~10.3 | ~16 commits/s |
| 16 | ~10.0 | ~17 commits/s |

Flat. **Value-level partitioning inside one shared predicate does not reduce contention** - period here, and by the same logic account or book when those are values in the same predicate - because `load_state` reads the whole predicate (`WHERE predicate_name = ANY(...)`), not a value sub-range, so every post's read set still overlaps every other post's writes on `JournalLine`.

The positive half: **predicate-disjoint partitioning does.** `--disjoint` switches to a synthetic workload whose entire footprint is one predicate, `Bench_{w mod periods}`, so `--periods` now partitions by *predicate*. Same 16 workers:

| periods (distinct predicates) | retry_rate | throughput |
|--:|--:|--:|
| 1 (all share `Bench_0`) | ~5.95 | ~680 commits/s |
| 2 | ~3.45 | ~1 400 commits/s |
| 4 | ~2.83 | ~2 000 commits/s |
| 8 | ~2.38 | ~2 590 commits/s |
| 16 (disjoint) | ~2.45 | ~2 720 commits/s |

(The synthetic workload is far lighter than the ledger, so compare *within* this table, not against the one above.) Spreading the workers across distinct predicates roughly quartered the wall-clock and more than halved the retry rate - the opposite of the value-partition sweep, which moved nothing. That is the law:

> The unit of concurrency is the read footprint the runtime loads, not the business value. Value-disjoint writes inside one predicate do not scale; predicate-disjoint footprints do.

The residual ~2.4 retries/commit at full disjointness is *consistent with* PostgreSQL SSI predicate-lock granularity rather than logical Morpholog overlap: the short, adjacent `Bench_*` keys likely share index pages, so logically-disjoint predicates still false-share. Confirming the mechanism - and driving the residual toward zero - would need a follow-up physical-layout control (spread predicate names, larger seeded key ranges, or partitioned storage), which is also the reason a partitioned substrate would matter at scale. This pair of sweeps also supplies the measured 40001 rate the roadmap requires before any substrate change (e.g. TimescaleDB) can be reasoned about.

### Embedder / CLI latency (`scripts/embedder_latency.sh`)

Everything above runs in-process. A non-Rust embedder (the reference ETRM) drives Morpholog as `morpholog propose ...`, paying process spawn + parse + validate + a fresh connection + propose + commit + JSON on every call - none of which the in-process bench sees. The harness times the CLI end-to-end (N=50, starting from an empty ledger - state grows over the run - local PostgreSQL):

| invocation | per-call |
|---|--:|
| `morpholog check` (spawn + parse + validate) | ~3 ms |
| `morpholog propose` (+ connect + propose + commit + encode) | ~9 ms |

So the fixed per-call tax a subprocess embedder pays is **single-digit milliseconds** - ~3 ms of spawn/parse/validate plus ~6 ms of connect/propose/commit against a small ledger. That sustains roughly 100 governed transitions/second single-threaded, which is comfortable for lifecycle events (capture, confirm, settle, approve) and admin/audit flows. The propose component grows with in-scope state exactly as the `write` scenario measures; the spawn/parse/validate floor does not. A long-lived worker (load + validate once, keep a pool warm, speak HTTP/socket) is the answer only if a high-frequency path needs it - and per the doctrine those paths should not run through the governed core anyway.

### Deferred

- **Incremental snapshots** for as-of replay. Replay is linear in N and now confirmed linear under retraction too, so the materialise-every-K-transitions checkpoint is not forced; revisit if linear-in-N becomes unacceptable for an interactive query.
- **Streaming `sqlx::query::fetch`** instead of `fetch_all`. Memory scales with audit rows fetched; not pressing until past N=1M (per-row payload is ~200-500 bytes).
- **Materialised derived claims**, with invalidation modelled as ordinary claims (not cache machinery). The current `list_scoped`-dominated read cost is the pressure; awaits a worked example.

### History

The write path was once structurally quadratic (~31 s per propose at N=10 000); the predicate-and-argument-position indexed `State` brought it down ~200x. The as-of `reconstruct_inner` had its own asserts-only quadratic, fixed by the `ReplaySet` (Vec + HashMap + live bits) that turned replay linear. The PRs: the bench's introduction; indexed `State`; the `--accounts K` axis + read-path phase split; predicate-scoped read loading; the `as-of` scenario and its replay quadratic; the `ReplaySet` fix; the scenario set that added the retraction axis, concurrency, and the smoke test (plus the `as-of` fabricator's overdue `actor` column); the `--periods` partition axis that measured value-level partitioning as no help against SSI contention; and the `--disjoint` predicate-partition mode plus the `scripts/embedder_latency.sh` CLI harness, which proved the positive half of the law (predicate-disjoint footprints scale) and put a single-digit-millisecond number on the subprocess embedder's per-call tax. The suite-discipline PR then turned the instrument itself benchmark-grade ahead of the compiled-checking arc: `--repeat` with first/steady-median reporting, `ANALYZE` after fixtures, sub-millisecond resolution, the frozen `suite` case matrix with per-case provenance and a machine-readable JSON mode, and the consumer-derived `import` (cumulative core-import curve) and `wide` (argument-count axis) scenarios.
