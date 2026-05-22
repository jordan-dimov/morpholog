# Morpholog: Roadmap

Status: operational. This document is what the project is *doing next*. It is forward-looking and lower-stakes than [`scope-and-ambition.md`](scope-and-ambition.md) (which fixes what Morpholog is *for*) and [`runtime-semantics.md`](runtime-semantics.md) (which fixes what the runtime *means*). When this file and design-history disagree, design-history is the historical record; this file is intent.

Each item below either has a concrete forcing scenario or is explicitly held until one appears. The "smallest possible increment, forced by a worked example" discipline still governs - this is not a wish list.

## Status today

The kernel, PG adapter, CLI, polling outbox worker, and the worked examples are in good shape. Programmes are declared-vocabulary objects. Execution is structurally inspectable through `propose_with_trace` and the CLI's `--trace` flag, including expression-internal failure-walk that identifies which sub-expression of a rejected `require` or `bind_one` was responsible. Predicate-scoped loading runs on both read and write paths. The kernel is split into focused submodules; no module is over ~750 lines.

The parser arc is mid-stream. A `.morph` source file can carry: the `program` header, `predicate` declarations, full expressions (atoms, arithmetic, comparators, boolean composition, `exists`/`forall`/`sum`/`value`/`in`, date and subject literals), and `invariant` declarations. `morpholog parse <file>` parses these end-to-end. What remains to land: transformations + statement syntax (P3b), derived claims (P3c), and date-comparison surface form (lands when an invariant body needs it). Worked examples are still constructed via the Rust `dsl` module for their transformation bodies.

## Imminent: surface syntax

The parser arc has started. Programmes are Rust IR via the public `dsl` module today; the natural reader of a Morpholog programme is a domain expert, not a Rust developer. The parser is what makes the programme legible to that audience, and it commits the surface syntax decisions that nothing else has forced yet.

The arc is multi-PR; each PR is one focused production. The crate is `morpholog-surface` (broader than "parser" so a future formatter / source-mapper / LSP shares the same crate). Tooling: `chumsky` for parsing, `ariadne` for diagnostics. File extension: `.morph`.

- **PR P1 (landed): predicate declarations only.** The `program <name>` header plus zero or more `predicate Name(arg: Kind, ...)` declarations. `morpholog parse <file>` CLI subcommand, ariadne-rendered diagnostics, parser-side duplicate detection. Commits the file-level surface decisions (identifier syntax, kind keywords, comments, trailing commas).
- **PR P2a (landed): expressions, no bounded forms.** Atoms (vars, decimal literals, wildcards, `actor`, predicate calls), arithmetic (`+`, `-`), comparators (`=`, `!=`, `<=`), boolean composition (`not`, `and`, `implies`) with standard precedence. The expression parser exposed as `parse_expression`. Honours the IR/surface doctrine in `docs/scope-and-ambition.md` (no bool literals, no escape hatches, term-only restriction on `Neq` and claim-call args). See `docs/design-history.md` entry "Parser P2a" for the per-operator decisions.
- **PR P2b-lite (landed): bounded forms + literals.** `exists x: body`, `forall x in source: body` (with auto-lift when source is a bare Term), `sum(target | body)` with target restricted to a variable, `value Pred(args) [default expr]` in claim-pattern shape (NOT a general query form - see the doctrine), `x in xs` membership comparator at comparator precedence, `@YYYY-MM-DD` date literals, `#NAME` subject literals. Date *comparison* (the surface form for `Expr::DateLe`) is deliberately deferred alongside the worked example that drives it.
- **PR P3a (landed): invariant declarations.** Smallest programme-integration step: `invariant <name>: <expression>`. Invariant bodies are pure expressions. No version syntax in v0 (`version` defaults to 1). Free interleaving of predicate and invariant declarations. Parser-side duplicate-invariant detection. Parser source split into `parser/{program,expr}.rs` ahead of P3b. Indentation block-syntax doctrine locked.
- **PR P3b1 (landing): transformations + gate statements + layout pipeline.** New `layout.rs` module emits virtual `Indent`/`Dedent` tokens at block boundaries (parens disable layout; tabs in indentation are a diagnostic). Transformation header (`transformation name(params):`) plus `require`, `bind`, `let name = expr`, `let name = new Subject()`. The P3b2 keywords (`admit`, `retract`, `emit`, `for`) are reserved at the lexer but not yet parseable. Quantifier bodies and invariant bodies accept optional `Indent body Dedent` wrapping.
- **PR P3b2 (landing): state-mutating statements + iteration.** `admit`, `retract`, `emit`, plus `for x in coll: <indented body>` (introduces nested layout). Reuses the P3b1 `claim_pattern` helper across all four verbs. After this, the worked examples that don't declare derived claims parse end-to-end via `morpholog parse`; examples with derived claims stop at the `derived` keyword (P3c). Date-comparison surface form deliberately deferred to its own focused PR.
- **PR P3-dates (landing): civil-date comparison surface.** `on_or_before` infix keyword lowering to `Expr::DateLe`. Distinct surface form from decimal `<=` because the kernel keeps the two comparators as separate IR primitives. Clinical-trial-enrolment example updated; new `consent_obtained_before_randomisation` invariant exercises the comparator inside an invariant body.
- **PR P3c: derived claims.** `DerivedClaim` shape with keys, values, and domain. Round-trip property tests (`format_program -> parse_program`) land here when the full surface is parseable. After P3c, all worked examples parse end-to-end.
- **`morpholog run <file.morph>`.** Once the full surface is parsable.

## After the parser: legibility tooling

