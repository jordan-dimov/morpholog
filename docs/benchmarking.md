# Benchmarking discipline

How performance work is measured in this repository, and the rules that keep the measurement honest - including against the AI agent that does most of the optimising. The instrument itself lives in `crates/morpholog-bench` (its README is the manual and holds the current readings); this document owns the discipline.

The one-sentence version:

> You may make Morpholog fast. You may not make the test of "fast" easier.

## The suite contract

`morpholog-bench suite` runs a **frozen matrix of named cases** across per-case ladders and prints one table (markdown for PR bodies, `--format json` for machine comparison). The matrix is versioned by `suite_contract=<N>` in the output header.

**A performance PR may add or change the implementation being measured. It may not alter the case matrix, fixtures, ladders, parameters, or aggregation semantics.** Any change to those is a change to the *ruler*, not the machine: it lands in its own commit, reviewed on its own, bumping `suite_contract` - and it lands **before** any optimisation it will judge. The contract is a tripwire, not just a promise: a DB-free test pins a fingerprint of both canonical plans, so editing `suite_plan` without bumping the contract goes red, and the ruler change becomes a conspicuous pinned-hash diff rather than one innocent parameter in a large PR. This is the bench-first rule: a new scenario and its baseline table are reviewed by someone who has not yet seen the optimisation, so the number the optimisation later moves is one nobody could retro-fit.

## The execution configuration is the machine, not the ruler

One runtime, three execution configurations, selected by `--implementation` on every scenario and on the suite: `interpreted` (the kernel interpreter, pinned whatever the programme is eligible for), `compiled` (the production compiled invariant route with no compiler-required index present), and `compiled-indexed` (the same route with every index the compiler names provisioned). The value fills the `implementation` column of every row. Selecting one changes what is measured, never how, so the suite contract does not move with it; the default is `interpreted`, so an unflagged historical command keeps its meaning, and a compiled measurement is always asked for by name.

The physical index condition is part of the configuration, and the bench establishes it itself after every logical reset and before the fixture, outside every timed sample: an unindexed configuration drops Morpholog's own compiled indexes and refuses to run if an operator's equivalent would satisfy a requirement; the indexed configuration provisions every requirement and refuses a conflict. A compiled configuration refuses a programme the production route would interpret, naming the refusals, so a row labelled compiled is one. The database must be at the migration head.

The three-way decomposition: `interpreted -> compiled` is what moving invariant evaluation into SQL bought; `compiled -> compiled-indexed` is what giving that SQL the access path the compiler asks for added; `interpreted -> compiled-indexed` is the production verdict. The `read`, `asof`, `replay` and `kernel` families are controls: `interpreted -> compiled` should leave them flat because the route is not exercised, while `compiled-indexed` additionally measures any collateral effect of the physical indexes on reads and on fixture maintenance.

## Cases are consumer-derived, with provenance

A scenario earns its place the way a worked example does: a real workload forced it, and the case carries that provenance (the suite prints the mapping above its table). The current families:

- `write`, `read`, `asof`, `contend` - the original mechanism scenarios; their *complement* cases (`/noise`, `/grouped`, `/retract`, the workers=1 baseline) are deliberate controls, not defaults. `asof/revise` is the as-of shape a consumer forced: a register revised in vintages, its live set a hundredth of its history, so an as-of read's cost is history rather than answer. The concurrency law is held by **same-workload A/Bs**: `contend/shared` vs `contend/value-partitioned` (both ledger - value sharding does not relieve SSI pressure) and `contend/predicate-shared` vs `contend/disjoint` (both synthetic - footprint partitioning does). The two scaling curves use different workloads and never compare to each other. The controls are what let a reader say **why** a number moved, and they are the first thing a narrow optimisation regresses.
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
3. **Baseline and candidate measured on the same host, same PostgreSQL instance and version, same suite contract, during the PR.** Historical README timings are never the baseline - the README itself records absolute numbers moving in *both* directions with the machine while the scaling laws held. **The runs are interleaved, in balanced order** - A B, B A, A B, B A for four a side - never all of one then all of the other. Machines warm, throttle, and change what they cache over a session; run in blocks, that drift lines up with baseline against candidate and can separate four identical runs from four others. Balancing who goes first keeps a second-run advantage off either side. Record the order beside the reports. `scripts/bench_ab.sh <baseline-rev>` does all of this: it builds the baseline from a temporary worktree and the working tree as the candidate, each into its own target directory, runs them interleaved from copies, waits for an idle machine (I/O wait counts as busy) that is cool, where the package temperature is readable, before every run, records each run's order, power source, profile, clock cap, throttled time and report hash, and ends with the `compare` table. It exits 2 when the evidence is suspect: the machine's configuration (power source, profile, governors, clock cap) changed during or between runs, the two sides throttled unevenly, or the throttle counters or the package temperature could not be read, so the session cannot be shown clean. Throttling alike on both sides is not contamination; a laptop that turbos to its temperature limit throttles on every run.
4. The command that produced each table, in the PR body - a performance claim's check is a re-runnable measurement, the same rule as every other claim here.
5. **The `compare` table** for baseline against candidate: `suite --format json` run at least four times on each side, interleaved as in item 3, then `morpholog-bench compare --before a1.json a2.json a3.json a4.json --after b1.json b2.json b3.json b4.json`. It pairs every case metric by case, axis, point, metric and unit, prints the median of each side's runs and their ratio, refuses two rulers, two ladders, one metric in two units, one run given twice, and a side whose runs measured different implementations or carry different rows, and lists any row only one side has - so a changed plan cannot pass as a changed number. Each row also says whether its runs separate: `lower` or `higher` when every run on one side lies beyond every run on the other, `within noise` when they overlap, and `too few runs` below four runs on either side. For interleaved runs of an unchanged binary, complete separation of four against four happens under 3% of the time. That is a per-row rule, not adjusted for the number of rows: across a full suite expect a handful of `lower` or `higher` by chance. A lone call is something to look at, and the claim still comes from the curve and the controls. **The run is the unit of evidence, not the repeat:** repeats inside one run share a process, a fixture and a buffer cache, and agree with each other far more closely than two runs of the same binary do. An A/A pair judged on its repeats called about one row in ten changed (see the bench README's observations). A ratio on a `within noise` or `too few runs` row is not a result. Type nothing by hand.

## What CI runs, and what it never runs

CI and precommit run the **N=1 smoke** only: every scenario once at trivial size, plus the suite plumbing over a private tiny plan and a pure renderer test. No timing assertion ever enters CI - flaky timing gates rot trust in the whole gate - and the public quick/full ladders are manual, out-of-CI runs whose tables are recorded in PR bodies and the bench README's dated observations. Where SQL is compiled, **plan-shape regression tests** are the CI-safe form of a performance assertion: `EXPLAIN` output is deterministic where milliseconds are not.

## Correctness outranks speed

Every optimisation passes the correctness harnesses unchanged: the same-snapshot differential (the kernel stays the executable spec), the semantic differentials, `eval_totality`, round-trip, and the walker battery. A fast path that disagrees with the slow path on generated inputs is a red gate, not a benchmark judgment call.

## External ownership

The strongest held-out benchmark is one this repository cannot see: embedders keeping replay wall-clock in their own CI over their own corpora. An optimisation that improves `morpholog-bench` but not the embedders' replay time gets flagged by someone who is not the optimiser. Ask for that number when a performance arc starts; treat its drift reports as forcing evidence.

## Measurements on record

### Resident session against one-shot proposals (2026-10-11)

