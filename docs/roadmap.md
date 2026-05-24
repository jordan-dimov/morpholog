# Morpholog: Roadmap

Status: operational. This document is what the project is *doing next*. It is forward-looking and lower-stakes than [`scope-and-ambition.md`](scope-and-ambition.md) (which fixes what Morpholog is *for*) and [`runtime-semantics.md`](runtime-semantics.md) (which fixes what the runtime *means*). When this file and design-history disagree, design-history is the historical record; this file is intent.

The "forced by a worked example" discipline binds **kernel and IR primitives**: a new `Expr` / `Stmt` / `Value` variant earns its place only when an example needs it, never speculatively. That is where unconstrained growth is dangerous, and the bar stays high. It is *not* a brake on everything else - tooling, new worked examples, performance work, legibility surfaces, and product completeness move as soon as the direction is clear, not only when an example forces them. The bar for *adding to the language* is deliberately higher than the bar for *building around it*. This is intent, not a wish list.

## Status today

The kernel, PG adapter, CLI, polling outbox worker, and the worked examples are all in good shape. Programmes are declared-vocabulary objects. Execution is structurally inspectable through `propose_with_trace` and the CLI's `--trace` flag, including expression-internal failure-walk that identifies which sub-expression of a rejected `require` or `bind` was responsible. Predicate-scoped loading runs on both read and write paths.

The `.morph` parser arc is closed. Every worked example parses end-to-end from `.morph` source; a round-trip property test couples the formatter and parser. The CLI exposes `parse` (source -> IR JSON), `check` (parse + `Program::validate()` with uniform diagnostics, silent on clean), and `run` (parse + validate + propose a transformation against PostgreSQL, the non-built-in counterpart of `propose`). `check` runs one traversal that does the structural pass (arity, declarations, duplicates for both predicates and intents), the kind/type compatibility pass, unbound-variable and predicate/value position checks, and the actor-in-wrong-context check - catching authoring mistakes the kernel would otherwise raise as `TypeMismatch`, `UnboundVariable`, `NotPredicate`, or `UnboundActor` at runtime.

Outbox intents are declared vocabulary too. `IntentDecl` (parallel to `PredicateDecl`) names every intent type the programme may emit; a misspelled `emit` is a validation error rather than a silent route-to-nowhere on the outbox.

The compute-zone interface for non-Rust integrations is in place. `morpholog run` closes the input boundary; `morpholog outbox claim` / `complete` / `release` let a shell or Python deliverer participate in the lease protocol without a Rust `Deliverer` impl. The three-zone doctrine (compute / commit / outbox) is documented in [`scope-and-ambition.md`](scope-and-ambition.md); the round-trip compute pattern in [`outbox-sketch.md`](outbox-sketch.md).

The CLI is split by subcommand under `crates/morpholog-cli/src/commands/`. Adding a new subcommand is "add a file there, add a `Command` variant, add one dispatch arm in `main`."

## Imminent

With the input/output boundaries, the kind/type-compatibility layer, and the intent vocabulary in place, `.morph` authoring is materially more trustworthy than before. Most of the enriched check has landed; one lint-grade layer remains.

- **Enriched `morpholog check`.** The structural check (arity, declarations, duplicates for both predicates and intents), kind/type compatibility across both vocabularies, unbound-variable detection (the binding flow follows the runtime: parameters -> `bind` -> `let` -> `for`, and `require` does not export its matches), the symmetric predicate-shaped/value-shaped position checks, and actor-in-wrong-context all ship today - one traversal does the structural, kind, and binding-flow work together. What remains:
    - **Lint-grade hints under `--strict`.** Unused predicate / intent declarations, `sum(x | body)` where `x` doesn't appear in `body`, unused transformation parameters, fuzzy "did you mean `MayApprove`?" suggestions on an `Undeclared` reference. `--strict` promotes hints to errors; `--json` emits diagnostics in a tooling-friendly form for IDE integration later.

- **Legibility tooling.** Surface syntax makes programmes readable; the next gap is making them *reviewable*. Three `morpholog inspect` subcommands derived from static analysis of parsed `.morph` programmes:
    1. `morpholog inspect exclusions <program>` - walks invariants and `not` / `Neq` requires; emits the mutually-exclusive predicate pairs it can derive. Controllers and regulators ask "what does this system *prevent*?" before "what does it enable?", and nothing in `inspect` today answers that. Highest-leverage first tool; smallest implementation.
    2. `morpholog inspect transformation <name> --graph` - renders each transformation body as a pre/post dependency DAG: bind / require gates on one side, admits / emits on the other. Mechanically derived from the IR.
    3. **Subject-flow profiles** - walks predicate declarations to surface clusters where the same `Subject`-kind position recurs (the cluster of predicates that all carry a `participant_id`, for example). Static analysis output, not a runtime type system.

    All three live above the parser and are statically derivable; none add new IR primitives or runtime concepts.

## Performance, when forced

The current bench shows linear scaling on additive workloads (~1.6s per commit, ~1.5s for 100K-transition replay). The bench fixture is purely additive, so these numbers are best-case.