Surface syntax makes programmes readable; the next gap is making them *reviewable*. Three `morpholog inspect` subcommands derived from static analysis of parsed `.morph` programmes:

1. **`morpholog inspect exclusions <program>`** - walks invariants and `not` / `Neq` requires across the programme; emits the mutually-exclusive predicate pairs it can derive. The audience-first design here is that controllers and regulators ask "what does this system *prevent*?" before "what does it enable?", and nothing in `inspect` today answers that. Highest-leverage first tool; smallest implementation.
2. **`morpholog inspect transformation <name> --graph`** - renders each transformation body as a pre/post dependency DAG: bind-one and require gates on one side, asserts and emits on the other. Mechanically derived from the IR.
3. **Subject-flow profiles** - walks predicate declarations to surface clusters where the same `Subject`-kind position recurs (the cluster of predicates that all carry a `participant_id`, for example). Static analysis output, not a runtime type system; presents the cluster as a behavioural profile, not a class.

All three live above the parser and are statically derivable; none add new IR primitives or runtime concepts.

## Performance, when forced

The current bench shows linear scaling on additive workloads (~1.6s per commit, ~1.5s for 100K-transition replay). The bench fixture is purely additive, so these numbers are best-case.

- **Retraction-heavy bench scenario.** Add a `--retract-fraction K` flag mirroring the existing `--noise-claims`. Generates a fixture where K% of the N transitions are wildcard retracts against prior periods. The forcing pressure that would justify snapshot/lattice work is the curve this produces - not raw N. Worth doing the *measurement* before designing the lattice, so the snapshot interval and structure are sized to a real signal.
- **Snapshot / incremental-materialisation for audit replay.** Once the bench surfaces a real problem. Likely shape: a checkpoint table that materialises the live claim set every K transitions, plus a delta-replay path that resumes from the nearest checkpoint. Compounded by concurrent bitemporal query density (matrix-style reports running `list_derived_at` across thousands of historical timestamps).

## Operational completeness, when forced

These are deferred until a worked example or a real operator forces them.

- **Worker supervisor.** The polling outbox worker exists and ships a `StdoutDeliverer`. Missing: a supervisor running multiple workers under restart-with-intensity, with crash isolation between deliverers.
- **Per-target circuit breakers.** A delivery target that's misbehaving should be ring-fenced (back off, alert, eventually quarantine), not continue eating the worker's loop indefinitely.
- **HTTP-aware deliverer.** Once a worked example actually sends an outbox intent over the wire, an `HttpDeliverer` with the right retry/idempotency semantics.

## Language affordances awaiting a worked example

Each lands when an example actually demands the shape. None are pre-decided.

- **Higher-order authority / predicate-pattern matching.** *One* authority claim governing a *family* of transformations, instead of one per kind. The declared predicate vocabulary now provides a metadata home; the shape itself still needs a worked example.
- **Effective time as a separate axis.** As-of gives knowledge time. Effective time - the day a contract becomes binding, the period a posting reflects - is expressible as ordinary claims; combining the two gives bitemporal addressability without any `valid_from`/`valid_to` schema columns.
- **Validity windows + repair transitions + exception claims.** First-class typed claims that mark an admitted assertion as in-repair, contested, or quarantined, with audit standing. No bypass flags ever.
- **Materialised derived claims.** Reports are recomputed on demand today. For long audit logs and frequent queries, materialised snapshots become forced - with invalidation discipline modelled as ordinary claims, not as cache machinery.
- **Migrations framework.** A small one. The schema is hand-written today; with multiple deployments and an evolving claim vocabulary, a migrations story will be forced. The shape should be claim-shaped (migration as transformation, schema version as admitted claim).
- **Strict decimal comparisons (`Lt`, `Gt`, `Ge`), date arithmetic, civil intervals.** The current single-comparator-per-kind (`Le` for decimal, `DateLe` for civil dates) is a deliberate floor. The third comparator is what would force the generic dispatch shape; until then, one per kind.

## Deliberately out of scope (revisit only with explicit reason)

These are floors, not preferences. The reasons live in [`scope-and-ambition.md`](scope-and-ambition.md)'s "Non-goals" section. Restating here for completeness:

- No entities, classes, services, or ORM in the surface language. Subjects are opaque; predicates attach to subjects; that is the entire object model.
- No general workflow engine. Lifecycle is conjunctions of admitted claims, eventually derived claims. Morpholog is not Camunda and must not grow toward it.
- No arbitrary computation inside transformations. Pure expressions over admitted claims, plus assertions, retractions, intents.
- No BI / analytics / reporting engine. Derived claims govern reproducible read-side outputs; everything else lives outside.
- No optimisation / solver runtime. ETRM scheduling, AP payment runs, dispatch - outside. Morpholog governs the inputs and admits the outputs.
- No ad-hoc query DSL beyond derived-claim queries and the as-of operator.
- **No bypass flags ever** (`skip_validation`, `force_commit`). Exceptions are first-class typed claims with full audit standing.
- No tree-sitter or LSP grammar pre-built before the parser ships. The parser is the forcing pressure for those.
- No self-hosting. Morpholog governs business state; the compiler is Rust.

## How to read this file

If something appears on the "awaiting a worked example" list, that's a *deliberate* hold, not a TODO. The project's central discipline is that kernel primitives arrive alongside the example that forces them; reading the list as a backlog defeats the discipline.

If a section moves from "imminent" to "landed" or from "awaiting" to "imminent", that change goes through the same review/PR loop as code changes. The roadmap moves; the doctrine in [`scope-and-ambition.md`](scope-and-ambition.md) does not.
