# Morpholog

An experimental runtime for business systems where the rules that decide whether a piece of state is legitimate are enforced by the language itself, not by code somebody remembered to write.

## The problem it targets

If you have spent any time near finance, trading, accounting, or regulated lending, you will recognise a small family of questions that conventional software answers badly:

- Why doesn't the trial balance match the sub-ledger any more, and when did the drift start?
- What did we actually believe at 5pm last Tuesday, under the rules in force then?
- The covenant test said we were compliant in Q2. Was that based on the revenue figure we had then, or on the figure that has since been restated?
- Who authorised this posting? The workflow log says it was approved, but the approval limit was raised three days later.
- Can we reproduce the regulatory return we filed last quarter, line by line, from the inputs available at the time?
- The position report disagrees with the risk system, which disagrees with the back office. Whose number is right?

These are not seven different problems. They are one problem in seven disguises: **state was treated as legitimate that the system cannot, under explicit rules, justify treating as legitimate.**

Conventional systems leak on this question because the legitimacy boundary is scattered across thousands of lines of validation code, ORM hooks, stored procedures, end-of-day reconciliation scripts, and tribal knowledge in the heads of senior controllers, traders, and risk officers. Morpholog's wager is that this boundary should be **the language itself** — and that once it is, the entire family of failures above becomes much harder to produce.

## The shift

Morpholog asks you to write only two things:

- **Invariants** — rules that must always hold over admitted state. "The debits and credits of a posted journal entry must balance." "A bilateral net settlement must equal the signed sum of its approved underlying lines." "Every confirmed trade must have both counterparty confirmations admitted."
- **Transformations** — the only path by which admitted state may change. A transformation proposes a set of additions, removals, and outbound intents. The runtime evaluates every active invariant against the resulting candidate state. If any invariant fails, the transformation is rejected atomically — nothing is written, no outbound message is sent.

State itself is not a soup of mutable rows. It is a set of **claims**: typed assertions admitted into governed state under a specific authority, at a specific moment, by a specific transformation.

The wording matters. A claim is not "the truth" — it is an *admitted assertion*, with a recorded provenance. The optimiser may claim a battery earned £100k of revenue this month, the independent verifier may claim £91.7k, the bank may recognise £92k for debt-service coverage, the owner may expect £110k. Conventional systems collapse these into a single mutable status (often a lie). Morpholog preserves them all and lets invariants decide which claims may be used for which purposes.

This single shift — *invariants decide; transformations are the only mover; state is a set of admitted claims with provenance, not a snapshot of mutable truth* — turns out to make the legitimacy-bearing versions of the failures above much harder to produce, and in well-modelled cases structurally impossible. It does not, of course, prevent every kind of failure a business system can have: bad inputs, faulty integrations, human misuse, and missing rules will still bite. What it removes is the specific class of failure in which *admitted state itself is illegitimate under the rules the system claims to enforce*.

## What this looks like in three domains

### Energy trading and settlement

Two counterparties have dozens of bilateral settlement lines between them. At month-end the back office produces a single net settlement amount and instructs payment.

In a conventional system, the netting amount and the underlying lines can drift apart: someone edits an approved line after the netting was computed; the same line is included in two different nets by accident; an unapproved line slips in. The discrepancy usually surfaces when the counterparty's settlement team queries the figure, and an apologetic reconciliation begins.

In Morpholog, *the netting amount equals the signed sum of its approved lines* is an invariant. The transformation that creates a net settlement stages the assertion of the net, the line-to-net links, and the per-line "netted" flags. If any line is already netted, or the computed amount does not match the admitted line amounts, the transformation is rejected — no settlement row is created, no instruction is sent. The audit row records exactly which invariant version checked which lines, so a regulator or counterparty asking "how do you know this number was correct" gets a structured answer rather than a forensic exercise.

The same pattern extends to position limits ("total open exposure to counterparty C must not exceed the credit limit in force at admission time"), REMIT-style reporting obligations, and any rule of the form "this artefact is admissible only if these other claims hold."

