# Morpholog: Roadmap

Status: operational. This document is what the project is *doing next*. It is forward-looking and lower-stakes than [`scope-and-ambition.md`](scope-and-ambition.md) (which fixes what Morpholog is *for*) and [`runtime-semantics.md`](runtime-semantics.md) (which fixes what the runtime *means*). When this file and design-history disagree, design-history is the historical record; this file is intent.

The "forced by a worked example" discipline binds **kernel and IR primitives**: a new `Prop` / `ValueExpr` / `Stmt` / `Value` variant earns its place only when an example needs it, never speculatively. That is where unconstrained growth is dangerous, and the bar stays high. It is *not* a brake on everything else - tooling, new worked examples, performance work, legibility surfaces, and product completeness move as soon as the direction is clear, not only when an example forces them. The bar for *adding to the language* is deliberately higher than the bar for *building around it*. This is intent, not a wish list.

## Status today

The kernel, PG adapter, CLI, polling outbox worker, and the worked examples are all in good shape. Programmes are declared-vocabulary objects. Execution is structurally inspectable through `propose_with_trace` and the CLI's `--trace` flag, including expression-internal failure-walk that identifies which sub-expression of a rejected `require` or `bind` was responsible. Predicate-scoped loading runs on both read and write paths.

The kernel's identifiers are opaque newtypes, not bare strings - subjects, the transition actor, bound variables, and predicate names - so the compiler keeps the kernel's nouns distinct and a class of runtime checks (e.g. "the actor must be a subject") becomes unrepresentable rather than enforced. Intent and declaration names are the remaining few. The migration discipline for these codebase-wide type changes lives in [`refactoring-playbook.md`](refactoring-playbook.md).

The `.morph` parser arc is closed, and the worked examples are now authored *solely* as `.morph`: each example module embeds and parses its source, with no hand-written IR to drift. A round-trip property test couples the formatter and parser. The CLI is uniformly file-path-driven - it parses whatever `.morph` you point it at and bundles no programmes: `parse` (source -> IR JSON), `check` (parse + `Program::validate()`, silent on clean), `run` (parse + validate + propose against PostgreSQL), and `inspect` / `explain`.

Outbox intents are declared vocabulary too. `IntentDecl` (parallel to `PredicateDecl`) names every intent type the programme may emit; a misspelled `emit` is a validation error rather than a silent route-to-nowhere on the outbox.

The compute-zone interface for non-Rust integrations is in place. `morpholog run` closes the input boundary; `morpholog outbox claim` / `complete` / `release` let a shell or Python deliverer participate in the lease protocol without a Rust `Deliverer` impl. The three-zone doctrine (compute / commit / outbox) is documented in [`scope-and-ambition.md`](scope-and-ambition.md); the round-trip compute pattern in [`outbox-sketch.md`](outbox-sketch.md).

The CLI is split by subcommand under `crates/morpholog-cli/src/commands/`. Adding a new subcommand is "add a file there, add a `Command` variant, add one dispatch arm in `main`."

## Imminent

With the input/output boundaries, the kind/type-compatibility layer, and the intent vocabulary in place, `.morph` authoring is materially more trustworthy than before. Most of the enriched check has landed; one lint-grade layer remains.

The prior art and influences behind the directions below - the explanation engine, materialised claims, obligations, the substrates we reject - live in [`prior-art.md`](prior-art.md). That file is possibility pressure; this one is sequencing.

