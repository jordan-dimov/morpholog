# Spike verdict: compiling invariant checking to SQL

**Question:** does a compiled relational core make commit latency flat in in-scope
state and un-serialise value-partitioned writers, without disagreeing with the kernel?

**Verdict: GO.** Every pre-registered criterion passed; see the conditions the real
feature must additionally meet at the end.

Branch `spike/sql-invariants` (never merges). Machine: see appendix.

## Kill-signal probe (step 2) - PASSED

N=10k entries (30k claims), hand-written residual for `balanced_posted_entry`
(case-bound to one entry_id), partial expression indexes
`(arguments -> 0 ->> 'value') WHERE predicate_name = '<P>'`. Probe:
`scripts/spike_kill_signal.sh` (re-runnable).

| | with indexes | without (control) |
|---|---|---|
| execution | 0.198 ms (16 buffers) | 176 ms (4369 buffers) |
| plan | index/bitmap scans, zero Seq Scan | full predicate-prefix scan + Seq Scan subplans |
| SIRead locks | page x2 + tuple x3, NO relation lock | relation-level on claims AND claims_pkey |

## What was built

- `spike/compile.rs` - fragment compiler (Claim/And/Implies/Forall/Exists/Not/
  Compare-Decimal/Eq/Neq/Sum over subjects+decimals; raw-JSONB Eq for other kinds),
  denial-oriented violation queries with witness columns, stage-2 case binding via
  occurrence binders (sound by widening), whole-run refusal classifier. Ledger SQL
  byte-pinned.
- Kernel split `propose_stage_delta` (body-only execution; all 184 core tests
  unchanged; parity test).
- `spike/propose.rs` - compiled propose: body-scoped load (invariant footprints never
  fetched/decoded/indexed), delta written first inside the SERIALIZABLE tx (the claims
  table IS the candidate), per-invariant violation queries, identical
  commit/rollback/rejection-log contract. Plus `propose_differential`: kernel verdict
  and both compiled stages against ONE snapshot, error on disagreement.
- `spike/indexes.rs` - partial expression indexes derived from the compiled set.
- Bench axes `--engine {interpreted|stage1|stage2}`, `--arg-index {none|spike}`,
  `--repeat` (all header-echoed).

## Fragment & coverage census (`spike_coverage.rs`)

74 of 99 gallery invariants compile in the minimal fragment. Whole programmes
in-fragment: 01 settlement_netting, 02 verified_revenue, 03 double_entry_ledger
(target, 3/3 incl. the discipline-generated uniqueness rule), 04 approval_controls,
09 carbon_credit_provenance. Refusal reasons per invariant recorded by the census
test. Largest refusal classes: arithmetic in invariant bodies, `pre(...)`,
non-Decimal comparison domains, `defined` calls, or/xor, round/abs.

## Correctness

- **Differential harness** (`spike_differential.rs`): eval_totality-style argument
  vectors (baseline + boundary witnesses incl. zero/negative/DECIMAL_MAX/shared
  subjects) over a depth-2 governed frontier held in PG; every probe judged by
  `propose_differential`. **412 probes over five programmes, zero disagreements.**
- **Witness contract**: verdict + invariant name + version strict; witness VARIABLE
  SET strict; values observational. Two lawful value divergences observed and
  documented in code: symmetric self-join pair order (kernel candidate order appends
  the delta last; SQL sorts it into key order) and `new Subject()` bodies minting
  different fresh ids per execution. On the ledger smoke, witness values matched the
  kernel byte-for-byte.
- **Dirty history** (`spike_compiled.rs`): pinned triple - over an unbalanced legacy
  entry, a fresh balanced write is refused by kernel and stage 1, COMMITTED by
  stage 2; a worsening write refuses everywhere. The divergence runs in exactly one
  direction (Decker & Martinenghi case-boundedness).
- **Break-check**: flipping `Eq` to `<>` in the compiler reddens the sweep;
  restoring greens it. The harness is a real gate.
- Not run (recorded honestly): the seeded ~2k-op randomized governed streams; the
  frontier + boundary vectors stood in.

## Measurements

Warm medians (runs 2-5 of `--repeat 5`), cold in parens. `propose_one`, ms.

### Table W - write scenario

| N | interpreted | stage1 idx | stage2 no-idx | stage2 idx |
|--:|--:|--:|--:|--:|
| 1 000 | 23 (26) | 15 (16) | 3 (5) | 1-3 |
| 10 000 | 132 (149) | 135 (136) | 16 | 2-3 |
| 100 000 | 1 293 (1 655) | 1 621 (1 851) | 141 (163) | **2 (3-6)** |
| 100k + 200k noise | - | - | - | **2 (5)** |

- Criterion (a) PASS: stage2+idx is FLAT (ratio ~1x across 100x state growth,
  absolute ~2ms; interpreted ratio ~56x). ~650x faster than interpreted at 100k.
- Stage 1 stays linear by construction (full-body aggregate) - it is the
  differential anchor, not the perf target.
