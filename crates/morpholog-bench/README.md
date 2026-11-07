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
| `as-of` | N fabricated audit rows (direct SQL, bypassing the kernel) | one `reconstruct_state_at` and one `list_derived_at` against a target transition | audit-log replay as a function of N, `--at <fraction>` (how far through the log the target sits), and `--retract-fraction K` or `--revise K` |
| `contend` | optional `--prepopulate N` (ledger entries, or with `--disjoint` items per synthetic predicate), then W workers post concurrently | `--workers` x `--ops-per-worker` concurrent `propose_against_pg` calls, spread across `--periods P`, optionally `--disjoint` | throughput and the SERIALIZABLE 40001 retry rate under concurrency; whether value-level vs predicate-level partitioning relieves it |
| `kernel` | an in-memory ledger of N entries; no database, no `--reset` | one `propose` with the ledger's invariants, the same with none, and `--acts` sequential proposals each against the candidate the act before it produced | the kernel alone: what invariant evaluation against the book costs (the difference between the two proposals), and the interpreted core of `transact` |
| `replay` | the `as-of` fixture | `coverage_replay` and `score_candidate` of the ledger against itself | the audit-analysis arc: what a replay that observes every audit row costs as the log grows |
| `rehearse` | N fabricated audit rows of a register in the TEC shape (direct SQL, replayable row for row: every row decides `same`), `--revise K` keeping the live set at 2K claims, `--noise M` admitting M claims no rule reads before them, `--phases` timing the decision step by step | `list_audit_rows`, then `rehearse_recorded` over the rows in memory, then the report's serialisation, then the end-to-end `rehearse_candidate`, each timed apart, and inside the read and the end-to-end call their page queries, decoding, decisions and advances | what a rehearsal costs per row as the history grows, split between the read, the kernel's decisions and the report; `--refusing` rehearses a candidate that refuses most rows, so the diagnosis is on the clock too |

`write` also reports where each proposal's time went - `phase_begin_tx`, `phase_load_state`, `phase_kernel`, `phase_finalise` - read from the production path through its timed facade, `propose_against_pg_timed`; the ordinary facade touches no clock. That split is what tells a rebuild from an invariant scan from a commit before anyone estimates.

The `write` and `read` fixture distributes journal lines across `K` distinct accounts (default `K = 2`, via `--accounts`). Entry `i` debits `account_{i mod K}` and credits `account_{(i+1) mod K}` for the same amount, so every entry is self-balancing and the `balanced_posted_entry` invariant holds. The trial-balance derived claim produces one row per distinct account that received a line; the read scenario checks the loose bound `0 < rows <= K`. Larger `K` stresses `enumerate_derived`'s grouping (the BTreeSet ordering the K key tuples) and the per-account `Sum` lookups (each narrowed by the argument-position index on `JournalLine[1] = account`). The write path is largely independent of `K`.

`--noise-claims K` pre-populates K rows of an `UnrelatedNoise` predicate that no transformation or invariant touches. A correct predicate-scoped `load_state` skips these server-side (`WHERE predicate_name = ANY($1)`); an unscoped loader would fetch and JSONB-decode every one. It is the bench-side equivalent of the noise-claims regression test in `morpholog-postgres`.

The `as-of` fixture fabricates audit rows directly via SQL (one `INSERT ... SELECT ... FROM generate_series`), bypassing `propose_against_pg`: the bench measures replay cost, not write cost, and going through the kernel per row would make fixture build dominate. Each row carries a 3-claim payload (1 JournalEntry + 2 JournalLines) and a strictly-monotone `committed_at` so replay order is deterministic. `--retract-fraction K` (0-50) makes every `stride`-th transition retract the immediately-prior entry's payload instead of asserting a fresh one; the stride is at least 2 so a retract always targets a still-live assert. The purely-additive default is best-case for replay, so this axis is what would expose any non-linearity in the `ReplaySet` retract path. `--revise K` builds a different shape: the first K transitions admit K entries and every later one replaces one entry's current version (its date and line amounts change), so the live set stays at 3K claims however long the history grows. That is a register revised in vintages, NESO's TEC archive being the case that forced it: the answer stays small while the history before it does not.

