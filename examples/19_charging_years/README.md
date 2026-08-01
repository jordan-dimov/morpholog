# Charging years: a period may not straddle the anniversary

Distribution charging years run 1 April to 31 March, and a billing
run resolves a supply's band rates for exactly one of them - so a
period crossing 1 April must not be billable as one run. In most
billing systems that refusal is prevalidation in application code,
checked before the record is asked; a bug or a skipped check puts a
straddling run on the books with no rule against it. Here the
boundary is admission law: a date's charging year is a computed
coordinate, the gate demands both ends of the period share it, the
run records the coordinate the rules computed (the caller cannot
state it), and invariants re-check everything over every committed
run.

This is the worked example that forced `period_index(anchor, span,
at)` into the kernel: which anniversary-anchored period a date falls
in, as an integer-valued decimal. With shifts and day counts alone,
"both ends fall in the same 1-April-anchored year" was not
expressible - day counts do not identify the boundary crossing
without division. The extractor subsumes the alternatives the
forcing embedder offered: same-period is index equality, annual
indexing is `span(P1Y)`, and same-calendar-month is a monthly
anchor.

## The programme at a glance

| Claim | Meaning |
|---|---|
| `BillingRun(run, starts_on, ends_on, charging_year)` | One run: its period (inclusive at both ends - a full charging year is 1 April through 31 March) and the charging year the rules computed for it, as the year's own name (2026, not an epoch offset). Append-only, one record per run. |

| Rule | What it refuses |
|---|---|
| `the_period_stays_inside_one_charging_year` (gate) and `runs_stay_inside_one_charging_year` | A period straddling 1 April - refused at the act, and again by the invariant against any other act. |
| `runs_record_their_own_charging_year` | A recorded year that is not the record's own computation - the caller never chooses the figure. |
| `runs_run_forwards` | A period whose end precedes its start. |

Transformations: `open_run` - the charging year is not a parameter.

## Run it

```bash
morpholog check -v examples/19_charging_years/charging_years.morph
morpholog propose examples/19_charging_years/charging_years.morph open_run \
  --actor billing_engine --args-named '{"run":"r1","starts_on":"2026-04-10","ends_on":"2026-07-09"}'
```

The `.morph` teaches the domain from scratch - the guided tour lives
there, not here.

## Deliberately not covered

Band rates and money (the metered-billing and scoped-charges
examples own pricing shapes), splitting a straddling period into two
runs (an embedder decision about act granularity, not a record
rule), and period conventions beyond membership - month-boundary
alignment and the like use the same extractor with first-of-month
anchors when a rule needs them (an anchor on the 15th makes
15th-to-14th months, which is sometimes exactly what a contract
says).