- **Retraction-heavy bench scenario.** Add a `--retract-fraction K` flag mirroring the existing `--noise-claims`. Generates a fixture where K% of the N transitions are wildcard retracts against prior periods. The forcing pressure that would justify snapshot/lattice work is the curve this produces - not raw N. Worth doing the *measurement* before designing the lattice, so the snapshot interval and structure are sized to a real signal.
- **Snapshot / incremental-materialisation for audit replay.** Once the bench surfaces a real problem. Likely shape: a checkpoint table that materialises the live claim set every K transitions, plus a delta-replay path that resumes from the nearest checkpoint. Compounded by concurrent bitemporal query density (matrix-style reports running `list_derived_at` across thousands of historical timestamps).

## Operational completeness

The hardening real deployment needs. Not language growth and not gated on a worked example - these move as Morpholog approaches its first production deployment. Listed, not frozen.

- **Worker supervisor.** The polling outbox worker exists and ships a `StdoutDeliverer`. Missing: a supervisor running multiple workers under restart-with-intensity, with crash isolation between deliverers.
- **Per-target circuit breakers.** A delivery target that misbehaves should be ring-fenced (back off, alert, eventually quarantine), not continue eating the worker's loop indefinitely.
- **HTTP-aware deliverer.** Once a worked example actually sends an outbox intent over the wire, an `HttpDeliverer` with the right retry/idempotency semantics.
- **Parser-side input-depth guard.** `Program::validate` already rejects expression and `for`-statement nesting past a fixed depth, so over-deep IR cannot exhaust the stack in `propose` - the teeth behind "validate untrusted IR before proposing it". The `.morph` parser has no equivalent guard: a pathologically nested source file could overflow the recursive-descent parser before it produces any IR. Deferred because in v0 you author your own `.morph` files; forced once `parse` ingests source from an untrusted origin.

## Language affordances awaiting a worked example

Each lands when an example actually demands the shape. None are pre-decided.

- **Higher-order authority / predicate-pattern matching.** *One* authority claim governing a *family* of transformations, instead of one per kind. The declared predicate vocabulary now provides a metadata home; the shape itself still needs a worked example.
- **Effective time as a separate axis.** As-of gives knowledge time. Effective time - the day a contract becomes binding, the period a posting reflects - is expressible as ordinary claims; combining the two gives bitemporal addressability without `valid_from`/`valid_to` schema columns.
- **Validity windows + repair transitions + exception claims.** First-class typed claims that mark an admitted assertion as in-repair, contested, or quarantined, with audit standing. No bypass flags ever.
- **Materialised derived claims.** Reports are recomputed on demand today. For long audit logs and frequent queries, materialised snapshots become forced - with invalidation modelled as ordinary claims, not as cache machinery.
- **Migrations framework.** A small one. The schema is hand-written today; with multiple deployments and an evolving claim vocabulary, a migrations story will be forced. The shape should be claim-shaped (migration as transformation, schema version as admitted claim).
- **Strict decimal comparisons (`Lt`, `Gt`, `Ge`), date arithmetic, civil intervals.** The current single-comparator-per-kind (`Le` for decimal, `DateLe` for civil dates) is a deliberate floor. The third comparator is what would force the generic dispatch shape; until then, one per kind.
- **`morph fmt` (canonical formatter as a CLI).** The formatter exists in `morpholog-core::format` and is coupled to the parser by the round-trip property test. A CLI front-end (`morpholog fmt <file.morph>` or `--check` mode) lands when a project's worth of `.morph` files starts to feel like editorial drift; not before.

## Deliberately out of scope (revisit only with explicit reason)

Floors, not preferences. Reasons live in [`scope-and-ambition.md`](scope-and-ambition.md)'s "Non-goals" section.

- No entities, classes, services, or ORM in the surface language. Subjects are opaque; predicates attach to subjects; that is the entire object model.
- No general workflow engine. Lifecycle is conjunctions of admitted claims, eventually derived claims. Morpholog is not Camunda and must not grow toward it.
- No arbitrary computation inside transformations. Pure expressions over admitted claims, plus assertions, retractions, intents.
- No BI / analytics / reporting engine. Derived claims govern reproducible read-side outputs; everything else lives outside.
- No optimisation / solver runtime. ETRM scheduling, AP payment runs, dispatch - outside. Morpholog governs the inputs and admits the outputs.
- No ad-hoc query DSL beyond derived-claim queries and the as-of operator.
- **No bypass flags ever** (`skip_validation`, `force_commit`). Exceptions are first-class typed claims with full audit standing.
- No self-hosting. Morpholog governs business state; the compiler is Rust.
- Tree-sitter / LSP grammars: the parser is the forcing pressure. Deferred until an authoring workflow puts real demand on them.

## How to read this file

If something appears on the "awaiting a worked example" list, that's a *deliberate* hold, not a TODO. The project's central discipline is that kernel primitives arrive alongside the example that forces them; reading the list as a backlog defeats the discipline.

If a section moves from "imminent" to "landed" or from "awaiting" to "imminent", that change goes through the same review/PR loop as code changes. The roadmap moves; the doctrine in [`scope-and-ambition.md`](scope-and-ambition.md) does not.