### Accounting and period close

Every accounting system has a fundamental invariant: *the debits and credits of a posted journal entry must balance*. Every accounting system also has a second, harder invariant: *once a period is closed, no posting may add to it — except via a controlled restatement that preserves the history of what was filed before.*

The first invariant is usually enforced. The second is usually a polite fiction. Real systems close a period with a flag, then quietly accept "adjusting" entries that bypass the close because finance needs to fix something the auditors found. The original numbers get overwritten; the prior-quarter trial balance silently changes; an external auditor asking "what did we report at the time, and what did we restate to, and why" can only be answered by archaeology.

In Morpholog, period close is a transformation that admits a `PeriodClosed` claim. Ordinary posting transformations require the absence of that claim and are refused after close. Restatement is a separate, governed transformation that admits a new claim about the closed period and records a `Supersedes` link to the prior one. **The original posting is never deleted or edited.** The historical trial balance still resolves correctly. The restatement is fully audited. The two questions an external auditor cares about — *what was filed?* and *what was restated, by whom, when, and why?* — have definite answers that the system cannot misplace.

This is not theoretical. The second canonical worked example in this repository (`examples/02_revenue_restatement/`) proves exactly this pattern, durably, against PostgreSQL.

### Banking and regulated lending

A bank lends against a battery storage asset. The loan covenant requires the debt-service coverage ratio to exceed 1.2 each quarter. The bank computes the ratio from the revenue figure it has independently verified at quarter-end.

In a conventional system, the revenue figure is a row in a database. It changes. It may be restated three months later when an audit finds a meter reading was wrong, or when the optimiser corrects a settlement adjustment. Was the covenant met *as of the quarter-end test*, based on the figure the bank had at the time? Or is it currently in breach, based on the restated figure? Most systems cannot answer this question without a paper trail kept outside the database.

In Morpholog, the bank's recognised revenue is a claim with a transition id. The covenant test result is another claim that cites the specific revenue claim it relied on. When the verifier later admits a corrected revenue claim — and records a `Supersedes` link to the prior one — the historical claim is *not deleted*. The covenant test still cites the figure it actually used. Asking "was the covenant met when we tested it, and what did we believe at the time?" returns a precise answer from the audit log. Separately, the system can compute the answer *under the restated figure* by re-evaluating against the current state. Two different questions, two different answers, neither one a guess.

The same pattern covers ongoing covenant monitoring, regulatory capital reporting, IFRS-9 staging decisions, and any setting where "what did we know, when, and what did we rely on for which decision" is a question that can be put to you by a regulator or a court.

## What the language looks like

Surface syntax is not final. The example below illustrates the *shape* of an invariant and a transformation. The current implementation reads programs as Rust data structures; a parser for the surface syntax is deliberately deferred until the kernel has been pushed harder.

```
invariant net_amount_equals_lines:
    NetSettlement(net, _, _, amount) implies
        amount == sum { x | SettlementLine(line, net, x) }

transformation create_net_settlement(party_a, party_b, lines):
    require forall { line | line in lines }:
        ApprovedSettlementLine(line) and not Netted(line)
    let net = new Subject()
    let amount = sum { x | line in lines and LineAmount(line, x) }
    assert NetSettlement(net, party_a, party_b, amount)
    for line in lines:
        assert SettlementLine(line, net, LineAmount(line))
        assert Netted(line)
    emit NetSettlementCreated(net)
```

Note what is and is not in the language: there are no classes, no entities, no services, no ORM, no workflow engine. There are predicates, claims, invariants, transformations, and outbound intents. *Whatever you want to make legitimate, you name as a predicate and admit as a claim. Whatever rules must hold, you write as an invariant. Everything else lives outside.* That is the entire surface area.

## Where Morpholog ends and the rest of your system begins

