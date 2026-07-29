# Spike verdict: compiling invariant checking to SQL

**Question:** does a compiled relational core make commit latency flat in in-scope
state and un-serialise value-partitioned writers, without disagreeing with the kernel?

Status: IN PROGRESS on branch `spike/sql-invariants`. This branch never merges; this
document's final form lands on main as a design-history entry plus a roadmap edit.

## Kill-signal probe (step 2) - PASSED

N=10k entries (30k claims), hand-written residual for `balanced_posted_entry`
(case-bound to one entry_id), partial expression indexes
`(arguments -> 0 ->> 'value') WHERE predicate_name = '<P>'` on JournalEntry and
JournalLine. Probe: `scripts` in session scratchpad; EXPLAIN (ANALYZE, BUFFERS) plus a
two-session pg_locks inspection under an open SERIALIZABLE transaction.

With indexes:

| metric | value |
|---|---|
| execution time | 0.198 ms (16 buffers) |
| plan | Index Scan (spike_je_a0) + 2x Bitmap Index Scan (spike_jl_a0); zero Seq Scan |
| SIRead locks | page x1 (spike_je_a0), page x1 (spike_jl_a0), tuple x3 (claims) - NO relation lock |

Without indexes (control):

| metric | value |
|---|---|
| execution time | 176 ms (4369 buffers) |
| plan | Index Only Scan over the whole predicate prefix + Seq Scan subplans |
| SIRead locks | relation (morpholog.claims), relation (claims_pkey) - full-table footprint |

Reading: the compiled WHERE clause plus something to seek on gives a fine-grained SSI
footprint and sub-ms residual checking; without the index the footprint collapses to
relation-level and the contention win cannot exist. Proceed to the compiler.

## Fragment & coverage census

(step 6 - pending)

## Correctness (differential harness)

(steps 5, 7, 10 - pending)

## Measurements

(step 12 - pending: Table W, Table C, noise row, pg_locks, EXPLAIN captures,
machine/PG config appendix)

## Criteria

Pre-registered in the plan: (a) stage2+idx flat-vs-N within 2x at 100k; (b) contend
periods=1 retry_rate <= ~2x disjoint floor, throughput >= 5x, no-idx control flat;
(c) zero differential disagreements, divergence only in the pinned direction;
(d) 100% bench+03 compile coverage; (e) residual-risk list written.

## Verdict

(pending)