- **Enriched `morpholog check`.** The structural, kind/type, binding-flow, and actor-context passes all ship today (the full contract is in [`runtime-semantics.md`](runtime-semantics.md)'s authoring-time checks). What remains:
    - **Lint-grade hints under `--strict`.** Unused predicate / intent declarations, `sum(x | body)` where `x` doesn't appear in `body`, unused transformation parameters, fuzzy "did you mean `MayApprove`?" suggestions on an `Undeclared` reference. `--strict` promotes hints to errors; `--json` emits diagnostics in a tooling-friendly form for IDE integration later.

- **Legibility tooling - Morpholog as an explanation engine.** The runtime answers "is this admissible?"; the higher-value questions are "why not, what is still missing, and what does this model make impossible?" - the language an auditor or regulator actually needs. Almost all of it is *derivable from the IR and the existing trace*, not new kernel or surface, and all of it is **deterministic template rendering** of that trace (the `morpholog_core::format` family), **never natural-language generation**: an explanation an auditor relies on must be reproducible and faithful to the exact failing claim. No NLP or LLM in the path; the legibility comes from good predicate / transformation names plus good templates.

    **Shipped:** `morpholog explain` (a rejection rendered as a directly-missing-evidence checklist - the required claim-conjuncts the gate is missing, each paired with the candidate transformations that assert that predicate, a one-hop static lookup) and `morpholog inspect guarantees` (each invariant as its rule, and for `not(...)` invariants the forbidden state named outright), both in prose or JSON. Deliberately one-hop in v0; present blockers, comparator failures, and bounded why-not / abduction are deferred. The carbon-credit example is the flagship the engine points at. See [`design-history.md`](design-history.md).

    **Remaining:** `inspect transformation --graph` (the body as a pre/post dependency DAG), subject-flow profiles (clusters where the same `Subject`-kind position recurs), and `generate controls` (the same static analysis as a control matrix: each control an invariant, the evidence predicates it references, the failure mode it prevents). All derive statically from the parsed programme; none add IR primitives. Full multi-step path-finding is bounded model checking, deferred. Predicate-kind annotations (`kind Standing` / `Evidence`) were considered and deferred - the classification is largely inferable from structure.

## Performance, when forced

The current bench shows linear scaling on additive workloads (~1.6s per commit, ~1.5s for 100K-transition replay). The bench fixture is purely additive, so these numbers are best-case.

- **Retraction-heavy bench scenario.** Add a `--retract-fraction K` flag mirroring the existing `--noise-claims`. Generates a fixture where K% of the N transitions are wildcard retracts against prior periods. The forcing pressure that would justify snapshot/lattice work is the curve this produces - not raw N. Worth doing the *measurement* before designing the lattice, so the snapshot interval and structure are sized to a real signal.
- **Snapshot / incremental-materialisation for audit replay.** Once the bench surfaces a real problem. Likely shape: a checkpoint table that materialises the live claim set every K transitions, plus a delta-replay path that resumes from the nearest checkpoint. Compounded by concurrent bitemporal query density (matrix-style reports running `list_derived_at` across thousands of historical timestamps). This is plain-PostgreSQL application logic: state-at-T is a stateful fold over assert/retract, not a time-bucket aggregate, so it is *not* something a TimescaleDB continuous aggregate could express.
- **TimescaleDB for the audit log - candidate substrate enhancement, when real scale forces it.** `morpholog.audit` is append-only and time-ordered, a natural hypertable on `committed_at`. Two wins would be real at multi-gigabyte scale: chunk exclusion for as-of window queries, and columnar compression of cold chunks (the repetitive-key JSONB payloads compress well, and compressed-chunks-are-read-only suits an immutable ledger - the strongest reason to reach for it, since the target domains produce GB-scale audit lakes). Hard constraints, because this touches the correctness substrate: single-node only (distributed hypertables drop cross-node `SERIALIZABLE`, which is non-negotiable, and are deprecated upstream regardless); the `transition_id` primary key must become composite with the partition column, so the as-of lookup path needs re-checking; and SSI behaviour across chunks (predicate-lock granularity, 40001 rate) must be *measured*, not assumed. It does not replace the checkpoint work above and gives nothing to it. Adopting it would be an explicit revisit of the locked PG-only substrate floor. Timescale's analytics primitives (`time_bucket`, gap-filling) stay outside the boundary, on the BI side, not in governed reads. Revisit only when the bench surfaces a real storage or replay problem on a real workload - not before.

## Operational completeness

The hardening real deployment needs. Not language growth and not gated on a worked example - these move as Morpholog approaches its first production deployment. Listed, not frozen.

- **Worker supervisor.** The polling outbox worker exists and ships a `StdoutDeliverer`. Missing: a supervisor running multiple workers under restart-with-intensity, with crash isolation between deliverers.
- **Per-target circuit breakers.** A delivery target that misbehaves should be ring-fenced (back off, alert, eventually quarantine), not continue eating the worker's loop indefinitely.
- **HTTP-aware deliverer.** Once a worked example actually sends an outbox intent over the wire, an `HttpDeliverer` with the right retry/idempotency semantics.
- **Parser-side input-depth guard.** `Program::validate` already rejects expression and `for`-statement nesting past a fixed depth, so over-deep IR cannot exhaust the stack in `propose` - the teeth behind "validate untrusted IR before proposing it". The `.morph` parser has no equivalent guard: a pathologically nested source file could overflow the recursive-descent parser before it produces any IR. Deferred because in v0 you author your own `.morph` files; forced once `parse` ingests source from an untrusted origin.
- **Schema generation from the declared vocabularies.** `PredicateDecl` and `IntentDecl` make this mechanical now: emit JSON Schema for a transformation's arguments and for each intent's payload, so a non-Rust caller has a typed contract instead of having to learn the `EvalValue` JSON shape by hand. **Transformation arguments shipped** (the `transformation_arg_schema` adapter over `transformation_param_kinds`; the kernel exports the inferred input contract via `ParamKind`, the schema module renders one encoding of it). Intent payloads remain: they share the same shape but trade-flow examples have not yet pushed the embedder to consume them. Generated Python / TypeScript clients and an OpenAPI surface are a larger productisation step, later. The remaining forcing function is the first real embedder consuming intent payloads.
- **`--args` ergonomics for `run` / `explain`.** Both subcommands take transformation arguments as the raw adjacently-tagged `EvalValue` JSON the kernel uses internally (`{"type":"decimal","value":"100.00"}`, `{"type":"subject","value":"..."}`, `{"type":"date","value":"2026-05-01"}`) - faithful, but implementer-facing: a caller has to know the codec before they can invoke a transformation. The declared argument vocabulary makes a friendlier surface mechanical - named values coerced against each parameter's declared kind, so a bare `{"amount": "100.00"}` suffices. Same forcing function as schema generation above: the first real embedder. Until then the raw codec is the documented path.

## Language affordances awaiting a worked example

Each lands when an example actually demands the shape. None are pre-decided.

- **Higher-order authority / predicate-pattern matching.** *One* authority claim governing a *family* of transformations, instead of one per kind. The declared predicate vocabulary now provides a metadata home; the shape itself still needs a worked example.
- **Effective time as a separate axis.** As-of gives knowledge time. Effective time - the day a contract becomes binding, the period a posting reflects - is expressible as ordinary claims; combining the two gives bitemporal addressability without `valid_from`/`valid_to` schema columns.
- **Validity windows + repair transitions + exception claims.** First-class typed claims that mark an admitted assertion as in-repair, contested, or quarantined, with audit standing. No bypass flags ever.
- **Materialised derived claims.** Reports are recomputed on demand today. For long audit logs and frequent queries, materialised snapshots become forced - with invalidation modelled as ordinary claims, not as cache machinery.
- **Migrations framework.** A small one. The schema is hand-written today; with multiple deployments and an evolving claim vocabulary, a migrations story will be forced. The shape should be claim-shaped (migration as transformation, schema version as admitted claim).
- **Date arithmetic, civil intervals.** The comparator set is now complete per kind (`<=` `<` `>=` `>` for decimal; `on_or_before` `before` `on_or_after` `after` for civil dates). What remains deferred is arithmetic *on* dates - adding a duration to a date, the length of an interval - which awaits a worked example that needs it.
- **`morph fmt` (canonical formatter as a CLI).** The formatter exists in `morpholog-core::format` and is coupled to the parser by the round-trip property test. A CLI front-end (`morpholog fmt <file.morph>` or `--check` mode) lands when a project's worth of `.morph` files starts to feel like editorial drift; not before.
- **Integration / external-compute provenance.** The same primitive (claims) reaching the system's edges: external-computation results admitted as provenance claims, outbox intents acquiring delivery/acknowledgement claims, and actor authority extending to delegated and external actors. The longest-horizon direction; deliberately unspecified in detail until the embedder forces a concrete shape.

## Deliberately out of scope (revisit only with explicit reason)

The doctrinal floors - no entities/classes/services/ORM, no workflow engine, no arbitrary computation inside transformations, no BI engine, no solver runtime, no ad-hoc query DSL, no bypass flags ever - all hold here; they and their reasons live in [`scope-and-ambition.md`](scope-and-ambition.md)'s Non-goals and are not re-listed. The deferrals specific to the operational plan:

- No self-hosting. Morpholog governs business state; the compiler is Rust.
- Tree-sitter / LSP grammars: the parser is the forcing pressure. Deferred until an authoring workflow puts real demand on them.

## How to read this file

If something appears on the "awaiting a worked example" list, that's a *deliberate* hold, not a TODO. The project's central discipline is that kernel primitives arrive alongside the example that forces them; reading the list as a backlog defeats the discipline.

If a section moves from "imminent" to "landed" or from "awaiting" to "imminent", that change goes through the same review/PR loop as code changes. The roadmap moves; the doctrine in [`scope-and-ambition.md`](scope-and-ambition.md) does not.