Forced by Gate Closure (#421): a web site proposing a few hundred entries in the minute before a daily close, one subprocess per proposal. The question was what a resident `morpholog session` buys over that.

The claim: **on one machine, a resident session takes about half the time per proposal of a one-shot `morpholog propose`**, the same ratio in three runs. That is a protocol-level result, measured with `scripts/embedder_latency.sh` in Bash. It is not a measurement of the generated Python client inside a web framework, and the parts of each call (spawn, parse, connect, the commit and the session exchange) were not timed separately.

| | Run 1 | Run 2 | Run 3 |
|---|---|---|---|
| `check` (spawn, parse, validate), ms per call | 4.05 | 3.96 | 3.91 |
| one-shot `propose`, ms per call | 21.35 | 21.70 | 20.63 |
| session startup to ready, ms once | 7.02 | 10.35 | 10.78 |
| session first request, ms | 8.88 | 8.12 | 9.58 |
| session steady-state median, ms | 10.97 | 10.91 | 10.07 |
| session steady-state p95, ms | 18.04 | 18.65 | 17.41 |
| speed-up, mean against mean | 2.0x | 2.0x | 2.0x |
| session steady-state mean implied, ms | 10.68 | 10.85 | 10.31 |
| one-minute load at start | 1.47 | 1.36 | 1.31 |

The script prints the speed-up rounded to one decimal and does not print the session's mean, so the implied mean is the one-shot time divided by 2.0: with the rounding, each lies within about 2.5% of the printed value. The startup figure starts its clock after the shell has launched the session process, so it leaves out part of the launch; the speed-up rests on the proposal timings alone.

Invocation: `DATABASE_URL=<disposable database> ./scripts/embedder_latency.sh 300`, three runs a few seconds apart, each started once the one-minute load average was under 1.5 (readings above). Release binary at commit `cbb7f36`, sha256 `bf437a7e5c705d6f826a033f3d2d4437192a4e906d10f98cf2c2558ec3489cc2`; PostgreSQL 18.6 on the same host, an Intel Core Ultra 7 155H laptop on mains power, no thermal events in the kernel log. The workload is the ledger example's `post_simple_entry`, one worker, proposals in sequence.

Not measured: several workers proposing at once, and the time inside a web request. Whether a burst in the last seconds before a close completes in time, and how often concurrent workers meet serialization failures, depend on the deploying programme's own rules and state; measure them against that programme when they matter.

### Resident explain against one-shot explain (2026-10-17)

Forced by the representation-search experiment: a loop that asks the record many times whether its evidence permits a conclusion, with nothing recorded, and commits through the session when it does. The question was what `{"op": "explain"}` on the session buys over a one-shot `morpholog explain` per question, and the parts the one-shot pays are reported apart: the process cost (`check`: spawn, parse, validate), the evaluation over a warm connection (the session's steady state), and the throughput one sequential worker gets.

The claim: **on one machine, a resident explain takes well under half the time of a one-shot `morpholog explain`**, the same ratio in three runs, and a one-shot explain is cheaper than a one-shot propose by about the commit. Protocol-level, in Bash, one worker in sequence; the generated client inside a loop was not timed.

| | Run 1 | Run 2 | Run 3 |
|---|---|---|---|
| `check` (spawn, parse, validate), ms per call | 3.83 | 4.05 | 3.94 |
| one-shot `propose`, ms per call | 20.38 | 21.59 | 21.54 |
| one-shot `explain`, ms per call | 15.87 | 14.77 | 15.57 |
| session startup to ready, ms once | 8.28 | 7.33 | 7.77 |
| session first request, ms | 8.56 | 9.00 | 8.46 |
| session `propose` steady-state median, ms | 10.94 | 10.28 | 10.35 |
| session `explain` steady-state median, ms | 5.65 | 5.19 | 6.01 |
| session `explain` steady-state p95, ms | 6.63 | 6.51 | 7.22 |
| session `explain` throughput, requests per second (mean) | 179 | 192 | 173 |
| speed-up over one-shot `explain`, mean against mean | 2.8x | 2.8x | 2.7x |
| one-minute load at start | 6.54 | 4.89 | 4.00 |

Read it as three parts. Process startup and programme preparation: about 4 ms per one-shot call, paid once per session (startup to ready, under 9 ms). Evaluation over a warm connection: 5 to 6 ms per explain, which is one read of the rules' predicates, the kernel in memory and the JSON line. Throughput: one worker asking in sequence gets about 180 explains a second through the session against about 65 one-shot.

Invocation: `DATABASE_URL=<disposable database> ./scripts/embedder_latency.sh 300`, three runs a few seconds apart. The one-minute load was above the 1.5 the earlier table waited for, because the release build had just finished; the `check` and `propose` rows reproduce that table within a few percent, so the runs were kept. Release binary sha256 `5ec428a75b54ad87dac0e29b6dd772fbf28d9ffc60d51d0be9a2d904f750f702`, built from the working tree of the pull request that added the explain operation (commit `a08f9f8` plus that change); PostgreSQL 18.6 on the same host, an Intel Core Ultra 7 155H laptop on mains power, `powersave` governor, no thermal events in the kernel log. The workload is the ledger example's `post_simple_entry`; each session explain ran after the session's own 300 proposals, so against a ledger of 300 entries.

Not measured: a larger ledger. An explain runs the interpreter over a whole read of the predicates the rules name, so its cost grows with those predicates' size where a compiled `propose` stays flat; measure it against the deploying programme's own state when the loop's state is large.
