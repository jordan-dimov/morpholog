# Scoped charges: the record picks the figure's source

A tariff prices some charges off the meter's own reading and others
off the quantity the caller proposes - and which source applies is
the tariff's declaration, never the caller's choice. In most billing
systems that selection is an if-statement inside the engine, where a
bug can apply a proposed figure to a metered charge and nothing
refuses it. Here the selection is admission law: one line act serves
every source, the applied figure is computed by the rules from the
declared source, and a line wearing the wrong source's figure cannot
commit.

This is the worked example that forced `if(condition, a, b)` into the
kernel - the value selected by whether a proposition holds. The
forcing shape is caller-value versus record-value: one branch is a
governed lookup (`value MeterReading(meter, _)`), the other an
already-bound proposal, and no relational spelling can *produce* the
applied figure - an `or` of tests can only check a value that already
exists, which is why this used to be two near-identical acts, one per
source, multiplying with every source added. An embedder counted that
cost on a live model (seven bodies where five would do, every new
scope another pair) and their report is what forced the node.

## The programme at a glance

| Claim | Meaning |
|---|---|
| `TariffCharge(charge, source)` | The tariff's declaration: which source prices this charge. One per charge, never rewritten. |
| `MeterReading(meter, qty)` | What the meter recorded. One standing reading per meter in this snapshot; a reading does not unhappen. |
| `ChargeLine(line, charge, meter, proposed_qty, applied_qty)` | One priced line, keeping both the proposal and what the record applied. |

| Rule | What it refuses |
|---|---|
| `applied_quantity_follows_the_declared_source` | Any applied figure that is not the declared source's own: a caller figure on a metered charge, a meter figure on a caller-sourced charge, a stale or tampered figure - refused by name, in either proposal order. |
| `lines_name_a_declared_charge` | A line for a charge the tariff never declared - the totality companion that keeps the selection from passing emptily. |
| `sources_are_the_known_two` | A source tag the rules have no branch for. |
| `the_selected_source_exists` (gate) | A metered line with no reading to read - a lawful refusal, not a kernel error. A caller-sourced line commits with no reading at all: the untaken branch is never evaluated. |

Transformations: `declare_charge`, `read_meter`, `record_line` - the
one act that used to be two.

## Run it

```bash
morpholog check -v examples/18_scoped_charges/scoped_charges.morph
morpholog propose examples/18_scoped_charges/scoped_charges.morph declare_charge \
  --actor tariff_desk --args-named '{"charge":"standing","source":"caller"}'
```

The `.morph` teaches the domain from scratch - the guided tour lives
there, not here.

## Deliberately not covered

Statement-level branching (a conditional selects values, never which
statements run - two genuinely different business acts stay two
acts), reading corrections and multiple readings per meter (this is a
single-period snapshot; a production model gives each reading an
identity and lines refer to the exact reading used), charge periods,
and rates or money (the metered-billing example owns the penny).