The `contend` fixture posts uniquely-keyed entries concurrently. In the default ledger workload, worker `w` posts into period `w mod P` (`--periods P`, default 1 = one shared period) but always shares the journal-line *predicate* footprint that `load_state` reads, so `--periods` partitions by value. With `--disjoint`, the workload becomes a synthetic one whose whole footprint is a single predicate `Bench_{w mod P}` (built via `ir_builder`, since the ledger's predicates are fixed), so `--periods` partitions by predicate and `--periods >= workers` makes the workers genuinely disjoint. Each proposal carries the retry loop a real embedder owns - the kernel and adapter never retry a 40001 - backing off and retrying up to `--max-retries` before an operation is recorded as failed. The pool is sized to the worker count so connections never serialise the workers ahead of SSI.

## Running

```bash
# Use a throwaway database - the bench TRUNCATES it. Never your dev DB.
createdb morpholog_bench
psql morpholog_bench -v ON_ERROR_STOP=1 -1 -f crates/morpholog-core/sql/schema.sql
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
  cargo run -p morpholog-bench --release -- as-of 73038 --revise 730 --at 0.1 --reset

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

# baseline and candidate side by side, from `suite --format json` runs taken
# on the same host under the same contract, at least four a side, interleaved
# in balanced order (a1 b1 b2 a2 a3 b3 b4 a4); each row says whether the runs
# separate (see docs/benchmarking.md):
cargo run -p morpholog-bench --release -- compare --before a1.json a2.json a3.json a4.json \
  --after b1.json b2.json b3.json b4.json

# the whole comparison in one command: baseline rev against the working tree,
# built separately, four runs a side interleaved, provenance recorded, then
# compare (--max-mhz caps the clock for the session; it needs sudo):
DATABASE_URL=postgres:///morpholog_bench ./scripts/bench_ab.sh main

# end-to-end CLI latency a subprocess embedder pays (not in-process):
DATABASE_URL=postgres:///morpholog_bench ./scripts/embedder_latency.sh 50
```

Every scenario takes `--repeat R`: repeats start from the same logical pre-state (mutating scenarios rebuild their fixture, which is excluded from the timed sample), the first sample reports as `first`, the median over the rest as `steady median`. Planner statistics are refreshed (`ANALYZE`) after every fixture build.

`--release` matters; debug builds add an order of magnitude that obscures the algorithmic signal.

The bench **truncates the entire `morpholog` schema before each run**. The required `--reset` flag is the acknowledgement: without it the binary refuses to start, so the `DATABASE_URL` env-var fallback cannot silently destroy a database a shell already happens to point at.

## Observations

### The interleaved runner, and what a clock cap buys (2026-09-22, `scripts/bench_ab.sh HEAD`, `suite --ladder quick --repeat 5`, PostgreSQL 18.6)

Three A/A sessions of the runner on one laptop (Core Ultra 7 155H), each building the same commit twice into separate target directories. The two builds came out byte-identical (same sha256), so the comparison really was one binary against itself.

- **Every run throttled.** With the counters read per run, the first session showed every run throttled at a package temperature of 45 C. That was not a sensor fault: under a 4-core load this machine draws about 40 W and sits at 100 C within a second, the cores 10-25% below their maximum clock. It turbos to its temperature limit by design. So the runner compares how much each side throttled rather than treating any throttling as contamination.
- **Uncapped, performance profile, on mains power:** 17.3% of the baseline's run time throttled, 20.5% of the candidate's. 155 of 156 rows `within noise`. The session took 3 minutes with warm build caches.
- **Capped at 3000 MHz (`--max-mhz 3000`):** 0.0% throttled on both sides, 153 of 156 rows `within noise`, everything 1.4 to 1.5 times slower.

Spread between the eight runs of each millisecond row, (max - min) / median:

| session | median row | 90th percentile row | worst row |
|---|--:|--:|--:|
| uncapped | 12.3% | 48.4% | 229% |
| capped at 3000 MHz | 11.9% | 24.4% | 91% |

Capping leaves typical run-to-run noise where it was - that noise is not thermal - and halves the tail, where the misleading outliers live. It stays opt-in: in the uncapped A/A session interleaving kept the two sides' comparison under control, and the default is faster. Reach for the cap when a claim rests on a row with a noisy tail.

A fourth uncapped session on a warmer machine (27% of run time throttled against 19%) came out `clean` with six rows called changed, all in the candidate's favour, while the candidate side had throttled 2.8 points less - inside the 5-point threshold. A clean session can still carry a handful of same-direction calls, which is why a lone call is something to look at and the claim comes from curves and controls. The package temperature also swung by more than 15 C within a second at idle, so runs started at up to 73 C despite the gate's 60 C: on this sensor the gate avoids starting in a hot spell, it cannot hold the start temperature steady. Absolute numbers also moved with the power state alone: the same quick-suite run took about 22 seconds on battery in the balanced profile and about 10.5 seconds on mains in performance, which is why the runner records both for every run.

### A/A: the run is the unit of evidence (2026-09-22, `suite --ladder quick --repeat 5`, PostgreSQL 18.6)

The same binary (sha256 `e18f9e3c...`, commit `2d0473d`), run twice 23 seconds apart on a quiet machine (load under 1.0, no throttling events during the run), then compared with a separation test over each run's repeats. Of the 76 rows with enough repeats to judge, 8 were called changed, in both directions and across unrelated families - roughly three times what chance allows. `write/base` at 1,000 read 43.0 ms and then 30.4 ms, a ratio of 0.71 that would have read as a 29% win. Repeats inside one run agree with each other far more than two runs do, so `compare` judges runs, never repeats.

Judged over runs instead: eight runs of one binary (sha256 `8afcdfee...`), interleaved as two labels x1 y1 x2 y2 x3 y3 x4 y4, no throttling events, compared four against four. One row of 156 was called changed - `kernel/acts` at 1,000, 172.8 against 176.7 ms, a ratio of 1.02 - and the other 155 read `within noise`, every family judged, the capped-repeat ones included.

### Case-local admission, paired against the whole-state check (2026-09-18, `suite --repeat 5` requested, PostgreSQL 18.6)

The same ruler again after case-local admission landed, with the whole-state binary (6174bef) and the case-local binary (1746237) run back to back in one session. The machine was thermally throttled that day, so every absolute number is roughly twice the earlier run's, the read families included; the within-session ratios are the claim and the earlier table is historical context, not a baseline.

The quick ladder, both binaries run back to back in one session; the decomposition reads left to right: what case-local semantics bought inside the kernel, what pushing the same bounded obligation into PostgreSQL added, and the total production effect.

| case, point | whole-state interpreted | case-local interpreted | case-local compiled, indexed | whole-state compiled, indexed | production ratio |
|---|--:|--:|--:|--:|--:|
| write/base 1,000, one proposal (ms) | 32.67 | 30.25 | 7.46 | 28.60 | 0.26 |
| write/noise 1,000, one proposal (ms) | 33.69 | 31.25 | 8.37 | 26.70 | 0.31 |
| wide/size 1,000, one proposal (ms) | 92.76 | 32.17 | 27.53 | 73.16 | 0.38 |
| contend/shared, 4 writers, retries/commit | 2.29 | 1.66 | 0.59 | 1.77 | 0.33 |
| contend/shared, 4 writers, commits/s | 138.09 | 154.24 | 459.40 | 201.75 | 2.28 |
| import/core 500, journey (ms) | 4,835 | 4,134 | 2,200 | 28,555 | 0.08 |
| read/base 1,000, scoped read (ms) | 26.33 | 26.02 | 26.84 | 27.15 | 0.99 |
| replay/retract 1,000, coverage replay (ms) | 16.24 | 15.79 | 10.20 | 15.66 | 0.65 |
| asof/retract 1,000, reconstruct (ms) | 13.28 | 12.54 | 13.45 | 12.74 | 1.06 |

The full ladder, indexed compiled route, whole-state against case-local, same session:

| case, point | whole-state | case-local | ratio |
|---|--:|--:|--:|
| write/base 1,000, one proposal (ms) | 32.05 | 8.96 | 0.28 |
| write/base 10,000 | 205.41 | 6.80 | 0.03 |
| write/base 100,000 | 1,583 | 7.44 | 0.005 |
| write/noise 100,000 | 1,547 | 10.90 | 0.007 |
| wide/size 10,000 | 2,983 | 420.88 | 0.14 |
| wide/size 100,000 | 326,896 | 30,213 | 0.09 |
| contend/shared, 16 writers, retries/commit | 11.03 | 0.72 | 0.07 |
| contend/shared, 16 writers, commits/s | 22.23 | 1,882 | 84.67 |
| contend/disjoint, 16 writers, commits/s | 1,969 | 1,854 | 0.94 |
| import/core 1,000, journey (ms) | 59,028 | 4,572 | 0.08 |
| import/core 3,000, journey (ms) | 63,056 | 15,800 | 0.25 |
| read/base 100,000, scoped read (ms) | 1,245 | 1,312 | 1.05 |
| read/grouped 100,000, scoped read (ms) | 1,295 | 1,262 | 0.97 |
| replay/retract 100,000, coverage replay (ms) | 844.42 | 1,211 | 1.43 |
| asof/retract 100,000, reconstruct (ms) | 1,194 | 1,286 | 1.08 |

- **Semantics:** inherited violations outside the transition's affected cases no longer block admission, on either evaluator.
- **Computational scaling:** an ordinary posting on the indexed compiled route is flat across a hundred-fold state sweep, the case-local shape rather than a better constant; irrelevant state barely registers.
- **Concurrency scaling:** the shared-key workload no longer carries an excess retry penalty (0.72 retries per commit at sixteen writers against 2.39 for the disjoint control), and the paired whole-state comparison (11.03 retries, 22 commits per second) attributes it: narrowing the read footprint to the touched entry removed the relation-wide dependency pattern stage 1 suffered under SERIALIZABLE.
- **Import** from an empty table no longer depends on PostgreSQL having learned global statistics: about 5.3 ms per transition end to end at 3,000.
- **The limitation, stated with the claim:** case locality removes scaling with unrelated state; it does not remove scaling with the size of the affected case. The wide case at 100k rows, whose invariant quantifies over rows so every row is the affected case, still costs 30.2 s per proposal (whole-state: 327 s).
- **Controls:** the read control did not move; replay and as-of, untouched by this change, sit inside the day's noise band, replay at 100k moving 1.43 one way on the full ladder and 0.65 the other at the quick ladder.

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
- **The whole-state check with indexes tracks the interpreter, both linear, same slope.** The spike's flat ~2 ms curve at 100k and its ten-fold retry reduction came from the case-bound check, which production did not yet run; case-local admission, above, then made it the production rule. Under shared contention throughput rises by half because each proposal is cheaper; retries per commit do not fall.
- **Import gains nothing from indexes on a table growing from empty**, and loses at 1,000: the statistics are the empty table's until something analyses it (verified: the same violation query scans by primary key before one `ANALYZE` and seeks the provisioned indexes after). At 3,000 autovacuum has caught up and the journey is within a quarter. The ruler stays as it is; this is what a fresh deployment sees.
- **Controls:** the read, replay and kernel families are flat across configurations; the indexed configuration's one collateral cost is fixture building, up a fifth to a third at 1,000 entries from index maintenance on bulk inserts, measured with the index condition established outside the timer.

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

`list_scoped` dominates and is fetch + decode of the full in-scope set, not index lookup - the primary key `(predicate_name, arguments)` served the predicate-scoped read as an index-only scan when this was measured, so a dedicated `predicate_name` index would not help. The key has since become `(predicate_name, arguments_hash)`. The only real levers here are materialised derived claims or streaming the fetch, both deferred. Adding 200 000 unreferenced noise claims leaves the read path unchanged (the `ANY([...])` filter skips them server-side).

### Write path (N=100 000 ledger entries, K=2)

`propose_one` is ~1.6 s, dominated by `load_state` + the per-row claim/audit/outbox writes + COMMIT. With no noise every claim is in scope, so scoping has nothing to skip; with 200 000 unreferenced noise claims the scoped path stays ~1.6 s while an artificially-unscoped loader grows ~54%. The kernel itself is not the bottleneck at these sizes - the cost is loading and re-indexing the full in-scope claim set every transition, which is intrinsic until incremental snapshots land.

### As-of replay (`--at 1.0`, full replay)

| N | reconstruct (asserts only) | reconstruct (50% retract) |
|--:|--:|--:|
| 10 000 | ~157 ms | ~136 ms |
| 100 000 | ~1 649 ms | ~1 384 ms |

**Replay is linear in N, and stays linear under a retraction-heavy log.** 10x N costs ~10x reconstruct in both regimes; the 50%-retract log is no worse (slightly cheaper, because its final live set is smaller, so `list_derived_at` enumerates fewer claims). The `ReplaySet`'s amortised-`O(1)` retract holds empirically. This is the curve the roadmap wanted before sizing any snapshot/lattice work: even maximal churn replays linearly, so that work remains unforced.

`as-of --snapshot-every K` runs `refresh snapshots` with that interval before timing, records its build time, storage, snapshot count and the rows replayed after the nearest snapshot, and fails if the read from the snapshots differs from a full replay. With `--snapshot-every 0` (the default, and the suite's) nothing is kept and the scenario measures full replay as before.

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

### Keyed body reads (2026-09-25, `contend --disjoint --prepopulate 5000`, PostgreSQL 18.6)

The law above was measured when a proposal read every predicate its body consulted whole. A body's reads are now keyed by what its arguments fix before it runs, and the synthetic workload's gate (`require not Bench_p(item)` before `admit Bench_p(item)`) is the capture shape that contends. With five thousand items already in each predicate, sixteen writers, twenty operations each, four runs a side in balanced order, baseline `main` against this change:

| implementation | periods | before: retries/commit, commits/s | after: retries/commit, commits/s |
|---|--:|---|---|
| compiled, indexes provisioned | 1 (all share `Bench_0`) | 8.4-8.7, 59-60 | 0.07-0.08, 5,470-5,600 |
| compiled, indexes provisioned | 16 (disjoint) | 8.2-8.4, 54 | 0.00, 4,830-5,050 |
| interpreted (no indexes, by this bench's definition) | 1 | 7.9-8.2, 62-63 | 8.9-9.2, 45-46 |
| interpreted (no indexes) | 16 | 8.1-8.2, 55 | 8.7-9.6, 40-42 |

Two readings. With its index, a keyed read remembers only the index range it searched, so the shared-predicate case that anti-scaled runs at the disjoint case's speed: the unit of concurrency became the rows a body can observe, not the predicate. Without the index, a keyed read scans the predicate computing a key per row, slower than the old whole read and holding the same relation lock, so the interpreted ruler here measures keyed reads unindexed, not the interpreter; the ledger fixture moved nothing on either route, since its posting's only gate reads an empty predicate.

### Embedder / CLI latency (`scripts/embedder_latency.sh`)

Everything above runs in-process. A non-Rust embedder (the reference ETRM) drives Morpholog as `morpholog propose ...`, paying process spawn + parse + validate + a fresh connection + propose + commit + JSON on every call - none of which the in-process bench sees. The harness times the CLI end-to-end (N=50, starting from an empty ledger - state grows over the run - local PostgreSQL):

| invocation | per-call |
|---|--:|
| `morpholog check` (spawn + parse + validate) | ~3 ms |
| `morpholog propose` (+ connect + propose + commit + encode) | ~9 ms |

So the fixed per-call tax a subprocess embedder pays is **single-digit milliseconds** - ~3 ms of spawn/parse/validate plus ~6 ms of connect/propose/commit against a small ledger. That sustains roughly 100 governed transitions/second single-threaded, which is comfortable for lifecycle events (capture, confirm, settle, approve) and admin/audit flows. The propose component grows with in-scope state exactly as the `write` scenario measures; the spawn/parse/validate floor does not. A long-lived worker (load + validate once, keep a pool warm, speak HTTP/socket) is the answer only if a high-frequency path needs it - and per the doctrine those paths should not run through the governed core anyway.

## The `rehearse` ruler (2026-11-02)

Release build, `cdba18ad`, one laptop (Intel Core Ultra 7 155H, governor `powersave`, PostgreSQL 18 on the disposable cluster), two repeats, steady medians. The fixture is the register in the TEC shape; every row decides `same` except in the refusing case, where every version past 100 is refused.

| case | rows | live claims | fetch rows | decide | per row | serialise | end to end | peak RSS, process |
|---|---|---|---|---|---|---|---|---|
| append | 10,000 | 20,000 | 102 ms | 925 ms | 92 us | 0 ms | 1.08 s | 63 MB |
| revise 100 | 10,000 | 200 | 202 ms | 727 ms | 73 us | 0 ms | 0.96 s | 63 MB |
| refuse | 10,000 | 20,000 | 109 ms | 921 ms | 92 us | 7 ms | 1.05 s | 79 MB |
| append | 100,000 | 200,000 | 1.20 s | 29.0 s | 290 us | 0 ms | 31.9 s | 425 MB |
| revise 1,000 | 100,000 | 2,000 | 2.46 s | 7.81 s | 78 us | 0 ms | 10.5 s | 324 MB |
| refuse | 100,000 | 200,000 | 1.15 s | 26.1 s | 261 us | 76 ms | 30.9 s | 580 MB |

What it says, and what it does not. The per-row cost of a decision follows the live set, not the history: the revise case keeps 2,000 claims live and holds 73 to 78 us a row from 10k to 100k rows, while the append case's live set grows with the history and its per-row cost triples over the same span. The code points at the per-proposal clone of the pre-state's overlay and the compaction that keeps it bounded, but the two cases differ in workload as well as in live set, so the 21 s between them is an opportunity, not a measurement of the clone; the `--noise` axis below holds the workload fixed and grows only the irrelevant live set, which is the measurement. The 78 us is the whole decision fold under a small live set, candidate construction, delta computation, comparison and state application included; it is not a floor on what the kernel can do. Removing the clone should take the 100k append case towards the revise case's 8 s, a hypothesis the first experiment tests. A one-second end-to-end run needs more than thirty times the current speed, and the 1.2 to 2.5 s read of 100k rows alone exceeds it, so the pre-decoded in-memory decision and the end-to-end call are two targets, kept apart. Refusing changes little on the decision and adds the report's size, which is where its larger RSS goes: the differences it lists, not the state. The RSS column is the bench process's high-water mark over the run, which holds the decoded rows during the decision; it is not the runtime's footprint on its own. The fixture builds in 6 to 8 s at 100k, outside every sample. Two repeats under `powersave` locate the bottlenecks; an optimisation is judged by the paired A/B discipline in `docs/benchmarking.md`, not by this table.

**The irrelevant live set, held apart (same run).** The 100k revise case again, with 300,000 `Noise` claims no rule reads admitted before the register's rows. `decide_register` is the register's rows alone, timed from the progress call that ends the noise prefix, so the noise rows' own decisions are not in it.

| rows | live claims | decide, all rows | decide, the register's 100k rows | per register row |
|---|---|---|---|---|
| 100k revise | 2,000 | 8.0 s | 8.0 s | 80 us |
| 300k noise, then 100k revise | 302,000 | 234.6 s | 151.3 s | 1,513 us |

The same hundred thousand decisions cost nineteen times more behind state they never read. Nothing else differs between the first two rows: the same acts, the same keys, the same rules, the same plans, which bound every decision to its own row. The cost is in the per-proposal clone of the pre-state's overlay and the compaction that keeps it bounded, each proportional to the live set, which is what the candidate-state view in #674 removes. Peak RSS 1.14 GB with the noise, 400k decoded rows held in memory among it. The acceptance condition for the view is this table's two `decide_register` figures coming out about the same.

### After the view (2026-11-02, same machine and configuration, one repeat)

Two changes, each measured on its own against the ruler above. Step A reads a proposal's candidate through a view over the pre-state and its staged delta instead of cloning the overlay; step B drops the rehearsal's own compaction policy, which existed to keep those clones small, and leaves the state its ordinary maintenance. The counters are the state's own: clones made and overlay entries copied, full rebuilds and claims re-indexed, over the register's rows alone; the boundary counters arrived with step A, so the noisy row's `before` count is not split by rows (the whole run, noise prefix included, rebuilt 226 M).

| 100k register rows | live claims | before | step A | step B | per row after | clones, A | claims rebuilt, before / A / B |
|---|---|---|---|---|---|---|---|
| revise | 2,000 | 8.4 s | 3.75 s | 3.55 s | 35 us | 0 | 3.1 M / 1.5 M / 1.5 M |
| append | 200,000 | 31.0 s | 24.5 s | 2.43 s | 24 us | 0 | 36.5 M / 36.5 M / 1.6 M |
| refuse | 200,000 | 26.1 s | | 2.58 s | 26 us | | |
| revise behind 300k irrelevant claims | 302,000 | 151.3 s | 155.1 s | 4.91 s | 49 us | 0 | not counted by rows before / 154 M / 1.8 M |

What each step bought. The view removed every clone and with it a fixed cost per decision that showed even at two thousand live claims: the revise case more than halved. It did not touch the 300k case, whose suffix spent its 155 s in 511 rebuilds of a 300k-claim state; the clones had been the noise prefix's cost. Dropping the rehearsal's compaction policy took that suffix from 155 s to 4.9 s and the append case from 24.5 s to 2.4 s: six rebuilds instead of 511, under the state's ordinary threshold. The acceptance condition was the two revise figures coming out about the same, 8.0 and 151.3 before; they are 3.55 and 4.91 now, so it is substantially met, with a penalty of about two fifths left. That remainder is the six rebuilds of a 302k-claim state the ordinary threshold still makes: the state's maintenance keeps a global component, and whether a large state can compact locally, without re-indexing predicates no rule read, is a question of its own, worth asking at millions of live claims. No decision, witness or error moved; the fingerprint corpus and the kernel-SQL differential are unchanged. The read is now the larger share of an end-to-end run: 1.2 to 4.8 s of fetching and decoding against 2.4 to 4.9 s of deciding.

**The ordinary proposal path, paired against the parent commit.** The view changes `propose` as well as the rehearsal, so the quick suite ran interleaved, four runs a side, parent against candidate (`scripts/bench_ab.sh 15a31914`, session clean, both sides throttled alike). Every `write`, `read`, `replay` and `wide` metric was within noise, and so was the chained `acts_sequential`; the one finding was at the kernel's own scale, where a bare `propose` with no invariants ran 1.33 to 1.37 times slower, about 3 us on 10, because the owned path built the view and then the owned candidate. The owned path now builds its owned candidate once and judges it, as before, through the one judgement the view path shares; a second interleaved compare, four runs a side, has `propose_no_invariants` at 1.00 and 0.97, `propose_one` at 0.98 and 0.99 and every kernel phase within noise. The counters are compiled only for tests and under the bench's `tally` feature, so an ordinary build carries none.

### Where a decision's time goes (2026-11-03, `rehearse --phases`, same machine, two repeats)

The decision step by step over the same 100k rows, each step on its own clock, from outside the kernel (which reads no clock): the act's execution (`stage_delta`), the candidate's preparation (the effective delta and the view, repeated outside the decision to be timed), the whole decision (`decide_staged_delta_with`, the preparation included), and the state's advance by the row's recorded effect (`advance_recorded`, the production fold's own step, taken whatever the candidate decided). The ordinary fold's `decide_rows` is beside them.

| 100k rows | stage | candidate | decide, whole | residual decision work | advance | fold total | per row |
|---|---|---|---|---|---|---|---|
| append | 101 ms | 115 ms | 1,011 ms | about 900 ms | 1,036 ms | 2,350 ms | 24 us |
| revise 1,000 | 147 ms | 268 ms | 1,854 ms | about 1,590 ms | 1,101 ms | 3,439 ms | 34 us |
| refuse | 115 ms | 139 ms | 1,224 ms | about 1,090 ms | 1,362 ms | 2,436 ms | 24 us |

What it measures. Executing the act is about 1 us a row and preparing the candidate 1 to 3 us: neither is where the time is. Two terms of about equal size take the rest. The residual decision work, the decision less its preparation, is 9 to 16 us a row: everything `decide_staged_delta_with` does after the candidate is prepared, which is the impact classification, the invariants' matching over the touched cases (two or three case-bound rules, a keyed lookup or two each), the state lookups those make, the temporaries' destruction and, on refusal, the diagnosis. These timers do not separate those; how much of the residual is the matcher's binding maps and match vectors (#352's question) stays open until a profile distinguishes them. The state's advance is 10 to 14 us a row: the index maintenance behind every admitted claim (a map entry per argument value), the retraction's lookup, and the ordinary compactions (1.5 M claims re-indexed over the run). That term is as large as the residual and #352 did not name it. The steps sum to within a few hundred milliseconds of the fold's total either way; the rehearsal's own comparison of the recorded effect with the decided one is inside that difference, with the run-to-run noise. The refusing run's advance reads a third more than the append run's over the same rows and the same step, minutes apart; the step does not depend on the verdict, and this ruler does not say why.

### The state's advance in its parts (2026-11-03, `rehearse --phases`, two repeats)

The same rows again, two ways. First the production advance, with the calls that compacted timed apart: a call counts as compacting when the state's rebuild counter moved during it. Then the recorded operations applied one at a time with the compaction held off: each retraction, each admission's presence check on its own, and each admission (the check again, then the index entries). The run's absolutes read 20 to 30 percent above the table before, on the same idle machine; only shares within one run are compared.

| 100k rows | advance, whole | calls that compacted | compactions | claims re-indexed | retractions | presence checks | admissions |
|---|---|---|---|---|---|---|---|
| append, 200k live | 1,311 ms | 1,148 ms | 40 | 1.58 M | 0 ms | 22 ms | 146 ms |
| revise, 2k live | 1,334 ms | 1,065 ms | 775 | 1.55 M | 106 ms | 31 ms | 115 ms |

What it measures. The calls that compacted are 80 to 90 percent of the advance, about 0.7 us for each claim they re-indexed. The incremental work held apart, finding what to retract, checking presence, and adding the index entries, is 1.5 to 2.5 us a row, and it agrees with the advance less its compacting calls. Over either run the compactions re-index about sixteen claims for every row applied. They get there by different routes. In the growing register the threshold is an eighth of the base, so the state is rebuilt each time it grows by an eighth, about eight times its final size in all, with few retractions to reclaim. In the revised register the live set never passes the fixed floor, so a 2k state is rebuilt every 130 revisions or so, each adding about four entries of churn against a floor of 512, most of them dead. The state's index maintenance is not where its advance goes; its compaction policy is. Whether a different policy keeps the rest as fast is a separate question this ruler does not answer: a state that compacts less carries more overlay, which reads search and an ordinary proposal's owned candidate copies.

### Compaction policies over one recorded history (2026-11-03, `rehearse --policies`, three rounds)

The rehearsal's fold over the same rows three times, under three policies for when the state folds its overlay into a new base: the state's own (overlay entries plus retracted base positions past an eighth of the base, or 512), a relaxed threshold (the same past half the base, or 2,048), and a split one that bounds two causes apart (dead entries held to the live count, or 1,024; live overlay entries allowed to reach the base's size, or 4,096). Each fold decides every row over the state the record built and advances it by the recorded effect. One row in each thousand, at an offset that wanders from block to block and is the same for every policy, also builds the owned candidate an ordinary proposal builds: a copy of the state with the row's change applied. The peaks are read after every operation, before any compaction it triggers. The order rotates each round; steady medians of the two later rounds.

Two checks refuse a run: the state's own policy must rebuild as many times as the production fold did, and every policy must end in the same state. Both were broken and seen to refuse. Neither proves the rebuilds fell at the same rows, or that every intermediate state and decision matched. A unit test crosses each of the split policy's bounds and holds its logical state, order included, to the state's own policy over the same changes.

| | current | relaxed | split |
|---|---|---|---|
| **append, 200k live** | | | |
| advance | 1,005 ms | 452 ms | 350 ms |
| decisions | 1,112 ms | 1,120 ms | 1,128 ms |
| owned candidate, each | 1.5 ms | 5.7 ms | 9.1 ms |
| rebuilds, claims re-indexed | 40, 1.58 M | 11, 0.47 M | 6, 0.26 M |
| most entries held, of them dead | 200k, 0 | 200k, 0 | 200k, 0 |
| **revise 1,000, 2k live** | | | |
| advance | 1,243 ms | 575 ms | 453 ms |
| decisions | 2,155 ms | 2,174 ms | 2,164 ms |
| owned candidate, each | 63 us | 180 us | 381 us |
| rebuilds, claims re-indexed | 775, 1.55 M | 194, 0.39 M | 99, 0.20 M |
| most entries held, of them dead | 2,256, 257 | 3,024, 1,025 | 3,998, 2,000 |
| **revise behind 2M unrelated claims** | | | |
| advance | 20.8 s | 7.4 s | 4.3 s |
| decisions | 4.11 s | 4.17 s | 4.21 s |
| owned candidate, each | 19 ms | 66 ms | 133 ms |
| rebuilds, claims re-indexed | 61, 18.9 M | 17, 5.4 M | 9, 2.1 M |
| most entries held, of them dead | 2.11 M, 111k | 2.20 M, 198k | 2.20 M, 198k |

What it measures. The advance falls two to three times under the relaxed threshold and three to five times under the split policy. Behind two million unrelated claims that is 16.5 s saved, where the state's own policy spends 20.8 s of the fold's 26.9 s re-indexing 18.9 M claims. The decisions, the reads and the invariants over the touched cases, do not move with the policy on any of these histories.

What the split policy retains. After each operation it holds at most live + max(live, 1,024) entries: twice the live state once 1,024 claims are live, more below that, where a two-claim register may carry 1,024 dead entries. These are claim entries, not bytes; index allocations and the temporary storage of a rebuild or a candidate are not counted.

What it costs. The owned candidate copies the overlay, so it is three to four times dearer under the relaxed threshold and six to seven times under the split policy, because the overlay is let grow. The database proposal path loads its state afresh for every proposal, a base with no overlay, so its candidate copies nothing whatever the policy. The copy is paid by a caller that keeps a state in memory and proposes against it across many changes, an embedder of the kernel or an atomic batch advancing between acts. The cheapest fold and the cheapest owned candidate come from different policies; which a state should use depends on what it is used for, and this ruler does not decide that.

### A replay's upkeep in production (2026-11-04, paired against the parent commit)

The split policy became a replay's upkeep, `State::apply_replayed`, for the folds that never copy their state: rehearsal, scoring, receipts and as-of replay. Proposals, atomic batches and the live verifier keep the ordinary upkeep. Two binaries, built from the parent commit and this one, were run interleaved, baseline, candidate, candidate, baseline, from copies that no rebuild could swap.

| production rehearsal fold, 100k rows | parent | this |
|---|---|---|
| append, 200k live | 3,360 and 2,791 ms | 1,933 and 2,010 ms |
| revise 1,000, 2k live | 4,411 and 4,112 ms | 3,393 and 3,285 ms |
| revise behind 2M unrelated claims | 27.7 and 27.2 s | 10.7 and 10.8 s |
| same, peak RSS | 5.5 GB | 4.3 GB |
| same, rebuilds and claims re-indexed | 61, 18.9 M | 9, 2.1 M |

The last baseline run of the large case came a day after the other three, the session having ended between them; it agrees with the first within 2 percent. Before each large-case run the order table was compacted outside every timer, the stopgap for the fixture's slow insert (#682).

The bench suite, four runs a side interleaved by `scripts/bench_ab.sh`, completed clean with both sides throttled alike. Every proposal metric, `propose_one` on every scenario and the kernel's sequential acts, is within noise or lower. As-of reconstruction of a thousand admissions is 21 percent faster. One read is slower: a derived read at a thousand revisions, 4 percent or about 1 ms. Its history leaves 30 claims live beside 990 dead, under the replay's dead floor of 1,024, so the read skips slots the ordinary upkeep would have cleared. Three reads this change does not reach read 4 to 6 percent higher (`build_state` twice, one `list_scoped`).

### The matcher's binding pressure (2026-11-04, `morpholog-matcher`)

A separate binary, so its counting allocator never slows the suite. Six shapes from #352, each one rule or one act over an in-memory state built outside every timer: a long `and` chain whose conjuncts bind what the next reads, an `or` whose branches export the same bindings, `forall` over a claim pattern with an `exists` inside, a defined call projecting a variable its caller never sees, a sum over many witnesses, and an act's `for` with a `bind` per item. Each point reports the median of 31 evaluations, and for one more the allocations it made and the binding maps the evaluator copied, counted behind the `measure` feature at the matcher's copy sites and the `for` loop's frame restore. The counts are exact and repeat run to run; the times are one laptop's, under `powersave`.

| shape | witnesses | live vars | per witness | allocations | binding copies | entries copied |
|---|--:|--:|--:|--:|--:|--:|
| and chain | 10,000 | 2 | 0.9 us | 19 | 3 | 3 |
| and chain | 10,000 | 4 | 2.5 us | 46 | 5 | 10 |
| and chain | 10,000 | 8 | 6.8 us | 123 | 9 | 36 |
| or branches | 10,000 | 2 | 1.0 us | 20 | 3 | 4 |
| forall, exists | 10,000 | 2 | 2.1 us | 50 | 8 | 8 |
| defined call | 10,000 | 2 | 3.3 us | 73 | 11 | 13 |
| sum | 10,000 | 3 | 3.8 us | 70 | 10 | 10 |
| for, bind | 100 | 2 | 10 us | 229 | 2 | 3 |
| for, bind | 1,000 | 2 | 84 us | 2,042 | 2 | 3 |
| for, bind | 10,000 | 2 | 808 us | 20,042 | 2 | 3 |

What it measures. Every rule shape costs about the same per witness at 100, 1,000 and 10,000 witnesses, within a fifth: the matcher is linear in its witnesses. Per witness it is not cheap. The chain's cost grows with the variables live in the frame, 0.9 us at two and 6.8 us at eight, and its allocations with it, 19 to 123. Each copy of a binding map copies every entry, and every entry is a variable name and a value that each own a string, so the entries copied grow with the square of the chain's length (3, 10, 36). Which share of the time those allocations take, the counts do not say; they say where to look.

The act's `for` is quadratic in its collection: per item 10 us at a hundred items, 808 us at ten thousand. Its binding copies stay at two per item, but its allocations per item are twice the collection's size plus a constant at every size. The collection is an argument of the act, so it sits in the binding map, and each of the two copies per item, the loop's frame restore and the `bind`'s match, copies the whole collection. That is a defect on the ordinary proposal path, apart from this measurement (#686).

### Bound values shared, the effective delta linear (2026-11-04, paired against the parent commit)

Two changes for #686. A binding map's values that own heap memory, subjects, collections and quantities, are shared rather than copied, so copying the map costs its entries and not what they hold; the rest stay inline, where a shared box would cost an allocation a copy never did. And the effective delta checks membership through a set once a delta passes a few dozen claims, where a scan per claim made an act that stages n claims cost n squared. The matcher harness, the parent's binary and this one run interleaved, baseline, candidate, candidate, baseline:

| shape | parent, per item or witness | this | allocations, parent and this |
|---|---|---|---|
| for, bind, 100 items | 9.0 us | 1.4 us | 229, 26 |
| for, bind, 1,000 items | 80 us | 2.1 us | 2,042, 39 |
| for, bind, 10,000 items | 764 us | 2.2 us | 20,042, 39 |
| and chain, 10,000, 8 live vars | 6.4 us | 6.1 us | 123, 95 |
| and chain, 100, 8 live vars | 5.5 us | 4.6 us | 124, 96 |
| defined call, 10,000 | 3.0 us | 2.9 us | 73, 66 |
| sum, 10,000 | 3.57 us | 3.65 us | 70, 70 |

The act's `for` is now linear in its collection, ten thousand items in 21 ms rather than 7.6 s, its allocations a constant per item. The rule shapes are up to 17 percent faster and allocate less wherever a subject is copied; the sum over decimal lines, which copies none, is unchanged within 2 percent. A first version shared every value and boxed the decimals too: that sum read 7 to 10 percent slower until heap-free values were held inline.

The production paths do not move. The 100k rehearsal ruler, interleaved the same way, reads 1.57 and 1.55 s against 1.52 and 1.53 s appending, 2.65 and 2.64 s against 2.66 and 2.66 s revising: within noise either way. Its rules bind two or three variables to subjects drawn from a few claims, so a binding copy was never its cost. The bench suite, four runs a side by `scripts/bench_ab.sh`, completed clean with no metric outside noise.

### One defined call, many projections (2026-11-05, paired against the parent commit)

The defined-call shape projects three paths per root, so no single call yields many matches. The fan-out shape is one call over a group whose members each carry a tag the call hides: its distinct projections grow with the group. Each was checked against every projection kept so far, so the call was quadratic in them. Past 32 a set of the projected values now checks membership, and a smaller result keeps the scan. Baseline, candidate, candidate, baseline:

| shape | parent | this | allocations, parent and this |
|---|---|---|---|
| defined fan-out, 100 | 173, 230 us | 109, 109 us | 1,725, 1,827 |
| defined fan-out, 1,000 | 9.6, 16.8 ms | 0.95, 0.95 ms | 17,031, 18,037 |
| defined fan-out, 10,000 | 1.66, 0.83 s | 9.8, 9.8 ms | 170,039, 180,048 |
| defined call, 10,000 | 28.9, 28.8 ms | 28.8, 29.6 ms | 660,014, 660,014 |

The call is now linear in its projections, about 1 us each. The quadratic's spread across the parent's two runs is its own. The three-path call allocates exactly what it did and reads within noise, as does every other shape. A first version used the set for every call: it cost the three-path call four allocations and 8 to 10 percent until small results kept the scan.

### The production rehearsal, reconciled (2026-11-05, `rehearse`, three repeats)

The ruler's `fetch_rows` and `decide_rows` time a different path from `end_to_end`: the first reads the whole history into memory, the second decides over it, while `rehearse_candidate` reads a page, decides it, and reads the next. Their sum is not the end-to-end time. So timers now run inside the paths themselves, behind the `measure` feature: the page queries through the driver's rows, the decoding of those rows, each row's decision and each advance. Within `end_to_end` what is left over is the transaction, the row count and the report. Steady medians over 100k rows:

| 100k rows | page queries | decoding | decisions | advances | other | end to end |
|---|---|---|---|---|---|---|
| append | 605 ms | 402 ms | 1,195 ms | 292 ms | 142 ms | 2,637 ms |
| revise 1,000 | 856 ms | 631 ms | 2,128 ms | 319 ms | 166 ms | 4,099 ms |
| `fetch_rows`, append | 684 ms | 499 ms | | | | 1,191 ms |
| `fetch_rows`, revise | 1,349 ms | 1,087 ms | | | | 2,444 ms |

What it measures. The decisions are the largest term: 45 percent of the append run and 52 percent of the revise run, 12 and 21 us a row. The production read is 38 and 36 percent, split about 60 to 40 between the queries and the decoding. The read held whole costs more than the paged one, much more for revise, 2.44 s against 1.49 s, so `fetch_rows` overstates what a rehearsal pays to read. The decision timer includes the comparison of each decision with the recorded one. The query timer includes the database's execution, the transfer and the driver's own row decoding, which these timers do not separate. The timers add four clock reads a row; end to end reads within 2 percent of the uninstrumented run before them.

### Cheaper index lookups (2026-11-05, three binaries interleaved A B C C B A)

A profile of the decisions put SipHash at about a quarter of them, most of it the state's argument index, which a rule looks claims up in by value. Three binaries were compared: the parent (A); a lookup that borrows the bound value instead of cloning it (B); and that with aHash for the argument index alone (C). The aHash index is keyed per map from the standard library's random keys. Means of two runs each over 100k rows:

| 100k rows | decisions, A B C | advances, A B C | end to end, A B C |
|---|---|---|---|
| append | 1,177, 1,157, 1,096 ms | 296, 292, 265 ms | 2,614, 2,585, 2,509 ms |
| revise 1,000 | 2,118, 2,067, 1,946 ms | 337, 336, 299 ms | 4,162, 4,088, 3,908 ms |
| 100k rows behind 100k noise claims | 1,347, 1,318, 1,265 ms | 570, 573, 521 ms | 3,876, 3,856, 3,717 ms |

What it measures. Borrowing takes about 2 percent off the decisions; aHash about 5 percent more, so 6 to 8 percent together. aHash also takes 9 to 11 percent off the advances, which build and extend the same index. End to end is 4 to 6 percent faster. Peak memory does not move beyond a few MB. On the matcher shapes borrowing is 3 to 9 percent faster with a tenth fewer allocations, and aHash 2 to 5 percent more; the sum, which looks nothing up by a subject, does not move. Every fixture's subjects are short; a lookup alone, outside the kernel, takes about half the time under aHash at every subject length from 8 to 128 characters. The bench suite, four runs a side, flagged one cell, `write/base` at 100 entries, with its database-only phases higher too; eight further runs of that scenario alone read the candidate equal or faster in every phase.

aHash takes its AES path only when a build enables that CPU feature, and Morpholog's builds do not, so these are its portable path's numbers: the path every default build takes, on any processor. They are still this machine's.

### Classifying a delta without hashing its cases (2026-11-05, three binaries interleaved A B C C B A)

A profile put about half of impact classification in the set that keeps each touched case once: hashing whole case maps, cloning them in, and dropping them. Three binaries were compared: the parent (A); the set keyed on borrowed values instead of a cloned map (B); and the cases compared directly until there are 32, then the set, as a defined call's projections already are (C). Means of two runs each over 100k rows:

| 100k rows | decisions, A B C | judgement phase, A B C |
|---|---|---|
| append | 1,128, 1,092, 1,034 ms | 877, 853, 809 ms |
| revise 1,000 | 2,059, 1,999, 1,943 ms | 1,739, 1,652, 1,553 ms |
| 100k rows behind 100k noise claims | 1,310, 1,309, 1,272 ms | 957, 947, 902 ms |

What it measures. An ordinary delta touches a handful of cases, so C hashes no case there, and takes 3 to 8 percent off the decisions to B's 0 to 3. End to end, append is 5 percent faster under C; the revise and noise runs moved with their database-only phases, which this change cannot touch, so they read nothing. The matcher's new wide delta, one act admitting a case per witness, stays linear under all three; at 10,000 cases B is 9 percent faster and C within 4 percent of A. Under the default release build the matcher read C up to 17 percent slower even on shapes that classify nothing, with the same allocations, the extra time in value comparison. Built with one codegen unit, most shapes came within 2 percent and none past 8, and the worst one, an `and` chain at 10,000 witnesses and eight variables, slowed under A to C's time. The gap is real in the binary that shows it, but it follows how the compiler splits the crate, not what classification does, so any later change can move it either way.

The scan switches on distinct cases, not on cases compared, so an act staging many claims over few cases compares each against up to 32 without ever building the set. A matcher shape measures that: one act staging 10,000 claims over 1, 8, 16, 32 or 64 cases, the same three variants, four runs each in the default build and in one codegen unit. At 32 cases, the scan's largest, C costs the whole act at most about 2 percent more than A and 3 to 6 percent more than B; at 8 cases or fewer it is level with both or faster. Up to 32 cases it makes 30,000 fewer allocations than A, which cloned every claim's case into its set, duplicates included.

### Deferred

- **As-of reads that cost what the answer costs.** Replay is linear in the transitions before the target, under retraction and revision alike. The TEC register made that unacceptable for an interactive read, and `asof/revise` is its ruler: a small live set over a long history, where the cost is all history.
- **Streaming `sqlx::query::fetch`** instead of `fetch_all`. Memory scales with audit rows fetched; not pressing until past N=1M (per-row payload is ~200-500 bytes).
- **Materialised derived claims**, with invalidation modelled as ordinary claims (not cache machinery). The current `list_scoped`-dominated read cost is the pressure; awaits a worked example.
