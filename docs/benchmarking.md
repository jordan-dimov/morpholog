# Benchmarking discipline

How performance work is measured in this repository, and the rules that keep the measurement honest - including against the AI agent that does most of the optimising. The instrument itself lives in `crates/morpholog-bench` (its README is the manual and holds the current readings); this document owns the discipline.

The one-sentence version:

> You may make Morpholog fast. You may not make the test of "fast" easier.

## The suite contract

`morpholog-bench suite` runs a **frozen matrix of named cases** across per-case ladders and prints one table (markdown for PR bodies, `--format json` for machine comparison). The matrix is versioned by `suite_contract=<N>` in the output header.

**A performance PR may add or change the implementation being measured. It may not alter the case matrix, fixtures, ladders, parameters, or aggregation semantics.** Any change to those is a change to the *ruler*, not the machine: it lands in its own commit, reviewed on its own, bumping `suite_contract` - and it lands **before** any optimisation it will judge. The contract is a tripwire, not just a promise: a DB-free test pins a fingerprint of both canonical plans, so editing `suite_plan` without bumping the contract goes red, and the ruler change becomes a conspicuous pinned-hash diff rather than one innocent parameter in a large PR. This is the bench-first rule: a new scenario and its baseline table are reviewed by someone who has not yet seen the optimisation, so the number the optimisation later moves is one nobody could retro-fit.

## Cases are consumer-derived, with provenance

A scenario earns its place the way a worked example does: a real workload forced it, and the case carries that provenance (the suite prints the mapping above its table). The current families:

- `write`, `read`, `asof`, `contend` - the original mechanism scenarios; their *complement* cases (`/noise`, `/grouped`, `/retract`, the workers=1 baseline) are deliberate controls, not defaults. The concurrency law is held by **same-workload A/Bs**: `contend/shared` vs `contend/value-partitioned` (both ledger - value sharding does not relieve SSI pressure) and `contend/predicate-shared` vs `contend/disjoint` (both synthetic - footprint partitioning does). The two scaling curves use different workloads and never compare to each other. The controls are what let a reader say **why** a number moved, and they are the first thing a narrow optimisation regresses.
- `import/core` - the in-process core of the embedder import/replay path (the workload that forced `propose --batch`; Redline's 130-act WAN seed; grid-mysteries' CI replay). Its ladder is deliberately small: per-commit cost grows with the book on the interpreted runtime, so the 0->N journey is roughly quadratic - the finding, not a reason to make the suite take days.
- `wide/size`, `wide/arity` - the argument-count axis (the billing embedder's 13-ary `InvoiceLine`; the gallery tops out at 7-ary).

Deferred cases, provenance recorded so they arrive bench-first when wanted: the O(audit-length) governance folds (`audit verify` replay, checkpoint creation, coverage - grid-mysteries runs pack verification in CI); `refresh_derived` (note: the bench's `RESET_SQL` does not truncate `morpholog_read.*`, so that scenario must extend the reset); session steady-state (owned today by `scripts/embedder_latency.sh`, which also holds the house percentile style).

## Curves, not points

The deliverable of a measurement is a **complexity-class claim** ("flat across a 100x state sweep", "linear in log length", "independent of unrelated-predicate count"), never a single number. Points invite tuning to the point; a curve shape across a ladder is much harder to game. Where an axis exists (`--noise-claims`, `--retract-fraction`, `--disjoint`, `--arity`), the suite runs its unflattering settings on purpose.

## Repeats, `first`, and `steady median`

Every repeat starts from the **same logical pre-state**: mutating scenarios rebuild their fixture per repeat; immutable fixtures are reused. Fixture construction is excluded from the timed sample. The first timed invocation reports as `first` - deliberately not "cold": the fixture insert has just warmed the buffers, so "first timed invocation after fixture construction" is the only claim the instrument can defend - and the median over the remaining repeats reports as `steady median`. The header's `requested_repeat` is a request, never a promise: expensive families (import, contend) cap their own repeats, and each table row carries its own `samples` count - captured evidence stays self-describing. The full case parameterisation travels with every table (the spike's rule: flags travel with the number). Planner statistics are refreshed (`ANALYZE`) after every fixture so the first measurement is not also paying for stale stats.

A canonical `contend` row must be **clean** (no failed, no rejected operations); a row where work stopped succeeding is not a comparable measurement, and the suite errors rather than reporting it.

## What a performance PR carries

1. **The quick whole-suite table** - proof the complements did not go sideways. Name explicitly what the change was *not* optimised for, with those numbers shown.
2. **The full ladder for every case the PR claims to improve.** The SQL spike's claims were 100x sweeps, and the sweep is what surfaced the JIT tax and the ORDER-BY planner flip; a quick ladder alone would have missed both.
3. **Baseline and candidate measured on the same host, same PostgreSQL instance and version, same suite contract, during the PR.** Historical README timings are never the baseline - the README itself records absolute numbers moving in *both* directions with the machine while the scaling laws held.
4. The command that produced each table, in the PR body - a performance claim's check is a re-runnable measurement, the same rule as every other claim here.

## What CI runs, and what it never runs

CI and precommit run the **N=1 smoke** only: every scenario once at trivial size, plus the suite plumbing over a private tiny plan and a pure renderer test. No timing assertion ever enters CI - flaky timing gates rot trust in the whole gate - and the public quick/full ladders are manual, out-of-CI runs whose tables are recorded in PR bodies and the bench README's dated observations. Where SQL is compiled (#277 and after), **plan-shape regression tests** are the CI-safe form of a performance assertion: `EXPLAIN` output is deterministic where milliseconds are not.

## Correctness outranks speed

Every optimisation passes the correctness harnesses unchanged: the same-snapshot differential (the kernel stays the executable spec), the semantic differentials, `eval_totality`, round-trip, and the walker battery. A fast path that disagrees with the slow path on generated inputs is a red gate, not a benchmark judgment call.

## External ownership

The strongest held-out benchmark is one this repository cannot see: embedders keeping replay wall-clock in their own CI over their own corpora. An optimisation that improves `morpholog-bench` but not the embedders' replay time gets flagged by someone who is not the optimiser. Ask for that number when a performance arc starts; treat its drift reports as forcing evidence.