- Stage2 no-idx grows with N: compilation alone does not pay; compilation PLUS
  something to seek on is the unit. (b)'s mechanism control.

### Table C - contend, 16 workers x 20 ops, prepopulate 2000 (single runs)

| periods | interpreted | stage2 no-idx | stage2 idx |
|--:|--:|--:|--:|
| 1 | 10.7 r/c, 10.4 c/s | 10.5 r/c, 164 c/s | **1.19 r/c, 2 451 c/s** |
| 16 | 11.2 r/c, 9.3 c/s | 11.3 r/c, 160 c/s | 1.13 r/c, 2 313 c/s |
| disjoint-16 control | - | - | 6.0 r/c, 1 033 c/s (no index on Bench_*) |

- Criterion (b) PASS: the headline is periods=1 - shared-predicate value-keyed
  writers go from ~10.7 to ~1.19 retries/commit (below the historical predicate-
  disjoint floor ~2.4) and throughput rises ~235x. The no-idx control's retry RATE
  stays flat at ~10.5 (its throughput rises only because each attempt is shorter) -
  the SSI footprint is what moved, exactly as the pg_locks probe predicted.
- Value partitioning stays irrelevant in every engine, as pre-registered.

## Two planner incidents (why the EXPLAIN regression tier is mandatory)

Both found by measurement, both fixed on the branch, both of the exact
"compiled rules are query-planning-sensitive" class prior-art.md:20 warns about:

1. **JIT tax**: correlated-subquery cost estimates trip the JIT threshold at
   N=100k - ~118ms of JIT compilation for a sub-ms plan. Fix: `SET LOCAL jit = off`
   in the check transaction (point probes, not analytics).
2. **ORDER BY plan flip**: ordering by raw `t0.arguments` matches the PK, baiting an
   early-stop scan of the entire predicate (23ms scanning 100k rows to find 0). Fix:
   order by the witness EXPRESSIONS, which match the spike indexes instead.

Measurement hygiene note: the first no-idx cells were contaminated by leftover
hand-created probe indexes (different names than the bench's derived set, so
`--arg-index none` did not drop them). Caught because the negative control failed to
degrade; re-measured after dropping. The real feature's index management must own
ALL its index state, and negative controls belong in every measurement.

## Criteria

- (a) flat-vs-N within 2x at 100k: **PASS** (~1x; absolute 2ms << 50ms).
- (b) contend periods=1 retry_rate <= ~2x disjoint floor, throughput >= 5x, no-idx
  control flat: **PASS** (1.19 r/c; 235x; control flat).
- (c) zero differential disagreements; divergence only in the pinned direction:
  **PASS** (412 probes; dirty triple pinned; break-checked).
- (d) 100% bench+03 compile coverage; census recorded: **PASS** (74/99 overall).
- (e) residual-risk list: **below**.

## What the real feature must additionally solve

- `pre(...)` (transition-relational invariants): two-state SQL, wholly unexplored.
- `Defined` transitivity: inlining through definition frames (projection-with-dedup
  semantics in SQL - the dedup is observable under `Sum`).
- Or/Xor multiplicity inside `Sum` bodies: heterogeneous binding rows; the classic
  silent-wrong-number risk. Keep refused until differential-proven.
- Checked-arithmetic parity: PG numeric is wider than rust_decimal; a kernel
  `ArithOutOfRange` has no SQL analogue - at DECIMAL_MAX the compiled path would
  commit what the kernel cannot evaluate. The spike falls back before this bites
  (arith is out of fragment); the real feature needs a decided semantics.
- Witness identity: adopt the weakened contract ("some violating binding,
  deterministic per query") or reproduce state order exactly; today's ORDER BY
  choice is also load-bearing for plan quality (incident 2), so the weakened
  contract is recommended.
- Per-compiled-invariant EXPLAIN/plan-shape regression tests (both incidents above
  prove the need); this spike carried probe scripts, not durable tests.
- Index lifecycle: derived indexes as part of `morpholog init`/`migrate`, owned
  wholly by the runtime.
- Arity-skew rows (`list_claims_where` refuse-don't-exclude parity), and the
  rejection-log/audit `invariants_checked` semantics for stage-2-skipped invariants
  (currently recorded as checked - decide whether "skipped-as-irrelevant" is a
  distinct audit fact).

## Roadmap edit (for main, after this branch is archived)

Replace roadmap.md:19's "Spike and measure against the bench before committing to
it" with: "Spiked and measured (2026-07-29, branch spike/sql-invariants, verdict in
design-history): flat commit latency (~2ms at 100k claims vs 1.3s interpreted) and
shared-predicate contention collapsed to ~1.2 retries/commit at 16 workers. GO,
with the residual-risk list as the feature's opening spec."

## Appendix: environment

Linux 7.0.0-28-generic; PostgreSQL 18 (system, morpholog_dev); cargo --release;
jit=on by default (hence incident 1), SET LOCAL jit=off on the check path;
max_pred_locks_* at defaults; single-shot contend cells (320 ops aggregated per
cell), write cells warm-median of 5.