Morpholog is deliberately not trying to be the language you write a whole business system in. UI, dashboards, market data ingestion, OCR, ML categorisation, pricing engines, valuation models, optimisation solvers, scheduling, search, BI — all of this lives outside Morpholog and uses normal tools. The runtime governs *commit legitimacy*: the question of whether a proposed change to admitted state may be admitted, and what the audit trail says about it afterwards.

Measured in lines of code, Morpholog will always be a minority of a real business system. Measured in **failure modes prevented**, it can be most of what matters. The deeper framing of this — the scope boundary, the ambition ceiling, and how concerns like read-side reporting, lifecycle, provenance, authority, and temporal qualification all collapse to "more claims" rather than "more subsystems" — lives in [`docs/scope-and-ambition.md`](docs/scope-and-ambition.md).

---

## Project status

Early but not a toy. A synchronous semantic kernel and a working PostgreSQL persistence adapter ship today. The two canonical examples — settlement netting and revenue restatement — are proven both in-memory and durably against PostgreSQL. There is no parser, no usable CLI beyond `--version`, and no outbox worker. Those are deliberately deferred; the next semantic frontier (claim *standing* — admissibility-for-purpose) comes before more plumbing.

```bash
cargo test -p morpholog-core --all-targets                              # 26 tests, in-memory
DATABASE_URL=postgres:///morpholog_dev \
  cargo test -p morpholog-postgres --all-targets -- --test-threads=1   # 10 tests, durable
```

Crates:

- **`morpholog-core`** — synchronous, pure semantic kernel. IR types (Invariant, Transformation, Claim, etc.), the evaluator, and `propose()`, which builds a candidate state, runs every active invariant against it, and returns `Accepted` or `Rejected`. No I/O.
- **`morpholog-postgres`** — async adapter. `propose_against_pg()` opens one PostgreSQL transaction at `SERIALIZABLE` isolation, loads the relevant claims, calls the sync kernel, and either rolls back atomically (`Rejected`) or commits claim mutations, the audit row, and outbox intents in one transaction (`Committed`).
- **`morpholog-cli`** — version-printer skeleton. Subcommands wait on surface syntax.

Canonical schema: [`crates/morpholog-core/sql/schema.sql`](crates/morpholog-core/sql/schema.sql) — three tables (`claims`, `audit`, `outbox`).

## Deeper reading

- [`docs/scope-and-ambition.md`](docs/scope-and-ambition.md) — what Morpholog is for, what it should grow into, and what it must never become. The unifying thesis and the four language affordances on the roadmap. Start here if you want the full design framing.
- [`docs/runtime-semantics.md`](docs/runtime-semantics.md) — design doctrine for the IR and runtime kernel. The semantics that `morpholog-core` realises.
- [`docs/postgres-persistence-v0.md`](docs/postgres-persistence-v0.md) — the design pin written before the PostgreSQL adapter shipped, preserved as the historical design record.
- Worked examples: [`examples/01_settlement_netting/`](examples/01_settlement_netting/) and [`examples/02_revenue_restatement/`](examples/02_revenue_restatement/).

## Requirements

- Rust 1.95+ (install via [rustup](https://rustup.rs)).
- PostgreSQL 17+. Morpholog v0 targets PostgreSQL only and deliberately uses PostgreSQL-specific features (SSI for `SERIALIZABLE`, JSONB with CHECK constraints, JSONB path functions) without portability apologies. Database portability is not a goal at this stage.

## Design tenets

- Surface language has only invariants and transformations. No entities, classes, or services.
- State is a set of *admitted claims* over opaque subject identifiers — not objective facts. A claim is a statement admitted into governed state under a specific authority, epoch, and transformation.
- Reads inside a transformation see pre-transformation state. Writes are staged and become real only at commit.
- Decimal arithmetic for business values. No floats.
- External side effects happen post-commit, at-least-once, with deterministic idempotency keys.
- Whatever you want to make legitimate, name it as a predicate and admit it as a claim. Whatever rules must hold, write as an invariant. Everything else lives outside. ([scope and ambition](docs/scope-and-ambition.md))

## License

MIT OR Apache-2.0.
