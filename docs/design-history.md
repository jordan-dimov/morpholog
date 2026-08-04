# What the worked examples forced into the language

Design archaeology. Companion to [`scope-and-ambition.md`](scope-and-ambition.md) (forward-looking doctrine) and [`runtime-semantics.md`](runtime-semantics.md) (what the kernel means). This file records each design move and the worked example that forced it.

The entries are compressed - each a Forced-by/Landed stub, with the per-PR detail and the considered-and-rejected alternatives left to git, and the rationale that became doctrine referenced inline. The one fuller passage is the parser-arc narrative below, kept whole because it orients a reader of the parser rather than recording a single move.

## Pre-parser-arc IR moves (compressed)

These entries record kernel work in the order it happened. Each says what was forced and what landed.

### `Value::Subject` IR variant

**Forced by:** claim standing example. Purposes like `bank_debt_service` needed to appear as constant references in invariant bodies; pre-existing IR had only `Value::Decimal`.

**Landed:** `Value::Subject(String)` variant; matched in `unify_args` and resolved by `resolve_term`. Constant of any other kind earns its variant when a future example needs one.

### The `require`-vs-`invariant` semantic distinction

**Forced by:** claim standing example. A rule like "every X claim implies an active Y" written as an invariant would either reject revocations or cascade-retract history. Neither matches the real semantic (decisions made under valid standing remain valid even after revocation).

**Resolution:** `require` is the admission gate (checked at admission, never re-checked); `invariant` is the eternal rule (must always hold against admitted state). The two answer different questions and are not interchangeable. This is the most reused doctrine in the codebase; pinned in [`runtime-semantics.md`](runtime-semantics.md).

### Currentness and standing as distinct constructs

**Forced by:** the verified-revenue and claim-standing examples together. Currentness ("which claim is in force now?") and standing ("which claim may be relied on for this purpose?") share the same lower-level mechanic - a separate retractable claim that confers a property on another, append-only claim - but they index differently. Collapsing them loses the ability to say "the same verification is in force for the bank but not for investor reporting."

**Pattern:** when modelling "which X is canonical for purpose Y?", first decide whether currentness alone suffices or whether purpose-specific standing is needed.

### History-as-append-only

**Forced by:** the verified-revenue and claim-standing examples. Discipline: content claims are append-only; pointer / admissibility claims are retractable; lineage / revocation claims are append-only again. The middle bucket is the only retractable kind.

### Generic standing on any subject

**Forced by:** claim standing example. `grant_standing` will admit `AdmissibleFor(any_subject, any_purpose)` even if the subject names no claim. The decision transformation catches the absence via its own `require`. Tightening this requires typed predicate declarations (an affordance still awaiting a worked example).

### Derived claims and `Expr::Sub`

**Forced by:** trial balance over the double-entry ledger. "What is the balance of every account?" had no expression in the runtime; computing it in plain Rust put the logic outside the governed model.

**Landed:** `DerivedClaim { predicate, keys, values, domain }` with `DerivedValue { name, expr }` per computed value. `Expr::Sub(Box<Expr>, Box<Expr>)` for decimal subtraction. `enumerate_derived` reuses `find_matches` for binding enumeration. v0 derived claims are not visible to invariants or transformations, not in `State.claims`, not persisted, not recursive, not as-of - each deferred deliberately. (Most of those deferred concerns landed later via the read-side surfacing PR.)

### As-of evaluation

**Forced by:** the read-side surfacing of derived claims revealed that auditors need historical state, not just current. Without an as-of facility, the only escape is bespoke SQL outside Morpholog.

**Landed:** no kernel change - as-of is a question of which `State` you hand to the kernel. `reconstruct_state_at(pool, transition_id)` on `morpholog-postgres` replays the audit log in `(committed_at, transition_id)` order. `list_claims_at` and `list_derived_at` are thin wrappers. `PgError::TransitionNotFound` is the unknown-id failure mode.

### `ReplaySet` (audit-log replay working set)

**Forced by:** the as-of benchmark surfaced a quadratic in `reconstruct_inner` at 10K-100K audit rows. The pattern is the third instance of the same fix: replacing a `Vec` + linear-scan membership check with a HashMap-backed structure that gives amortised O(1) assert / retract.

**Landed:** `ReplaySet` inside `morpholog-postgres`, used by `reconstruct_inner`. Replay at 100K rows became feasible (~1.5s, was projected ~8 minutes).

### `Transition` value object and `audit.actor`

**Forced ahead of an example** (the only ahead-of-example deviation in the codebase). The next authority example would need both the plumbing AND the consultation primitive in one PR; splitting them keeps each honest.

**Landed:** `Transition { transformation_name, args, actor }` in `morpholog-core`. `propose()` takes a `Transition` separately from the `Transformation`. `audit.actor` column on PostgreSQL. Required `--actor` flag on `morpholog propose`. `system_actor()` helper for runtime-initiated transitions (placeholder until lineage forces a richer model). `Term::Actor` deliberately did NOT land here - that's the consultation primitive, forced by the next example.

### `Term::Actor` consultation primitive

**Forced by:** approval-controls example. PR #35 plumbed the actor through the audit log; nothing in the IR could yet consult it. `require MayApprove(actor, doc_type)` needs an IR construct for "the proposing actor."

**Landed:** `Term::Actor` variant on the existing `Term` enum, no payload. Resolves to the actor of the proposed transition; raises `EvalError::UnboundActor` if no transition is in scope. Threaded through every binding-matching function. Authority checks belong in `require`, not invariants - an invariant body that reaches for `Term::Actor` errors at evaluation. Considered (and rejected): stashing the actor under a reserved binding key in `Bindings`.

### `Expr::Le` decimal comparison primitive

**Forced by:** approval-limits example. Unconditional authority ("may approve") gave way to quantitative ("may approve up to N"); `amount <= limit` had no IR expression.

**Landed:** `Expr::Le(Box<Expr>, Box<Expr>)` for decimal comparison. Both operands must evaluate to `EvalValue::Decimal` (anything else surfaces as `TypeMismatch`). Inclusive semantics: `amount = limit` admits. `Lt`, `Gt`, `Ge` each earn their place when an example forces them - the "complete set" temptation was deflected.

### `Expr::Add` decimal addition primitive

**Forced by:** insurance-claim-settlement example. Cumulative limit ("sum of payments + this payment <= cap") had no IR expression.

**Landed:** `Expr::Add(Box<Expr>, Box<Expr>)` for decimal addition. Decimal-only with `TypeMismatch` for other kinds, matching `Sub` / `Le`. Operands compose with `Sum` for cumulative checks.

### `Value::Date`, `EvalValue::Date`, `Expr::DateLe`

**Forced by:** clinical-trial-enrolment example. Inclusive `[from, to]` civil-date windows on a randomisation date had no expression. Comparing dates via `<=` (decimal) would fail at type-check; comparing them as subject literals would degrade to lexicographic ordering (wrong as soon as a year changes digits).

**Landed:** `Value::Date(String)` IR literal in `YYYY-MM-DD` form. `EvalValue::Date` runtime value. `Expr::DateLe` separate from `Expr::Le` - the kernel keeps them as distinct primitives so each type-checks its operands. Doctrine: no operator overloading by operand type. Strict-ordering date comparators (`before`, `after`, `on_or_after`), date arithmetic, intervals, business calendars, and time-of-day are deferred until an example forces them.

### `Stmt::BindOne` (refactor)

**Forced by:** a refactor arc, not a worked example. The "require to gate, then `let` and `value_of` to extract" workaround was idiomatic but indirect: three lines for one canonical move ("match this unique claim and extend the bindings").

**Landed:** `Stmt::BindOne(Expr)` as a first-class statement. Semantics: evaluate a predicate-shaped expression, require exactly one match, treat the match's bindings as the next context. The old workaround still works; new examples use the primitive.

### `PredicateDecl` + `Program.predicates`

**Forced by:** the refactor arc establishing predicate declarations as the canonical vocabulary of a programme. Pre-existing programmes were "vocabulary by implicit usage"; the runtime had no way to validate arity, kinds, or document the predicate surface.

**Landed:** `PredicateDecl { name, args: Vec<PredicateArgDecl> }` on `Program`. Strict arity validation: a claim with the wrong number of args for its predicate is `EvalError::ArityMismatch`. Kind information is documentation today; future static checks may consult it. `predicate(...)` DSL builder helps construct decls fluently in the Rust IR.

### `propose_with_trace` (kernel) + `propose_against_pg_with_trace` (PG adapter)

**Forced by:** the refactor arc preparing for legibility tooling. The runtime needs to explain *why* a proposal succeeded or rejected, not just whether. Without trace, the only diagnostic available to a rejected proposer was "rejected" + the error variant.

**Landed:** `propose_with_trace` returns `TracedProposal::{Completed, Errored}` - trace flows on both success and kernel-error paths. The PG-adapter equivalent `propose_against_pg_with_trace` returns `Result<PgTracedOutcome, PgError>` with `PgTracedOutcome::{Outcome, KernelErrored}` carrying trace through the kernel-error path; PG-layer errors flow through `Err(PgError)` and drop trace. CLI `--trace` emits structured JSON.

### Predicate-scoped loading

**Forced by:** the `--noise-claims` benchmark scenario. Loading the full claim set on every proposal scales linearly with admitted-state size; programmes that touch a small fraction of predicates were paying for the whole table.

**Landed:** `propose_against_pg` loads only the predicates the transformation and its invariants reference. Read and write paths are separately scoped via `predicates_referenced_by_*` walkers. Bench surfaced the win at 100K-row scale.

### Expression failure-walk on rejection paths

**Forced by:** the same legibility arc. A rejected `require` says "rejected" but not which sub-expression. For a long boolean chain, that's the wrong granularity; the auditor needs the specific clause that didn't hold.

**Landed:** `find_failing_subexpr` walks the rejected expression and identifies the smallest sub-expression responsible. Trace exposes it via `TraceEntry::RequireRejected.failing_sub_expression`. `find_conjunction` and the failure-walker share the binding-flow rule: each conjunct sees the prior conjunct's extensions; a walker that re-evaluates each conjunct against the original bindings is wrong.

## What deliberately stayed minimal across the early examples

Each was considered during a specific example and deferred. They live here so the deferred reasoning survives.

| Considered | Why deferred |
|---|---|
| `ReliedOnBy(decision_id, claim_id)` decision-snapshot pattern | The simpler `require`-based design proved sufficient. |
| `RevocationLifted` claim or grant-supersession for re-grant | Revocation is terminal in v0. Re-granting needs a lifecycle model an example doesn't yet force. |
| Temporal qualification claims (`OccurredOn`, `EffectiveFor`, `KnownAsOf`) | Candidates for the as-of operator; no example yet needs them. |
| Strict comparison (`<`, `>`, `>=`) | `Le` and `DateLe` are the only comparators today. Strict variants land when an example needs strictness. |
| Predicate-pattern matching (one authority claim governs a predicate family) | Higher-order authority shape; awaits an example. |
| Cumulative or time-bounded limits ("up to N per day") | Needs `Sum` over a time-bounded subset of admitted state. |
| Cascading retraction of historical decisions on standing revocation | Contradicts "history is preserved." |
| Per-example PostgreSQL schemas | All examples share the canonical runtime schema; predicates differ in what they admit, not in storage shape. |

## Parser arc (closed)

The `.morph` surface was built incrementally across eight PRs (P1 through P3c): predicate declarations, then expression syntax in isolation, then the bounded forms, then programme-level integration (invariants, then transformations + statements, then derived claims), then civil-date comparison, then the round-trip property test that closed it. The per-PR blow-by-blow lives in git; what follows is the load-bearing set of decisions a reader of the parser needs.

**The premise.** Programmes were Rust IR via the `dsl` module; the natural reader of a Morpholog programme is a domain expert, not a Rust developer. The arc gives the IR a surface. The crate is `morpholog-surface`, not `morpholog-parser`, because it also hosts the formatter and will host source-mapping and LSP tooling - one crate per source-aware concern, not one per tool.

**Pipeline.** Character lexer (`chumsky`) -> layout pass (virtual `Indent`/`Dedent`) -> structural parser -> `morpholog_core::Program`. Diagnostics (`ariadne`) carry byte-offset spans and render to line/column/caret; spans live entirely in `morpholog-surface` and never leak into kernel errors, which are runtime-shaped. Recovery collects multiple diagnostics per run by syncing at the next top-level keyword. Top-level declarations interleave freely (predicate / intent / invariant / transformation / derived in any order); duplicate-name detection runs parser-side so diagnostics carry both spans.

**Position A, made enforceable.** The surface is more readable than the IR but never more powerful (the doctrine in [`scope-and-ambition.md`](scope-and-ambition.md)). Every place the surface could outrun the IR, the parser refuses:

- `In` and claim-call arguments are `Term`-only in the IR, so `a + 1 in xs` and `Foo(x + 1, y)` are parse errors, not silently ill-shaped IR. (`Neq` was `Term`-only here too until the comparator-collapse PR lifted it to expressions; see below.)
- No `true`/`false` literals: the IR has no `Value::Bool` to lower them to. Reserved at the lexer, rejected at the parser.
- `value Pred(args) [default expr]` is claim-pattern-shaped, not a general `value(target | body)` query - the latter would be more expressive than `Expr::ValueOf` can represent. (A doctrine-table row had the wrong shape; corrected before any parser code committed it.)
- Transformation parameters carry no kinds, because `Transformation.parameters: Vec<String>` doesn't.

**Layout.** Blocks are indentation, not braces. Spaces only - a tab in indentation is a diagnostic, refusing the tabs-vs-spaces ambiguity rather than guessing. Parens disable layout, so a long expression spans lines freely inside them. No virtual `Newline` token: every statement and declaration starts with its own keyword, which is the boundary. Nested blocks (a `for` body inside a transformation) use a `recursive` parser bounded by matched `Indent`/`Dedent` pairs; each `: <body>` consumer (invariant, `exists`, `forall`) accepts inline or indented form via a local `(Indent body Dedent | body)` choice.

**Literals and quantifiers.** `@YYYY-MM-DD` for civil dates, `#NAME` for subjects - sigils resolve the arithmetic ambiguity (`2026 - 05 - 22`) with no lexer complexity; decimals are carried as strings to preserve exactness. `forall x in <source>` auto-lifts a bare-variable source to `Expr::In(Var(x), source)` so the kernel can iterate, while a claim-shaped source passes through. `in` is positionally disambiguated: structural inside `forall`/`for`, a membership comparator elsewhere. Quantifier bodies are greedy to the end of the enclosing expression; composition with outer terms needs parens. `sum`'s target is restricted to a variable name.

**`on_or_before` for date comparison.** Civil-date `<=` lowers to `Expr::DateLe`, a separate kernel primitive from decimal `Le`. Rather than dispatch `<=` on operand kind (the parser has no type environment, and the `DateLe` design deliberately separated the two), the surface uses a keyword that reads as a regulatory clause: `effective_from on_or_before randomisation_date`.

**Round-trip property test.** `tests/round_trip.rs` runs every `all_programs()` entry through `format_program -> parse_program -> assert_eq`, catching formatter drift and parser regressions together; a new worked example extends coverage for free. Building it forced the formatter to emit canonical *parseable* text (infix booleans, `admit`/`bind` verbs, `#`/`@` sigils, `on_or_before`) instead of debug-style output, and surfaced the actor-shadowing fix: transformation parameters named `actor` collided with the auto-mapped `Term::Actor`, so they were renamed `principal` - clearer anyway, since the parameter is the *subject* of an authority claim, not the proposer.

The arc is complete for v0: every worked example parses end-to-end. `morpholog run <file.morph>` (proposing a user programme against PostgreSQL) and the enriched `morpholog check` build on the closed surface.

## After the parser arc

Post-parser kernel and tooling moves, each a Forced-by/Landed stub (the per-PR blow-by-blow and the considered-and-rejected alternatives live in git; only the rationale that became reusable doctrine is kept inline).

### `Expr::Or` predicate-shaped disjunction

**Forced by:** chess `SingleCapturePerMove` (`after = before or after = before - 1`); landed a PR ahead of chess to keep that one on `Pre`.

**Landed:** `Expr::Or(Vec<Expr>)` mirroring `And`'s flattened shape; branch bindings concatenate without dedup. `find_failing_subexpr` returns `None` for `Or` - a failed disjunction failed every branch, so blaming one misleads.

### `Expr::Pre` transition-invariant primitive

**Forced by:** the chess transition-invariants example. State invariants (one state) and transition invariants (a pre/post pair) are distinct kinds; "MoveCount advanced by one" needs both states. Chess was chosen because no business example had operational state to compare across a transition.

**Landed:** `Expr::Pre(Box<Expr>)` - a subtree opts into pre-state evaluation; post stays default (the inverse of TLA+ priming, since Morpholog has one state at a time). `EvalError::PreStateUnavailable` is phrased about evaluation context, not AST position, so future both-state contexts share it.

### Insurance retrofit: `PolicyHeadroom` conservation via `pre(...)`

**Forced by:** closing the `Expr::Pre` PR's deferred business case with a real conservation rule (insurance, which already lives around aggregate-cap discipline).

**Landed:** `PolicyHeadroom(policy_id, remaining)` with uniqueness + existence guards; `authorise_settlement` binds, retracts, and re-asserts the decremented headroom. The transition invariant uses the sum-of-new-payments delta form, not a per-row equation - the per-row draft double-counted two same-amount settlements (caught in review). Bundled the `EvalContext { state, pre_state, bindings, actor }` refactor, now honestly forced by `Pre`'s second contextual arg.

### Enriched `morpholog check`: kind/type compatibility

**Forced by:** the input/output boundary made authoring trustworthiness the bottleneck - decimal-in-subject-slot, `<=` against a date, arithmetic on a subject literal all only surfaced at runtime as `TypeMismatch`.

**Landed:** `morpholog_core::kindcheck`, merged with the structural pass into one `Vec<ValidationError>`. Kind inference refines on observation (`Any` stays unconstrained until a concrete use). The walker mirrors the runtime binding quartet and shadows `Sum`/`Forall`/`Exists` binders so a loop-local cannot collide with an outer variable.

### `IntentDecl`: outbox vocabulary as a declared first-class kind

**Forced by:** the KYC example, where a mistyped `emit SARFiled(...)` would silently route to nowhere - the regulatory breach invisible until a downstream auditor noticed. Intents were stringly-typed and kindcheck skipped `Stmt::Emit`.

**Landed:** `IntentDecl` mirroring `PredicateDecl`; surface `intent X(args)`; strict from day one. The foundational move was the `VocabularyKind { Predicate, Intent }` enum so the two vocabularies share every diagnostic shape (`UndeclaredPredicate` generalised to `Undeclared { vocabulary, name, context }`); a third declared vocabulary would slot in without further renaming.

### Static-analysis pass: one visitor, binding flow, actor context, and a depth floor

**Forced by:** the kind check had landed as a second walker; the next checks (unbound variables, value/predicate shape, actor context) all needed the same traversal and scope notion, so the two-walker shape stopped paying.

**Landed:** `kindcheck` became `check`; `validate_program` is now a duplicate-declaration pass plus one `check_program` traversal carrying a `Scope { kinds, bound }`, cloned at the quartet's boundaries so the non-export rules fall out of structure. The subtlety pinned by running over every example: a disjunction exports only the *intersection* of names its branches bind, and `in` is a generator not a use. Added the nesting-depth floor (`NestingTooDeep` - the enforceable form of "validate untrusted IR before proposing it").

### Counting via a constant `sum` target (and dropping `Sum.binding`)

**Forced by:** the chess example wanted to count pieces, and nothing in the kernel could express it.

**Landed:** the `sum` target now accepts a decimal literal, so `sum(1 | body)` counts bindings - a parser relaxation only (the target was already resolved per match). The change retired the vestigial `Expr::Sum.binding` field. A dedicated `Expr::Count` was rejected as redundant with `sum(1 | body)`.

### The full comparator set per kind

**Forced by:** legibility, not a worked example - the deliberate exception. The strict forms are derivable (`a > b` is `not(a <= b)`), but the kernel has no `>` to render, so the formatter would print `amount > limit` back as `not (amount <= limit)` - derivability buys nothing for the auditor reading formatted output.

**Landed:** `Expr::Lt`/`Ge`/`Gt` and `DateLt`/`DateGe`/`DateGt`, each first-class so it round-trips as written. The duplicated comparator arms became `decimal_comparison`/`date_comparison` helpers parameterised by an `admit` closure. `before`/`after` are matched contextually rather than reserved, because existing examples use them as variable names.

### The explanation engine: rejection as read-side trace interpretation

**Forced by:** the legibility gap - the runtime could say *whether* a transition was admissible; the buyer's question is *why not, and what is still missing*. `propose_with_trace` already carried the failure data, so the work was interpretation.

**Landed:** `morpholog_core::explain` returns a structured `Explanation` from the kernel trace (`Admissible`, or a gate / invariant / kernel-error `Rejection`), rendered to deterministic claim-shaped prose or JSON, never NLP - an explanation an auditor relies on must be reproducible and faithful to the exact failing claim. v0 is one-hop (directly-missing positive claims only); true minimality is abduction, deferred.

### Carbon-credit provenance: the flagship that forced no primitive

**Forced by:** nothing in the kernel - and that is the point. The carbon-credit domain was chosen as the explanation engine's first home because its failure mode is pure legitimacy; the test was whether the claim model could carry evidence-provenance with no new primitive.

**Landed:** `carbon_credit_provenance` models the whole chain as claims about claims, gating `issue_credit` on a verified measurement, an attestation, and a currently-accredited verifier. The MRV computation stays outside and returns as an admitted `VerifiedMeasurement` quantity; `Expr::Mul` (generation-times-factor) was rejected as crossing the inside/outside boundary - the computation is the meter.

### Obligations over time: the outside-coordinator sweep

**Forced by:** the carbon domain's compliance half - a scheme obliges an account to retire enough credits by a deadline. The first example with a rule about *time*, and the kernel has no clock by design.

**Landed:** obligations as ordinary claims; `discharge_obligation(obligation, current_date)` admits satisfaction only when the retired total reaches target on or before the deadline; `sweep_obligation(obligation, current_date)` is the **outside-coordinator tick** - an external scheduler hands "now" in as an argument, never a clock the kernel reads (which would break the decidable core and make admissibility non-reproducible). No primitive - `Sum` already handles a join body, and the deadline is the existing `after` comparator.

### `inspect guarantees`: the model's impossibilities, before it runs

**Forced by:** the legibility brief's other half. `explain` answers "why was this rejected?"; a controller asks first "what does this model make impossible?".

**Landed:** `morpholog_core::guarantees(program)` returns one `Guarantee` per invariant - the rendered rule, plus a `forbids` clause naming the bad state only for `not(...)` invariants (whose inner expression *is* the forbidden state). Pure static read, tested across the whole registry. Deriving a forbidden state for `implies`/comparator invariants was rejected as semantic interpretation rather than formatting - honest over impressive.

### `morpholog explain`: the engine reachable from the command line

**Forced by:** the explanation engine shipped as a library with no way to ask the question from outside Rust.

**Landed:** `morpholog explain <file.morph> <transformation> --args <json> --actor <subject>` - the read-only counterpart of `run`, loading the predicate-scoped pre-state via a new `load_scoped_state` (a plain pooled read, not a SERIALIZABLE transaction, because explaining is a question, not a commit). The verdict does not affect the exit code; only operational failures exit non-zero.

### Collapsing the comparator catalogue into `Expr::Compare`

**Forced by:** the IR itself. The ordered comparators had grown to eight flat variants for what is structurally one operator over one of two ordered domains; every `Expr` walker paid an eight-way fan-out (`check` alone enumerated them thirty-eight times).

**Landed:** one `Expr::Compare { op, domain, left, right }` - the operator stays first-class (still renders as written, `>` never becoming `not(<=)`) but the domain is a field, so a future ordered domain is one row, not four variants across ten files. Zero behaviour change. Equality (`Eq`/`Neq`) stayed a separate node because it is kind-agnostic where the ordered comparators are domain-specific - two nodes keep each honest. The same PR lifted `Neq` from `Term`-only to full expressions, symmetric with `Eq`.

### `.morph` as the single source for the worked examples

**Forced by:** the parser arc's logical conclusion. With every example already parsing faithfully to its registered IR, the hand-built Rust IR in `morpholog-examples` was a second authored copy - a maintenance tax and a drift risk the faithfulness test existed only to police.

**Landed:** each example embeds its `.morph` (`include_str!`), parses it once into a `LazyLock<Program>`, and derives its by-name accessors from it. The runnable program *is* the teaching source, so they cannot drift; the now-vacuous faithfulness test was removed. `morpholog-examples` gained a dev-only reverse dependency on `morpholog-surface`.

### `dsl` -> `ir_builder`: the IR-builder is test tooling, not a language

**Forced by:** the reveal above. Once the examples were `.morph` and the parser became the only thing in the product that builds IR, the `dsl` module was left doing one honest job - constructing IR in tests, including the adversarial shapes a parser would never emit. The name "DSL" read as a rival language surface.

**Landed:** `morpholog_core::dsl` renamed to `ir_builder` - a pure rename, still public in core (an IR-builder in a crate depending back on core hits Rust's "two versions of the crate" wall, so it must stay). Business and explanation stories are authored in `.morph`; invariant-teeth tests that build "the real transformation minus one statement" keep the builder, marked deliberately adversarial.

### Splitting `Expr` into `Prop` and `ValueExpr`

**Forced by:** the deepest conflation left in the IR. One `Expr` did two jobs - truth-witness in predicate position, value in value position - so the kernel carried a dynamic repair for a static mistake (`NotPredicate`/`NotValue` plus re-policed static checks). The `.morph`-canonical work made the split tractable: the parser is the sole constructor and already knows its position.

**Landed:** two sorts. `Prop` *searches state and produces binding witnesses*; `ValueExpr` *computes one value*; mutually recursive where it matters. `find_matches(&Prop)` and `eval_value(&ValueExpr)` are now total over their sorts, and the boundary is a Rust type - the dynamic and static shape-checks are deleted, unconstructable. This *restores* "only invariants and transformations are first-class" by giving their bodies an honest shape rather than an overloaded `Expr` blob.

### Opaque newtypes for the kernel's identifiers

**Forced by:** the same conflation on the *noun* axis. Every identifier was a bare `String` - subject id, predicate name, bound variable, transformation name all the same type - so a function taking `predicate: String` would silently accept a subject. The kernel asserted "subjects are opaque" in prose, not in its types.

**Landed, one opaque newtype per slice** (`From` / `Display` / `as_str` only, no `Deref`, so call sites stay explicit): `Subject` first (deleting the runtime non-subject-actor guard), then `Var`, then `PredicateName` / `IntentName` / `TransformationName` / `InvariantName`. Near-identical, they collapsed into one `opaque_id!` macro whose `PartialEq<str>` is the load-bearing ergonomic. Like the two-sort split, this is the make-the-model-true discipline. The `Subject`/`actor` slices were hand-propagated by a sub-agent; the rest followed the [refactoring playbook](refactoring-playbook.md), which is the bar going forward.

### Trade lifecycle: phase as accumulated claims, forcing no primitive

**Forced by:** nothing in the kernel - the point again. The trade-lifecycle domain (capture, confirmation, price correction, settlement) was chosen as the first **external-embedder** target; the test was whether a multi-phase lifecycle could be modelled with no new IR shape.

**Landed:** `trade_lifecycle` models the phase as the accumulation of admitted claims, never a mutable status field. Three recorded decisions: lifecycle as claims (so as-of replay reconstructs any past phase); official-price correction as restatement lineage (the confirmation *event* split from the price *figure*, so a settlement under the prior figure stays standing); and no declared parameter kinds forced (every parameter flows into a typed claim position, so inference is total). The quantitative gate reuses `Compare`, the commodity-scoped authority reuses `Term::Actor`.

### `ValueExpr::Mul` and `Div`: completing the decimal arithmetic

**Forced by:** the borrowing-base example - a proportional limit (drawn within an advance rate times collateral) is a multiplication; a reported drawn-fraction is a division. `Add`/`Sub` could express neither.

**Landed:** `ValueExpr::Mul` (exact) and `ValueExpr::Div` (a zero divisor surfaces `EvalError::DivisionByZero`). This **reopens the carbon `Mul` rejection deliberately**, with the refined line: arithmetic that expresses an admission gate or a read-side projection is inside; arithmetic that produces a stored governed figure stays outside. Two conventions keep `Div` honest - gates use the cross-multiplied form so decisions stay exact, and `Div` is confined to read-side projections.

### `ValueExpr::Min` and `Max`: decimal floor and cap

**Forced by:** the insurance per-claim layer - a payout bounded both sides at once: `min(per_claim_limit, max(0, loss - deductible))`.

**Landed:** `ValueExpr::Min` and `Max` (exact, non-decimal operands surface `TypeMismatch`), surface form `min(a, b)` / `max(a, b)` - self-delimiting, so the formatter needs no parenthesisation. The forcing home is an invariant kept purely additive (it bites only when an optional `CoverageTerms` claim is set).

### Collapsing the arithmetic catalogue into `ValueExpr::Arith`

**Forced by:** the same per-operator churn the comparator collapse cured, now on the value sort - six near-identical `ValueExpr` variants touching the same ten match arms across the kernel.

**Landed:** one `ValueExpr::Arith { op, left, right }` mirroring `Compare`; the infix-versus-function split is now the `ArithOp::is_infix` predicate. Pure internal refactor, zero behaviour change (no `Serialize` on `ValueExpr`, no wire format to migrate). A future operator is one `ArithOp` row plus one dispatch line.

### `ArithOp::Mod`: parity, and the chess remodel that forced it

**Forced by:** the chess example remodelled - a square's colour is computed (`(file + rank) % 2`), and remainder was the one arithmetic operator the examples had not forced.

**Landed:** `ArithOp::Mod` (the small addition the `Arith{op}` collapse set up - one enum row, one eval arm reusing `DivisionByZero`), plus a `%` token and a square-colour layer on chess. The remodel replaced the opaque square subject with `(file, rank)` coordinates - the coordinate *is* the square. The collapse paid off exactly as predicted.

### `Prop::Xor`: a legible exclusive-or

**Forced by:** legibility, not expressiveness - `(a or b) and not (a and b)` reads as bookkeeping when `a` and `b` are full claim patterns. The only kernel addition so far justified by how a rule reads.

**Landed:** `Prop::Xor(Box<Prop>, Box<Prop>)`, defined as and evaluated by lowering to exactly that idiom (so its binding semantics cannot drift). Binary, not flattened (an n-ary xor is ambiguous between "exactly one" and "odd parity"). Forcing home: the KYC adjudication fork, whose distinctive teeth are the totality half - it rejects an adjudicated marker carrying *neither* disposition, which a plain exclusion would miss.

### Trade lifecycle: partial settlements (the ETRM enrichment begins)

**Forced by:** nothing in the kernel - the first step of deepening `trade_lifecycle`. A real commodity trade settles in slices, so the one-settlement-per-trade gate was the simplification to lift first.

**Landed:** `TradeSettled` became a settlement *slice*; the per-settlement cap became the cumulative `sum(q | TradeSettled(...)) <= captured_qty` (the insurance aggregate-cap shape). The settlement id becomes a real idempotency key, split across a `require` gate (refuses a replayed id before a second downstream emit) and a uniqueness invariant (the backstop). No new primitive - cross-example pattern reuse.

### Trade lifecycle: effective-dated terms and bitemporal replay

**Forced by:** the second ETRM step - a trade's quantity is re-agreed after the fact, effective from a past date, and the book must still answer "what was effective on date D?" years later. The valid-time axis, counterpart to the transaction-time axis `--as-of` already supplied.

**Landed:** quantity moved onto a versioned `TradeTerms(trade, version, quantity, period, effective_from)`; `amend_trade_terms` admits a later version effective from a possibly-backdated date, the original staying on record. **Forced no primitive - the headline.** "Terms in force on `d`" is the *not-a-later-one* pattern built from `on_or_before`/`after`/`exists`/`not`, all of which existed; the cap became a running total by effective date. So bitemporality is *demonstrated, not built*: the valid axis is dates on claims, the transaction axis is the replayed audit log - no temporal-database columns, the floor the prior-art doc draws against XTDB-as-substrate.

### `IntentDecl` and `PredicateDecl`: structurally similar, deliberately not unified

**Considered:** collapsing `PredicateDecl` and `IntentDecl` into one `Decl` (both carry only a name and a `Vec<ArgDecl>`).

**Decision:** keep them two. The name fields use distinct opaque newtypes, so a generic `Decl<N>` would never range over both, and a `Decl { name: String }` would erase the distinction the kernel just paid to add. The two vocabularies describe distinct kinds of thing (admitted claim shapes versus outbox effect shapes); separation is honest at the type level. Subtraction over churn - the two-struct shape carries no maintenance tax unifying would remove.

### The first embedder-facing kernel surface: per-transformation argument schemas

**Forced by:** the external ETRM embedder consuming the model from outside the workspace. The smallest honest surface is "given a validated programme and a transformation name, tell me what the outside world must supply."

**Landed:** `analysis::transformation_param_kinds` (the resolved input contract) and a `schema::transformation_arg_schema` JSON Schema adapter - **the kernel exports the inferred input contract, not a client protocol** (a future OpenAPI or dataclass renders the same `ParamKind` differently). **Forced no IR change - the headline:** every `trade_lifecycle` parameter flows into a typed position, so inference is total. Two doctrinal points: the accessor uses a *sibling* walker that never clones scope (a parameter used only inside `require` is still externally supplied), and `ParamKind` keeps `Concrete`/`Polymorphic`/`Unconstrained`/`Ambiguous` distinct (a review caught that collapsing the multi-kind case to one kind would make the schema lie about disjunctive-input transformations).

### `Value::Timestamp` and `Value::Duration`: the time arc opens

**Forced by:** `examples/12_laytime_demurrage/` - laytime is a time allowance, demurrage the priced excess, the Statement of Facts a minute-by-minute log. Modelling it needs exact instants, exact spans, instants shifting by and differencing into spans - none expressible over `Decimal` and civil `Date` without lying about the values.

**Landed:** `Value::Timestamp` (RFC 3339, `@...T14:00:00Z`) and `Value::Duration` (ISO 8601, `duration(PT6H)`), backed by `jiff`. The arithmetic rule matrix is enforced twice (`NoArithRule` at authoring, refused again at eval); `Sum` became type-driven. **Zone-less by design - the headline:** a `Timestamp` is an exact UTC instant; local calendars enter as admitted `PortDay` claims from an external authority, never a runtime tzdb, because a computation depending on which tzdb shipped cannot honestly replay. `SignedDuration` not `Span` (exact seconds only). The empty-sum landmine (empty `Sum` is decimal zero) is pinned and answered by a zero-length seed interval, not a coercion. And demurrage is *not* an invariant - running over the allowance is the normal priced outcome, so the excess is a derived claim floored at zero.

### `Value::Quantity` and `PredicateArgKind::Quantity(Unit)`: units land as labels

**Forced by:** the laytime example's third stage - cargo in tonnes against capacity, demurrage in dollars against what the delay is worth.

**Landed:** **units are contractual labels on exact decimals, not physical dimensions - the headline.** A `Decimal[USD]` is an exact decimal whose declared unit must match before arithmetic; no registry, no SI semantics, no compound symbols (`USD/day` is a field name and a formula, never a unit), which keeps dimensional analysis outside the kernel. Conversions are domain knowledge with provenance and time, so they enter as claims when an example forces one - the same doctrine that keeps tzdb lookups out. The algebra is the smallest forced (same-unit add/sub/min/max/sum/compare, bare-decimal scaling, same-unit ratio, `Duration / Duration -> Decimal`). A unit can be declared, never inferred; the wire carries the bare amount; diagnostics always print `Decimal[USD]`; aggregates seed `0 t` / `0 USD`, the seed pattern now official.

### Example 13 (EU AI Act Article 12) and `inspect controls`: the zero-kernel statute

**Forced by:** Articles 12 and 14(5) of Regulation (EU) 2024/1689 - record-keeping and two-distinct-person verification as admission law. Its headline is what it forced: **nothing in the kernel or the surface.** Authority grant/revoke, validity windows, standing-by-verification, and exact instants composed directly into a statute - the load-bearing evidence that the governance patterns are domain-independent.

**Landed:** the two-person rule is deliberately *both* a gate and an invariant (the authority check in the gate so revoking oversight never reaches back; the distinctness requirement an invariant, sound only because verifications are append-only - a later-fraudulent one will need exception-claims, not retraction). No clock (timestamps supplied, never read), so the record replays identically. **`inspect controls`**, forced by the example's own statute-clause-to-rule mapping table: a control matrix pairing each transformation's `require`/`bind` preconditions with the invariant guarantees - the gate-side complement to `inspect guarantees`, pure rendering over the IR.

### `define`: named propositions, the language's second authoring tier

**Forced by:** the worked examples three ways at once - the clinical-trial gate (a ~20-line conjunction with no way to name its sub-conditions), the AI Act's two-verifiers proposition carried verbatim in two places, and the trade lifecycle's "terms in force on a date" (the least readable invariant in the repo). One missing construct: declare a recurring condition once, under the business's own name.

**Landed:** `Definition { name, parameters, body }`; `Prop::Defined { name, args }` as the call; surface `define name(params): body`, resolved by name after the whole programme is collected (so a call may precede its definition). Load-bearing doctrine: **relational substitution with projection, not macro expansion and not a closure** - the body sees only its parameters (hygiene absolute, pinned adversarially) and projection deduplicates (a call counts 1 where its inlined body counts 2). Bodies are context-free (`actor`/`pre(...)` inside are errors). **Every walker stays transitive through `Defined`** as a red line - scoped loading, check, schema, controls, explain all descend, with one canonical `definitions` module owning expansion. The audit-honesty note: a definition edit changes invariant meaning without changing invariant text, so the programme hash, not the invariant list, names the rules in force.

### Claim disciplines: the doctrine moves onto the predicate declarations

**Forced by:** a census that found 22 hand-written uniqueness invariants (~80 lines of bookkeeping), four re-derived currentness pointers, and - the sharpest finding - models whose invariant-safety *rested* on append-only-ness asserted only in comments. Doctrine the runtime could not see.

**Landed:** the declaration clauses - `unique by`, `append only`, `current pointer by`, `superseded via` - lowered to generated invariants where a state rule is needed, enforced statically where cheaper, metadata either way. Doctrine kept inline: `unique by` is full agreement (the SQL-UNIQUE reading); `append only` is a static retract-ban, free at runtime; the pointer tier decomposes (`current pointer by` IS `unique by` plus the retractable-pointer class; `superseded via` is no-fork only); generated invariants are materialised and *prepended* so a root-cause rejection names the discipline, and carry `from:` provenance. Boring generated names are load-bearing (they appear in rejection reasons). Making disciplines a general rule-template mechanism was rejected - properties of claim shapes only: boring, deterministic, generated, visible, few.

### The lint tier, opened by the gate-vs-invariant hint

**Forced by:** the disciplines PR's own promise - declaring append-only and current-pointer classes made the oldest doctrine in the repo (authority/currentness belong in gates, never invariants) mechanically nameable.

**Landed:** a `Lint` surface distinct from `ValidationError` on purpose - an error means the programme cannot mean what it says; a lint means it says something usually-but-not-always a mistake. `check` prints hints to stderr and passes; `--strict` promotes; stdout stays silent. The first lint is **forward direction only** and polarity-aware, descending through defined calls; it is a hint not an error because continuous-compliance is a legitimate reading (the KYC onboarding shape stays clean precisely because its pointer is not declared append-only). Every worked example is lint-clean, pinned by a cross-example test.

### `run --batch`: the import shape, and an exit code that tells the truth

**Forced by:** the external embedder's import path (rows in, a receipt per row, each its own SERIALIZABLE commit) and the first throughput lever the bench's contend axis names.

**Landed:** NDJSON rows naming their own transformation and actor, decoded by the same codecs through the same adapter calls into the same envelopes - plus a `row` number, so the batch contract cannot drift from the single-run contract. Two doctrinal points: a malformed row is a receipt, not a process failure (the rows after a corrupt line still run), and **the exit code tells the import's truth** - zero whenever every row was processed (partial admission is an import's normal outcome), non-zero reserved for operational failure. The contrast with single `run` (exit 1 on rejection) is documented, not smoothed over.

### Source-located diagnostics: every finding points at the line

**Forced by:** the asymmetry every author hit - parse errors arrived with ariadne carets while validation errors, lint hints, and runtime rejections said *what* but never *where*. The Glasshouse authoring workflow (models far larger than the worked examples) made the decision due.

**Landed:** spans survive parsing in a `SourceMap` returned beside the IR (`parse_program` stays the discarding wrapper). Doctrine kept inline: **the map lives beside the IR, never inside it** - the kernel stays source-agnostic, carrying only a statement *index* the surface resolves to a span. Declaration + top-level-statement granularity (sub-expression spans wait for the kernel arc's shared fold); unresolvable findings degrade to a plain line, never block; the runtime rejection location is a stderr courtesy, leaving every pinned stdout envelope byte-identical.

### The client is a projection too: `generate python-client` and `schema --result`

**Forced by:** convergence - two independent Python embedders hand-wrote the same codecs, envelope models, subprocess adapter, and manifest-driven models, none embedder-specific, all able to drift. The doctrine already named the resolution: `.morph` is the truth and the client is the one projection still hand-maintained downstream.

**Landed:** `morpholog generate python-client` plus the `schema --result` envelope contract it consumes. Doctrine kept inline: **static templates emitted verbatim, only models generated** (the tested file IS the emitted file, determinism free); **one sample set holds three artefacts together** (goldens byte-equal to real serialization, validated against `result.json`, loaded by the client's own tests - the binary, schema, and client cannot drift apart without a test naming it); stdlib-only with a CI-enforced 3.10 floor (any library couples a Rust project to its release cadence); **whole-run refusal** (non-concrete kinds, Duration/Collection/Any, keyword names - every finding listed, nothing written; the laytime example is refused by name).

### inspect coverage: which of these rules has ever actually done work?

**Forced by:** the bitemporal-vacuity lesson made real (an effective-time invariant went silently vacuous when no governing claim existed), and every literature survey ranked rule-quality reporting high-leverage. The verification arc's opener, kernel-free.

**Landed:** `morpholog inspect coverage` - the audit log replayed through a coverage tracker. Doctrine kept inline: **the verdicts are bounded by what committed history can honestly show, and the report says so** - *fired*, *never fired* (the headline), *always on* (prohibitions, whose work is invisible); *constrained* awaits the rejection log and *dead antecedent* is static analysis, both named in the legend. The shape classifier reuses the lint tier's polarity walker (transitive through definitions); the delta prune is the cost model, but `pre(...)` antecedents are exempt (their firing lags the delta by one transition - a review catch). Reads under `SERIALIZABLE READ ONLY DEFERRABLE`. Counts transitions, not binding witnesses (multiplicity is noise at the auditor's altitude).

### The rejection log: refusals become operational evidence

**Forced by:** coverage's own legend - `constrained` was structurally unknowable because a rejection rolls back and vanishes.

**Landed:** `morpholog.rejections` - one row per refused proposal, coverage's `constrained` counted from it, `inspect rejections` to list it. Doctrine kept inline: **recorded after the rollback** (one autocommit insert, at-most-once, operational-not-legitimacy - the audit log stays the only legitimacy-grade record, "a floor, not a census"); **structure at the source, string at the boundary** (kernel `Outcome::Rejected` carries a `RejectionReason` enum whose Display *is* the pinned wire string, byte-pinned by a unit test; the table's kind/rule/version columns match the variant, never parse display text); one recording site (`finalise_outcome`, so all propose paths record for free); `constrained` is the strongest verdict, reaching always-on prohibitions invisible in committed history.

### The audit read contract: a projector leans, the tail gets blessed

**Forced by:** Glasshouse's projector began tailing `morpholog.audit` directly, the moment the embedder contract had reserved language for.

**Landed:** `inspect audit` streams NDJSON transitions, `--after <transition_id>` resumes, `--named` decodes claim arrays, the Python client grew `audit()`/`audit_named()`. **The headline is a refuted guarantee:** the first design ran under `SERIALIZABLE READ ONLY DEFERRABLE` claiming lossless resume from SSI's safe snapshot - killed because `GetSafeSnapshot` retroactively blesses the *original* snapshot, so an in-flight writer's row (sorted below already-emitted rows, since `committed_at` is transaction-start time) would be skipped forever. The replacement is a **start-time watermark** (minimum `xact_start` over open transactions, coalesced with `now()` in the same statement, computed before the snapshot); rows at or above the horizon are withheld, never lost. One opaque cursor token (a transition id, not a `committed_at,id` pair that invites hand-built timestamps); `--named` decodes claims only (arguments/intents are different vocabularies). The table contract is one paragraph teaching direct readers the same horizon-first recipe.

### The CLI meets its first stranger: propose, check --ir, and help that earns its keep

**Forced by:** reading `morpholog` with no arguments as a newcomer - the help was a wall of rustdoc, and the first external embedder regenerating its client made this the last cheap moment for from-scratch naming.

**Landed:** every command's first doc line is one plain sentence; a header and getting-started footer; journey ordering. Two renames: **`run` is now `propose`** (the kernel's own verb - a change is a proposal the rules may lawfully refuse; "run" suggested the imperative execution Morpholog exists to prevent) and **`parse` folded into `check --ir`** (a debug view now behind validation). **The rename stops at the wire:** the `run_outcome` `$defs` key and Python `parse_run_outcome` keep their names - envelope identity is contract identity.

### The unsupplied-antecedent lint: a declared-supplier smell, not a vacuity proof

**Forced by:** the authoring smell the lint tier exists to catch - declaring a predicate, referencing it in an antecedent, never giving it an admission path (a typo, or a transformation dropped in a later version).

**Landed:** a second lint occupant, `UnsuppliedAntecedent`, hint-grade. **The boundary the review tightened, kept inline:** the first draft framed this as proof an antecedent can *never* bind - it cannot, because state is admitted claims that outlive any source file (`inspect coverage` already reports transformations absent from the current programme). The honest finding is a declared-supplier authoring signal on a *fresh ledger*, not a reachability proof - the same overclaim refuted for the deferrable-snapshot guarantee, and it does *not* catch the bitemporal wound (a missing *claim*, not missing syntax). The detector lives in `analysis` (Boolean-structure evidence algebra so the diagnostic names a true cause), reuses `collect_implications`, and flags authored invariants only.

### The SQL views generator: a read contract, not a convenience dump

**Forced by:** the read side - BI and the embedder's projector want typed SQL, not hand-read positional JSONB.

**Landed:** `morpholog generate views` - a pure renderer (regenerate-and-diff golden), one typed view per base predicate. Doctrine kept inline: **read-only by construction** (each view wraps its source in a CTE, which disqualifies it from PostgreSQL auto-updatability so writes can't bypass the kernel - a read interface, not a security boundary); atomic `BEGIN; ... COMMIT;` application; **metadata-first columns** so appending a declared field is a compatible `CREATE OR REPLACE`; a hash-pinned `_morpholog_catalog`; the kind->SQL map exhaustive with no `_` arm (a new kind fails compilation). A live-DB test surfaced the real bug an offline design couldn't: jiff serialises a negative duration with a leading sign PostgreSQL's interval parser rejects, so the extractor strips and negates it. **Scope - base predicates only** (derived heads need the kernel->SQL spike the roadmap gates separately).

### The derived read cache: the kernel computes, SQL projects

**Forced by:** the obvious next step (derived-claim views) and the wall it hit - a derived value is computed by the kernel exactly, and recomputing it in SQL would force exactness-vs-coverage and commit the project to a second evaluator forever.

**Landed:** the reframe - **SQL must not be a second evaluator.** The kernel already computes derived rows via `enumerate_derived`; materialise *those* and let SQL project them. `morpholog refresh derived` and a `morpholog_read` schema kept separate from governed state. Doctrine kept inline: a read model never read by `propose` (the cache cannot become evidence); exactness by construction (tagged-JSONB byte-shaped like `morpholog.claims`); **freshness is a `source_snapshot` marker, not a high-water** (a review caught the original "never overclaims" wording as the same overclaim the audit-tail watermark exists to prevent); generation-based atomic publish; and the kernel compute held outside any transaction. This is the roadmap's "materialised derived claims" as an operational cache; the *governed* form (invalidation modelled as claims) stays a deliberate non-goal.

### Derived views: the read surface, completed

**Forced by:** the read cache existing but BI still reading its tagged JSONB by hand.

**Landed:** `generate views` now emits base predicates (over `morpholog.claims`) and derived predicates (over the `morpholog_read` cache) in one atomic script. The de-risking finding: no kind inference - a derived view's typed columns come straight from the declared head predicate through the *same* `column_sql` extractor, so the PR is a generalisation of the renderer over a second source, not a new analysis. The load-bearing decision (the plan review's must-fix): a derived view filters on the generated model hash, so a cache refreshed for a different model shows zero rows rather than mis-projecting - the difference between a correct read contract and a subtly-wrong one.

### The gate-protection map: which gate front-loads which invariant

**Forced by:** the verification arc's gap - `inspect controls` listed gates and invariant guarantees side by side but unconnected. The cross-link: this `require` is the front line for that standing rule.

**Landed:** a gate `G` of transformation `T` **front-loads** an implication invariant `A implies C` when `T` admits a predicate `A` rests on and `G` positively references a predicate `C` also references, reusing the lint tier's polarity walker. **The load-bearing decision is what the link is allowed to claim:** a syntactic correspondence through shared positive predicates, never a proof the invariant is unbreakable - so the field is named `front_loads`, not `protects` (the same boundary the unsupplied-antecedent lint was walked back to). The honest non-pairing of the quantity cap (a `sum` consequent has no positively-required predicate) falls out of the rule rather than being special-cased. The output DTOs became `#[non_exhaustive]` - the narrow right case for that deferral (evolving output structs the runtime hands out).

### The invariant-side front-line coverage: the control matrix, completed

**Forced by:** the deferral above - an auditor needs the inverse: "where is the front line for this standing rule, and where is there none?". The data already existed, so this inverts it; no new analysis.

**Landed:** `ControlMatrix::front_line_coverage`. The decisions all guard a quiet overclaim: **implication-shape granularity** (`(A implies B) and (C implies D)` with only the first front-loaded must not read as covered, so the inverse keys on the failure shape); **backstop vs dormant** (an empty `front_loaded_by` is either a transformation-can-trigger-but-no-gate backstop or a no-transformation-triggers-it dormant rule - intersecting footprints tells them apart); and honest domain in the wording (only authored implication invariants, so a prohibition a gate happens to guard is outside the view, not mislabelled a gap).

### Error/outcome surface hardening: `#[must_use]` and `thiserror`

**Forced by:** the "make Rust carry the doctrine" direction after the compile-time-SQL arc.

**Landed:** `#[must_use]` on the proposal outcomes - the value is the case where a caller handles the `Result` error arm but drops the *successful* `Outcome`, and `Rejected` is a successful outcome, so a dropped one silently treats a refused change as committed (a review subtlety: `#[must_use]` does not carry through a tuple or an awaited future, so the rejection-state path needed a named return struct). And `thiserror` for the leaf kernel errors - a mechanical lift colocating each message with its variant, messages carried over unchanged. Left manual by design: `RejectionReason`'s Display is the pinned wire string. Recorded as not done: threading `ValidatedProgram` to the commit boundary, architecturally blocked by the `'static` compensation path.

### Subtraction review: test-helper duplication, not structural churn

A workspace subtraction review found no architectural seam worth splitting in this pass. The useful duplication was in integration-test helpers (`test_pool`/`reset_db`/`expect_committed` across ~20 files), centralised into per-crate `tests/common` modules (~300 lines removed), plus a `begin_isolated_tx` helper unifying the six isolation-level sites. Larger candidates (splitting `postgres/lib.rs` / `check.rs`, unifying the IR-walker family, table-driving operator precedence) were rejected as relocating code without reducing conceptual surface - the disciplined state is the payoff of subtraction applied PR by PR, not a backlog. (It does not rule out a split a later ownership boundary creates - see `CompiledProgram` below.)

### CompiledProgram: the owned, indexed programme

**Forced by:** the cleanup arc's next step - a single validated, indexed model object to source by-name lookups from. Architectural, not performance.

**Landed:** `CompiledProgram` owns a `Program` and builds its by-name lookups once. Two decisions: it does not replace `ValidatedProgram` (the cheap borrowed `Copy` proof-of-validity the analysis API consumes - `CompiledProgram` hands one out via `validated()`, and constructing it *is* the validation gate); and indices are positions, not references (an owned struct holding `&Transformation` into its own field is self-referential). The CLI's by-name paths (`propose`, `explain`) build one; the kernel/PG signatures are untouched - this creates the owned centre of gravity, the next PR moves the orbit onto it.

### The proposal facade over CompiledProgram

PR 3 of the cleanup arc moved the runtime orbit onto that centre. The PG proposal path threaded `(transformation, transition, invariants, definitions)` as four arguments; now it takes `(pool, &compiled, &transition)`, resolving the transformation by the transition's own name (O(1)) - `PgError::UnknownTransformation` the one new error. Two boundaries held: the kernel stays decomposed (the facade lives only at the adapter), and the compensation path keeps calling private `*_inner` fns (it has no full programme to compile). `controls`/`lints`/`guarantees` also moved to `&CompiledProgram`. The cost was migrating every propose call site to build a `CompiledProgram` - which also closed a small gap where ad-hoc `ir_builder` proposals never went through validation.

### Splitting the PostgreSQL adapter into modules

Once the proposal facade settled the public surface, the grown `postgres/src/lib.rs` was split into one module per concern (`error`, `txn`, `propose`, `claims`, `audit`, `rejections`, `outbox`, `derived`, `as_of`, `verify`), `lib.rs` reduced to module declarations plus `pub use` re-exports. Pure code movement, no behaviour change - every consumer compiled untouched, the proof no public path leaked. The one risk (the compile-time SQL cache) was a non-issue: `.sqlx/` files are content-hashed by query text, not source location, so moving a `query!` between files leaves the cache valid.

### Separating structural match from environment generation in the evaluator

`unify_args` cloned the whole `base` binding map on entry, so lookup sites asking only "does this match?" cloned a map and threw it away. A private `match_args` now does the structural check and returns borrowed refs; `unify_args` clones once only on a verified match; `claim_matches` is the boolean guard that never clones - one shared core so the boolean and binding paths cannot drift. **Measured before shipping** and the honest outcome was a null result (the wasted clones are real but tiny - `base` size does not grow with state). Kept for the smaller reasons (the lookup sites read clearer, a failed match no longer clones). The episode is also the record that perf work here stays deferred until a benchmark forces it - this one did not.

### Tamper-evident audit: the Merkle history-tree foundation

**Forced by:** `verify` already caught an *uncoordinated* edit (claims and audit disagreeing) but not a *coordinated* edit of both.

**Landed:** a Crosby-Wallach / RFC 6962 (Certificate Transparency) **Merkle history tree, not a naive hash chain** - because the tree yields logarithmic inclusion and consistency proofs later, and because a chain would force every writer to read the previous row's hash (a contended chain-head, SSI aborts) whereas the tree is built entirely read-side. An `audit_checkpoints` table bounded by the resume watermark, `create_checkpoint` under `SERIALIZABLE READ ONLY DEFERRABLE`, `verify` extended to recompute every root. **The threat model is stated honestly, not oversold:** recomputing the root catches an editor who did not *also* rewrite the checkpoints; the real trust anchor is a checkpoint that has **left the database** - the load-bearing test forces a coordinated rewrite that passes bare `verify` and is caught only by the saved anchor.

### Evidence-pack export: the Merkle tree leaves the database

**Forced by:** the foundation's root was inert inside PostgreSQL - nothing outside could consume it. The embedder's standing ask, sharpened by the discovery direction: an accepted control should travel with a verifiable account of the history it was validated on.

**Landed:** `evidence export` writes a complete checkpointed prefix as JSON; `evidence verify` checks it with **no database access**. Two reuse instincts: rows carried as whole `AuditRow`s so the offline verifier recomputes each leaf with the *same* `audit_leaf_hash`, and one shared `verify_tree(leaves, checkpoints, anchor)` the live and offline verifiers both call (the parity lesson applied to tamper evidence). The sharpest point: an offline verifier consumes *untrusted* JSON, so `verify_pack` validates the envelope before the crypto core (strictly-increasing chain; rows numbering *exactly* the covering checkpoint - extra rows would ride along unproven; a malformed pack is a distinct error, never a crypto verdict). `--tree-size` means an exact existing checkpoint, never an arbitrary prefix off a later one - until consistency proofs exist a later root does not stand in for an earlier prefix. Honest boundary: a v1 pack is a complete prefix carrying the full audit data, not selective disclosure or redaction.

### Scoring a candidate against history: the evaluator pointed backward

**Forced by:** every read surface so far points forward (gate, prove, explain). `evaluate` runs the same evaluator backward - replaying the committed log under a *candidate* programme that is not deployed, reporting which already-admitted commits each candidate invariant would have refused. The fitness signal a discovery loop climbs; no kernel primitive.

**Landed:** a *sibling* of `coverage`, not a reuse - a committed rule held on every transition (coverage asks "did the antecedent fire"), but a candidate was never enforced, so the signal is exactly where history *violates* it. **The semantics is fresh violation:** a commit "would be refused" only when the post-state violates the invariant and the pre-state held (the commit introduced it) - without that a single bad subject explodes the count; the carry that makes this one evaluation per invariant per transition is valid only for state predicates, so v1 **rejects `pre(...)` candidates** rather than mis-scoring them (a pre-implementation review catch). The report carries a format version, a `semantics` tag, and the canonical `program_hash`. The marriage with the pack work landed next (`evaluate --pack`, scoring with no database, the pack *verified before scored*), then `evaluate --packs <dir>` to amortise the process-spawn tax over many cases - mechanical batching the substrate owns, while labels, fitness, and selection stay in the consumer.

### PostgreSQL 18 as the floor: a survey that forced no code

Raising the substrate floor to 18 prompted a deliberate survey of what 18 buys Morpholog. The honest finding, recorded so it is not re-surveyed: it buys real things, none needing code. **The free wins land elsewhere than the bottleneck** - async I/O and B-tree skip scan don't touch the hot path (the read path is a full-prefix index scan plus CPU-bound JSONB decode; skip scan needs an *omitted* prefix column, which we never omit), and a re-run of the bench confirmed every shape and law unchanged. `RETURNING OLD/NEW` has no site (both outbox `RETURNING`s already return only the new row). Native `uuidv7()` is refused by the same reasoning that put minting in the kernel (deterministic, seeded-reproducible, I/O-free). Temporal `WITHOUT OVERLAPS` is refused as a forbidden promotion (an invariant belongs to the kernel, not DDL); virtual generated columns deferred (we use none; derived state is kernel-owned). Net: a clean floor we benefit from by running on it.

### Signed tree heads, then keys as claims: attribution for the audit log

**Forced by:** the anchor proved *consistency* but not *attribution* - it was an unsigned hash. The `cadence` power-markets embedder forced the gap: a REMIT participant must hand a regulator a record that is complete, untampered, **and attributable**. Built as two PRs across a clean seam.

**Landed.** PR1 (substrate): a pure `signing` module (Ed25519, PKCS#8 PEM, `keygen`), the signed payload typed, length-delimited, and versioned (the DSSE pre-authentication-encoding idea, binding the bytes *and* an unambiguous payload type so a tree-head signature can never be replayed as a future artefact); checkpoints gained an optional `signatures` array (`skip_serializing_if` empty, so unsigned checkpoints stay byte-identical). PR2 (authority): a reserved `AuditSigningKey(key_id, purpose, public_key)` predicate the operator admits/retracts under its own gate (the runtime reserves only the name, as it knows `Exists`). **The load-bearing decision:** authority is resolved **as of the checkpoint's own prefix**, not current state - a key valid when a checkpoint was signed stays valid for it after later revocation, exactly like a decision that keeps standing after its authority is rescinded; one pure `authorized_keys_as_of` fold serves the live and offline verifiers so they cannot drift. The honest threat model holds: signing makes key authority governed and anchors attributable, it does not conjure a root of trust (the first authorisation is trusted the way the schema is). Deferred next: the CT proof protocol (inclusion/consistency proofs, selective disclosure, subject completeness), and key-compromise as a retroactive layer distinct from revocation.

### abs(): a surface affordance forced by a two-sided position limit

**Forced by:** the `cadence` embedder wanting to cap a signed net position to a symmetric band - `abs(net) <= limit` - where the verbose form was already legal (`(0 - limit) <= net and net <= limit`) but wrote the sum twice. Of the forms that could close the gap (unary minus, chained comparison, `abs`), `abs` is the one that removes the repeated sum; it came first.

**Landed:** a real `ValueExpr::Abs` node, not a desugaring to `max(x, 0 - x)`. The desugaring was refused because it evaluates the operand twice (the duplication moved into the IR), `0 - x` is barred on a quantity (so `abs` would silently not work on units), and with no node the form cannot round-trip or `morph fmt` back to `abs`. So `abs` is a unary, unit-preserving magnitude (decimal, quantity, duration) evaluated once, with its own `AbsKind` error. The forcing example is a net-position limit on `examples/10_trade_lifecycle/`, reusing `TradeCaptured`'s direction (`#buy`/`#sell`) and the effective-time idiom (no new quantity claim); it pins that `abs` catches the short side, not just the long. A side effect: the `limit` field name exposed that the views generator *refused* SQL reserved words - since it already double-quotes every identifier, the refusal only banned common business names, so reserved-word columns are now emitted quoted (`AS "limit"`). The remaining cadence forms (unary minus, chained comparison) stay deferred until an example forces them.

### Gateway attestation: the audit row learns how the actor was established

**Forced by:** honestly, ahead of its worked example - the deviation is owned here, and the bar it must clear is the one PR #35 set. The audit's strongest sentence about authorship was "the application asserted that Jordan did": `--actor jordan` became `Transition.actor` because the caller supplied the string, and `system_actor()` was a bare sentinel the roadmap itself called a placeholder for a lineage model. The defence for moving early is twofold: the durable wire shape (what the Merkle leaf commits to) is the expensive-to-change part and every later authentication rung lands additively on it, and the rung shipped is not speculative - it retires the placeholder with authenticated provenance available today. The signature rungs still wait for their forcing example (two-distinct-person verification is vacuous while a gateway can invent both identities).

**Landed:** every durable commit path takes a `Proposal` - transformation, args, and an `ActorAttestation` - and the adapter derives the kernel `Transition.actor` from the attestation, so actor and lineage cannot disagree by construction; the kernel and its `propose()` are untouched. Gateway mode records `{"mode":"gateway","authenticated_by":"<role>"}` where the role is `session_user` - the PostgreSQL-authenticated login role of the proposing connection, read inside the committing transaction, immune to SET ROLE, and never caller-supplied (a caller-supplied gateway identity would be two unauthenticated assertions instead of one). The lineage is inside the Merkle commitment: the leaf encoding gains an attested version carrying the tagged actor and the attestation bytes whole, and **the version a row hashes under is derived from the row's own content** - attestation absent selects the original encoding byte-for-byte (every historical root still verifies; the original encoding's bare-string actor quirk is frozen with it), present selects the attested one. A stored format column was considered and rejected: it could stamp the old version beside an attestation and leave the new field outside the hash, where content-derivation fails closed in both tamper directions (stripping flips the row to the original encoding, grafting flips an old row to the attested one - either breaks the root). The attestation object is covered as opaque bytes, so its internal shape can grow new modes without another leaf version. The one-variant `ActorAttestation` enum is wire-format prudence, not speculative API: the tagged durable shape is the part that feeds the leaf. Deliberately out, each awaiting its forcing pressure: the canonical proposal encoding, authentication-policy and actor-key claims, the authenticated-presentation record, and any attestation on receipts or the operational rejection log (whose actor deliberately keeps assertion-grade standing - stated in its rustdoc rather than papered over).

### Typed empty sums: the seed-claim ritual retired

**Forced by:** a CLI play session, not a planned example. A fresh programme with the laytime capacity shape - `sum(v | Brew(_, c, v)) <= cap` over `Decimal[L]` - detonated on the *first* commission: the empty sum evaluated to bare decimal zero, which no quantity comparison accepts, and the failure was invisible to `check`, reachable only at propose time. The sanctioned mitigation was a ritual: admit a zero-valued *seed claim* alongside the aggregate's anchor (laytime carried three - the zero-tonne parcel, the zero-length interval, the zero-dollar settlement), a pattern taught nowhere near the failure and one that conflicts with strict positivity bands (a `0 < v` rule refuses its own seed).

**Landed:** the empty sum is the typed zero of the summed variable's declared kind. The kind is static knowledge, so it resolves in a lowering pass (`lower_sum_seeds`, the disciplines precedent: runs in `parse_program`, idempotent, descends into definition calls by mapping call arguments onto parameters), stamped on the `Sum` node as a `seed` field the evaluator returns for the empty case - no evaluation-time declaration lookup, no third rules-shaped parameter on the eval entry points, no public API change. Un-lowered hand-built IR keeps the decimal default, which is the old behaviour. The dividend is subtraction: all three laytime seed claims, their transformation parameters, and the paragraphs explaining the ritual are deleted; the worked example reads the way the charterparty does. A count sum (`sum(1 | ...)`) and a variable no declaration decides stay decimal, so every pre-existing decimal aggregate is untouched.

### Chained comparisons: the range that reads as spoken

**Forced by:** the borrowing-base advance-rate band. `examples/11_borrowing_base/`'s own README described the rule as `0 <= rate <= 1`, while the `.morph` had to spell it `(0 <= rate and rate <= 1)` - the programme was the only place the rule could not be written the way everyone already said it. A doubled bound is also easy to get subtly wrong: flip one operator and the rule quietly inverts while still looking plausible.

**Landed:** a parser-only chain - `arith (cmp_op arith)+` - lowering to the `Prop::And` of pairwise `Prop::Compare` links the spelled-out form already produces. **The opposite call from `abs`, for symmetric reasons.** `abs` refused desugaring because the desugared form was *worse* than a node: it evaluated the operand twice where the author wrote it once, broke on quantities, and could not round-trip. The chain desugars because the desugared form is *identical* to what authors already write: the middle operand appears twice in the IR either way (no new duplication is introduced - `x` written once in the chain was always two comparison operands underneath), every link is an ordinary domain-checked comparison (units and dates need nothing special), and round-trip holds at the IR level because the formatter's canonical output is the expanded form - accepted chained on the way in, never re-sugared on the way out, so `explain` and every walker see a shape they already know. Guard rails: only the ordered comparators chain (each link carries its own domain), every link must point the same way (`<=`/`<` or `>=`/`>`), and a mixed-direction chain or an `=`/`!=`/`in` link is refused with a split-it-with-`and` diagnostic rather than guessed at. Unary minus, the last of the compact-bounds trio, stays deferred.

### Windowed evidence packs: the consistency-proof tier

**Forced by:** the `cadence`/REMIT embedder's reporting need - a participant hands a regulator a record for a *period*, which must prove it is a faithful continuation of the prior period, not a quietly restated history. The existing `evidence export` was complete-prefix only; its own `--tree-size` help admitted "a later checkpoint does not prove an arbitrary earlier prefix until consistency proofs exist."

**Landed:** RFC 6962 consistency and inclusion proofs in the pure `merkle` module (pinned byte-exactly to the standard's seven-input vectors), and a `WindowEvidencePack` (v2) carrying the interval `[from, to)` between two existing checkpoints, verified offline. The load-bearing doctrine - two overclaims a multi-LLM review caught before they shipped: a window pack is **not a smaller prefix pack** and proves two separate things that neither implies - a consistency proof (the later checkpoint is an append-only extension, so the prior period was not rewritten) AND a per-row inclusion proof (each exported row is the genuine suffix). A genuine consistency proof verifies between two roots *regardless of any rows*, so without inclusion the window rows are unauthenticated - pinned as a test that a tampered row fails inclusion while the consistency proof stays valid. And signing-key **authority** is a full-prefix property a window cannot establish (it lacks the `[0, from)` rows that fold `AuditSigningKey` claims), so a window checks signatures cryptographically only - "authorised as of the prefix" would be a third overclaim. Endpoints are checkpoint-to-checkpoint (dates map to a quarter-end checkpoint cadence), never arbitrary tree sizes, because an unsigned ad-hoc root is a debugging tool, not an evidence artifact. The window pack and its verdicts ride the consumed-surface contract (schema `$defs` + goldens + generated client). It proves log integrity for the interval, never *business* completeness - an unproposed reportable event is invisible to any Merkle proof - and it is the integrity/attribution artifact for a REMIT-relevant trail, not the ACER submission format.

### Worked-example accessors generated from the `.morph`

**Forced by:** a maintainability pass, not an example. Each worked example carried a hand-written `crates/morpholog-examples/src/<name>.rs` of mechanical by-name accessors (~700 lines across the gallery), plus a manual `all_programs()` list a new example had to be added to or the cross-example property tests silently skipped it.

**Landed:** a `build.rs` reads every `examples/<NN_dir>/*.morph`, extracts its `transformation` / `invariant` / `derived` declarations by a line-leading source scan, and generates the accessor module and the `all_programs()` registry into `OUT_DIR`; `lib.rs` brings each in with an `example_module!` line. Adding an example is now dropping a `.morph` plus that one line. **The decision that kept the generator simple:** scan the *source*, so it sees authored declarations only - generated discipline invariants (named at parse-time lowering, e.g. `issued_unique_by_measurement`) are not in the source, so rather than couple the generator to the lowering naming, the few accessors and the domain-symbol constants a `.morph` cannot yield are hand-written supplements in the `example_module!` body. A spike on one example surfaced both edge cases (the discipline-invariant accessor; the constants) before the sweep - the per-example `.rs` files looked purely mechanical but were not, quite.

### Train/test history splits: the overfitting guard for discovery

**Forced by:** the discovery harness. `evaluate` scored a candidate against the whole history, so a rule discovered from that history graded itself on its own answer key - "scores 99%" meant nothing until a held-out slice existed.

**Landed:** `evaluate --train-until <transition-id | timestamp>`: one continuous replay (the rule state entering the first test transition is the state the training slice built - a reset would mis-seed the fresh-violation carry), each violation attributed to the slice containing its introducing transition, the report carrying the resolved boundary so a stored experiment is self-describing. The boundary resolves inside the replay's own `SERIALIZABLE READ ONLY DEFERRABLE` snapshot (a writer still in flight can otherwise land a row at or before a timestamp boundary after resolution - the audit-tail watermark lesson, again). Pack mode resolves against the pack's canonically-ordered rows, pinned byte-equal to the live split. Refused with `--packs`: per-case packs are already the unit a harness assigns to slices. The unsplit report is byte-identical to before; the score reports joined the pinned-envelope contract in the same pass (they were the one consumed operational surface still reached by bridge).

### The sealed view surface: verify learns to see an in-place redefinition

**Forced by:** the embedder verify leg. The catalogue inventory catches a dropped or renamed view; a view *redefined in place* under the same name - same catalogue row, same model hash, different queries answered - passed everything.

**Landed:** the generated script seals itself: in the same transaction that creates the views it records each one's definition hash as PostgreSQL stores it (`pg_get_viewdef` read back, never the emitted DDL - PG's stored text is what an in-place `CREATE OR REPLACE` changes), catalogue included, into a `_morpholog_view_defs` table. `verify --views-schema` cross-checks three legs - intended inventory, seal, live views - so a redefinition is `mismatched` and a dropped view or deleted seal row is `missing`, never hidden; an unsealed surface is `not_sealed` and passes. The renderer stays pure (it only emits the SQL that does the capture at apply time), and the verdict rides the consumed-surface contract. The honest boundary: the seal lives in the same database it attests, so an attacker updating both is out of scope - this is drift evidence like the catalogue, not a root of trust; signing the surface stays deferred.

### Selective disclosure: prove what you show, show nothing else

**Forced by:** disclosure practice. Real disclosure is "show exactly what is relevant, prove it is authentic, reveal nothing else" - a participant asked to evidence three trades should not have to hand over the period's whole book. Until now the only pack shapes were the complete prefix and the whole window.

**Landed:** the v3 selective pack - a chosen subset of the rows a covering checkpoint commits to, each with the per-row inclusion proof the windowed tier already built, so the tier needed zero new cryptography; what the window envelope forbade was only contiguity. The selector is a transition-id list (`evidence export --transition`, repeatable): finding the relevant ids stays the discloser's read, which is where the relevance judgment lives - subject/predicate selector sugar waits for an embedder, deliberately, because a semantic selector tempts readers to see completeness in the selection. Unknown, duplicate, and empty selections are prover errors at export, never verifier verdicts (the verifier only sees disclosed rows); a row's position is proven by its Merkle path, never pack order (pinned by a swapped-proofs test); undisclosed rows are absent in any form (pinned byte-level against real audit rows). The honest boundaries hold: no completeness claim (subject completeness still needs a subject-indexed commitment), signature checks cryptographic only (authority is a full-prefix property), and disclosed indices reveal positions and count. The verify discriminator became exhaustive on the way: an unknown future pack version is named as too new, never misread as a malformed v1.

### The governing-selection lint: the vacuity lesson becomes a check

**Forced by:** the bitemporal-vacuity lesson turning from an anecdote into a pattern. The effective-time slice's hole - a settlement dated before the trade's first terms version silently escaping the cumulative cap - was closed by hand with a totality backstop, and every effective-dated model since re-creates the same exposure: any "the version in force at a coordinate" antecedent passes vacuously when no version exists there. With a billing engagement about to author several such rules (tariff versions, VAT rate records), the mitigation earned mechanical eyes.

**Landed:** the lint tier's third occupant. The detector recognises the bounded not-a-later-one shape in an implication antecedent - a dated claim bounded on-or-before a coordinate, plus a negated `exists` excluding a strictly later version of the same predicate, matched by variable identity so an unrelated temporal comparison is not a tiebreak - and flags it when no OTHER authored invariant carries the recognised backstop shape (an implication guaranteeing an `exists` witness of the predicate with a temporal bound). **The asymmetry a review caught before it shipped:** the first design collected selection evidence flatly (letting `or` branches combine into a pattern no branch contains) and suppressed on any positive `exists` mention (letting an optional, conditional, or undated witness silence the finding - protection in appearance only). The landed matcher keeps evidence branch-local, and suppression runs a must-guarantee algebra: `and` unions, `or` intersects, everything conditional or negative contributes nothing, an invariant never backs itself. A second review pass caught direction being discarded twice more - a forward window (candidate on-or-AFTER the coordinate) qualifying as a selection, and a future-only witness (a version guaranteed AFTER the coordinate) silencing the finding - so temporal relations are normalised to directed earlier/later sides: the window bound must establish candidate <= coordinate and the witness must sit on the earlier side, while the strict tiebreak stays direction-insensitive on purpose (within a retrospective window, the latest and the earliest selection share the vacuity hole identically). The honest boundary, same as the unsupplied-antecedent sibling: a shape smell, not a vacuity proof - coordinate agreement between selection and backstop is unverified (the verification arc's static-vacuity tier), and the unbounded "current version" spelling deliberately does not fire in v1. Example 10's shipped backstop is the fixture proving the suppression side: the cross-example clean gate stayed green untouched.

### Body `let`: naming an intermediate value without teaching the kernel about it

**Forced by:** the billing trial's rounding define. Half-away-from-zero rounding as admission law repeats `(a * b) / divisor` five times and the shifted form four more across its two sign branches, because nothing in a `define` or `invariant` body could name an intermediate value and defined-call arguments are terms-only. The repetition was not cosmetic: every copy is a chance for the copies to drift apart, in the one rule whose whole job is exactness.

**Landed:** `let name = (value)` lines opening an indented define or invariant body, desugared by substitution before the IR exists - zero kernel change, and "surface no more expressive than IR" holds by construction rather than by review. The direct property is stronger than round-trip: the sugared and hand-desugared sources parse to the *same* `Program`, so `canonical_hash` is identical and naming an intermediate value cannot change what rule the programme is. **The decisions that kept it honest:** the value's parentheses are required grammar, not style - parens already mean "layout off", so the value spans lines freely and a value ending in a bare decimal can never absorb the next line's identifier as a quantity unit (the hazard that sank the terminator-free spelling); substitution is deliberately algebraic, not hygienic - a value mentioning a claim-bound variable means whatever it means at the use site, pinned by test as intended behaviour; and every edge is refused rather than half-supported - duplicates, parameter and quantifier-binder collisions (shadowing refused, not implemented), `actor` as a name, self- and forward-references (a let may use earlier lets only), computed values in term-only positions, transitively dead bindings (liveness runs backwards so a chain dead at its tail is dead whole), and expansion past a node budget (a doubling chain grows exponentially while staying shallow - the shape the depth guard cannot see; review caught it at the plan stage). **Review caught two more before merge:** the order refusal was missing entirely - substitution order accidentally resolved a forward reference whenever the later let also appeared in the body, and reported it dead otherwise, so legality hinged on an unrelated use; and the sum target was misclassified as a binder when it is consumed against the sum body's bindings - a term-valued let now substitutes into it, a computed one gets the term-slot refusal. The same pass replaced the rewrite-every-later-value substitution loop with expand-each-value-once-against-earlier-expansions, closing a quadratic-CPU path a long one-node chain could reach without ever tripping the node budget. File-level `const` deferred until a worked example forces the programme-wide namespace.

### round(x, quantum): the payable figure becomes computable in the rule

**Forced by:** the billing trial. Money rounding as admission law was proven expressible without any primitive - one policy define, sign-branched, shift-and-remainder - and first judged sufficient. Real use reversed the judgment: the define repeats its product expression across both branches (body `let` reduced, but could not eliminate, the repetition), every programme that rounds must re-derive the sign-branch trick (whose naive single-branch form is WRONG on negatives - a ~1p upward bias a review caught and a probe confirmed), and the trial's VAT increment multiplied the copies. A convention this load-bearing deserves one spelling.

**Landed:** `ValueExpr::Round { value, quantum }` with `examples/15_metered_billing/` as the forcing example - the shape every metered supply shares (volume times rate never lands on a whole penny; the payable figure is a convention someone must own). One mode, measured, not configurable: nearest multiple of the quantum, exact halves away from zero (`0.125 -> 0.13`, `-0.125 -> -0.13` - the symmetry that keeps credit notes fair); a second policy joins as a parameter when a real bill forces it. Decimal-only in v0 under the currency-in-field-names convention; the quantity mode waits for its example. A non-positive quantum is refused by name at both tiers (literal at authoring, variable at evaluation). A dedicated node, not a desugaring, for the `abs` reasons. **What the example pins beyond the primitive:** the per-line-then-sum convention as law - two 0.4p lines total 0.00 per-line but 0.01 aggregate-rounded, and the sealed-total invariant refuses the rival figure by name - and the VAT totality pairing (the recompute rule alone passes emptily when the named rate was never declared; its companion invariant is what makes the pair govern), the vacuity lesson taught in-gallery.

### Checked arithmetic: the whole matrix honours the round() contract

**Forced by:** the round(x, quantum) review's transitive catch. Hardening the new node against a division-overflow panic prompted an empirical probe of the shipped arms - and the plain rust_decimal operators the decimal and quantity Arith arms used panic on overflow too (`8 / 1e-28`, `1e20 * 1e20`, both confirmed). Well-typed adversarial input could panic the kernel through any declared transformation, and the totality harness never saw it because its boundary witnesses stopped at zero and negative one.

**Landed:** every arithmetic site routes through checked variants - the decimal matrix via one `checked_decimal_op` helper, the quantity arms, the duration ratio, and both Sum accumulators (the realistic route: no absurd literal, just admitted amounts whose running total leaves the range). An out-of-range result is the named `EvalError::ArithOutOfRange`; zero divisors keep their own name; the three time-arm range errors that existed were reclassified from `TypeMismatch` to the same family. **The probe kept the tests honest twice:** remainder turned out to have NO reachable overflow (rust_decimal rescales internally; the first witness case chosen was refuted by being exactly divisible), so the suite pins rem's boundary exactness instead of a refusal that cannot happen. And the totality harness gained the decimal-maximum witness with a REFINED contract rather than a waived one: the named out-of-range family is lawful at extremes - checked arithmetic refusing an unrepresentable result is the contract working - while every other kernel error, and any panic, still fails the suite. No IR change, no wire change; kernel errors already travel as message strings.

### const: the figure the whole rulebook shares

**Forced by:** the billing trial's fourth-round evidence, exactly as predicted when the capability was split out of the body-`let` work: the embedder's VAT rates living as an application-side dict AND as magic decimals in the morph, and the penny quantum repeated in every rounding invariant - figures that can silently disagree, in the one file whose job is agreement. Our own gallery carried the same strain in miniature: `metered_billing.morph` spelled its quantum three times.

**Landed:** `const name = (value)` as a top-level declaration, substituted away at parse time by the body-`let` machinery generalised (one shared walker set, a diagnostic noun, and a new statement-level walker) - so the named and hand-inlined spellings are the same `Program` and the same canonical hash, proven on the retrofitted metered-billing example. What `const` adds over `let` is reach: derived clauses and transformation statements, the contexts with no local-naming alternative. **The two decisions worth recording:** collisions are refused programme-wide (a const name may not match any parameter, quantifier binder, statement binding, derived key, or body `let` anywhere - strict, and it pushes consts toward distinctive names, which is what an auditor wants from a shared figure); and the body-`let` shadowing hole was closed by plumbing rather than luck - lets substitute away before the const pass runs, so their names are carried forward specifically to refuse a local silently winning over a programme-wide name. Pattern variables deliberately stay substitutable, mirroring `let`'s algebraic doctrine. `const` joined the reserved words; the refusal vocabulary (duplicates, `actor`, earlier-only ordering, transitive deadness, computed-in-term-slot, budget) transfers verbatim from the `let` arc, review lessons included. **Review then forced the namespace doctrine tighter, and rightly:** the first cut left initialisers open (a free variable captured whichever local existed at each use site - `const uplifted = (rate + 0.01)` meant something different in every body, and `const proposer = (actor)` was not a constant at all) and let consts substitute into claim patterns (turning a distant rule's binding into a literal filter - the exact silent disagreement const exists to prevent, and a reversal of the collision-namespace promise the issue itself had made). Both closed: initialisers are literals-and-earlier-consts only (`sum`/`value` refused by the explicit decision that a state-reading figure is a rule's job), and pattern positions refuse const names outright, with constructive and resolved slots (admit/emit/retract, lookup keys, sum targets - none bind) staying ordinary uses. The body-let algebraic precedent deliberately does NOT transfer to patterns: a let is adjacent to what it rewrites; a const is not, and distance changes the risk.

### Refusal witnesses: the values, not only the rule

**Forced by:** the trial's operator question. `invariant call_amount_is_the_top_up violated` says which rule stopped a proposal and nothing about why, so the next step was always opening the database by hand. The machinery to answer it was already there and being discarded: `find_failing_subexpr` drills to the responsible sub-expression and its own doc conceded that "binding values are not substituted in v0" - the bindings were live at the failure point and thrown away on the way out.

**Landed:** one descent that carries both, so the rendered expression and the values can never disagree about which iteration was blamed; `invariant_witness` diagnoses only after `eval_invariant` has already returned false, so the accepting path pays nothing. The witness is sorted by variable because `Bindings` is a `HashMap` - unsorted, it would have flaked the byte-pinned envelopes rather than merely read untidily. Structured, not rendered into the reason: the reason string is the pinned wire format, and the whole point is that an embedder reads the offending account instead of parsing prose. `Display` is unchanged, pinned by a test that now asserts a *non-empty* witness leaves the string byte-identical.

**What the examples taught that the plan had backwards.** The plan predicted a witness would name the subject but not the figures. Metered billing showed the opposite: the refusal carries `net_gbp = 58.29` alongside the `13.5` and `431.7` it should have been computed from - enough to see the arithmetic without a database - but it cannot say which line, because that rule matches `ChargeLine(_, _, ...)` and a witness can only name what the rule binds. So a witness is exactly as informative as its rule's bindings, and wildcarding a subject has a diagnostic cost the author should know about; borrowing base carries the companion test for a rule that does bind its subject and names the facility. Two goldens, not one, so the witness-less shape stays covered: an absent witness (rather than `[]`) keeps every pre-witness envelope byte-identical, which the regeneration confirmed by changing no existing golden.

**Deliberately not in scope:** enumerating every failing witness rather than the first (an operator fixes one and re-proposes), the require-gate path (which already carries `failing_sub_expression` and `directly_missing_claims`), rendering the witness into the human stderr line, and persisting it to the rejection log - that log is an operational floor, and audit stays the only legitimacy-grade record. Carrying the offending *claim* rather than the bound variables would close the wildcard gap, and waits for a case that forces it.

### `--where`: a read narrows by argument

**Forced by:** the billing trial reading a whole predicate to find one invoice's lines. Selection stopped at predicate granularity, which the ETRM embedder had already flagged as the residual friction its README printed at the end of every run - two independent embedders reaching for the same missing filter.

**Landed:** `--where field=value` on `inspect claims` and `inspect derived`, repeatable and conjunctive, equality only. Field names resolve against the programme, so an undeclared field is a hard error naming the ones that exist - never an empty result that reads like "no such rows", which is the failure mode a filter must not have. Decimals compare as numbers (`--where net_gbp=13.5` finds a stored `13.50`; comparing the stored text would report no such row for a row that exists), and quantities and collections are refused rather than silently mismatched. **The honest boundary, stated in the release notes rather than discovered by the consumer:** this reduces transfer, not scanning. The comparison runs in the database so non-matching rows are never sent or decoded, but no index covers argument positions, so cost still follows the predicate rather than the answer.

### The aggregate `max` / `min`: a truth test becomes a key

**Forced by:** the trial's effective-date parameter, and the reframing that came with it - *remove parameters, do not add invariants*. A caller was supplying the effective date of the rate a charge should be priced at, which is a figure the record already held before the act. An invariant asking "is this the version in force?" only needs a truth, and `not (exists later: ...)` gives one; a transformation needs a *value* to look the claim up by, and `require` does not export bindings while `bind` needs a determined key. The aggregate is what turns that truth test into a key, so the parameter can be deleted rather than validated.

**Landed:** `ValueExpr::Extremum { op, value, body }` - `max(target | body)` and `min(target | body)`, deliberately spelled like `sum` because they are the same shape of thing, an aggregate over a bounded body. Note the collision of names with the two-argument `min(a, b)` / `max(a, b)` from the insurance layer: same word, different construct, distinguished by the `|`.

**The decision worth recording: an empty extremum raises rather than answering.** An empty `sum` has a typed zero, so `lower_sum_seeds` gives it one. An empty extremum has no answer at all - "the latest version in force" over nothing is not a value, and any answer we invented (a minimum date, a zero) would be a wrong one that flows silently into a `bind` key. So `EvalError::EmptyExtremum` names the aggregate and the body, and an author who means "none in force is a lawful rejection" writes the `require` in front. Restricted to ordered kinds by an allow-list (decimal, date, timestamp, duration, same-unit quantity) checked at validation, not at runtime: `UnorderedExtremum` is a modelling error, and a subject has no order to take an extremum over.

### The derived-claim refusal: a read model is not evidence

**Forced by:** the trial designing against a limitation that did not exist, then hitting one that did. `bind` on a derived claim passed `check --strict` and failed only against a live database - an hour lost to a design the checker had blessed.

**Landed:** a rule that references a derived head is refused, and the refusal is wider than the reported case: `bind`, `require`, `for`, an invariant body, `admit`, `retract`, and another derived's domain all name predicates, and a derived head is admitted by nothing. A derived output also cannot carry a claim discipline (`unique by`, `append only`, `current pointer by`): a discipline promises how governed state behaves, and a read model replaced wholesale on refresh honours none of them - refused at the declaration, where the author wrote the clause, rather than through the lowering, which would name a rule nobody typed.

**The framing that matters, and it is a limit on the claim being made:** this is a modelling rule, not a proof the reference could never match. State outlives a source file, so rows admitted under that name by an older shape of the programme may well exist. That is precisely the reason to refuse rather than an exception to it - the name would have two sources, the computed view and the stale rows. Same discipline as the lint tier's smells-not-proofs line.

### `effective by` / `total over`: time across slots, and the companion that keeps it honest

**Forced by:** the trial hand-writing "the latest version on or before D" six times - two transformations, two staleness views, and twice more in the tariff variants - each needing its own totality companion or the rule goes vacuously quiet. Their framing was better than ours: `current pointer by` governs corrections *within* a slot, `effective by ... on ...` governs time *across* slots, and the pair makes the defect **unstateable rather than merely testable**.

**Landed:** `Discipline::EffectiveBy { keys, on }` generates the in-force selector as a definition, plus the uniqueness invariant over keys-and-date that the hand-rolled version needed separately. `examples/10_trade_lifecycle` lost both its hand-rolled define and its now-redundant `unique by (trade, effective_from)` - the clause generates that invariant itself - with its tests unchanged as the equivalence proof. Two mechanisms this established for later work: `DefinitionOrigin::{Authored, Discipline}`, so the formatter, lowering idempotence and the shadowing check read provenance rather than names; and discipline lowering in **two passes**, definitions before call resolution (a call is spelled exactly like a claim reference, so a selector that does not exist yet resolves as an undeclared predicate), invariants after.

**The companion, and why it is a hint.** `invariant N total over P` says "I am what guarantees a version of `P` exists where one is needed", and an `effective by` predicate with no such declaration earns a finding. Hint, not error: a partial effective-dated predicate can be a correct model - a rule that genuinely should not apply before the first version exists is a legitimate thing to write - and `--strict` promotes it for an author who wants the pairing guaranteed rather than remembered. The declaration also settles the governing-selection lint, which had been recognising backstops by shape; declared, an unusual-but-intended backstop counts and a shape that matched by accident does not. The lint immediately flagged the gallery's own undeclared pairing, which had been carrying `settled_date_has_effective_terms` as an unstated backstop since it was written.

**Review forced two exclusions, and neither alone was enough.** Both reviewers found the same defect from different directions: the shape-recognised path had always required a *different* rule, while the declaration path flattened every marker into one global set, so a selecting invariant could satisfy its own demand. The fixes are **positional** (mirroring the shape path - this catches a hand-rolled selection that marks itself, where no generated selector exists to notice) and **selector-reading** (following definition calls - a rule that applies only where a version is in force is *no one's* companion, not merely not its own, so its declaration is dropped outright). Also from review: `total over` naming an undeclared predicate was inert metadata that travelled in the model hash while silently withdrawing the guarantee it appeared to make, now a validation error. Declined, with a test pinning the acceptance side: requiring the target to carry `effective by`. The governing-selection lint fires on hand-rolled dated selections too, so declaring the backstop for a predicate with no generated selector is a legitimate way to settle it.

### Named gates: a refusal identifies the rule, not its wording

**Forced by:** the trial twice, and the second report was the stronger one. First, they moved a unit check out of a `require` and into an invariant - not because the rule belonged there, but so a test could name it, the language distorting the model. Then, six sessions later, two of their tests passed for the *wrong* reason: a `bind` failure and a boundary-gate failure are indistinguishable to a test, so a charge that did not exist refused at the lookup while the test believed it had exercised the boundary. Their fix was to assert on a fragment of the gate's expression text, which is a test holding a variable name inside a rule. **Our own gallery carried the same strain**, documented in two test comments that said an assertion could prove "a require failed" but not which one; both had migrated to `propose_with_trace`, which answers positionally - stable under rewording, brittle under reordering.

**Landed:** an optional `name:` prefix on `require` and `bind`, carried through the rejection reason, the trace entry, the rejection log's `rule` column, `inspect controls`, and a new `rule` field on the rejected envelope. The envelope field is what makes the name reach a test rather than stopping at the reason string, and it is populated for **invariant** refusals too - so prose-parsing dies everywhere, not just for gates, and the envelope agrees with the log column.

**The decisions worth recording.** *Optional*, on a measurement: of the gallery's transformations, roughly as many have exactly one require (where the transformation name already identifies the refusal) as have two or more, so mandatory naming would have meant writing well over a hundred gallery names to buy identity for the third that needs it - and a require is a *statement*, not a declaration; invariants are versioned, audited, named by nature, while most gates are one self-evident line. *`rule` absent rather than prose-filled* when a gate has no name: that is what removes the ambiguity mandatory naming would have been protecting against, since a caller reading the field never gets a value a rewording can change. The rejection log's column keeps the rendered text for an unnamed gate - an operational floor loses nothing by being fuller than the envelope. *Unique per transformation, not per programme*, reversing our default on the consumer's argument: two acts legitimately carry the same gate verbatim, and programme-uniqueness would force meaningless suffixes. The check descends into `for` bodies, since a named gate in a loop competes for the same names.

**Deliberately not in scope:** a lint for an unnamed gate in a multi-gate transformation. It is the tier pattern `total over` established, and the evidence is not in - a large minority of gallery transformations would earn a finding, and whether that is a real smell or ceremony is what the next consumer report can settle. Naming `admit` / `retract` / `emit` is not on the table at all: they do not refuse, so they have nothing to identify.

### The SQL spike: invariant checking compiled to the substrate, measured

**Forced by:** the kernel arc's standing question - commit latency scaling with in-scope state, and value-partitioned writers anti-scaling under SSI - plus an external embedder's evidence-relation ask waiting on the same mechanism. The roadmap gated the direction on "spike and measure against the bench before committing to it"; this entry records the measurement. Branch `spike/sql-invariants` (archived, never merged) holds the code and the full verdict document with the tables.

**Measured, and the verdict is go.** The spike built a fragment compiler (denial-oriented violation queries over the claims table; the compiler doubles as the fragment classifier - whole-run refusal, interpreted fallback), a propose sequence that loads only the transformation body's read set and writes the delta first inside the SERIALIZABLE transaction (the claims table becomes the candidate state, so real indexes serve the checks), and Nicolas-style delta-substituted residual checks with partial expression indexes derived from the compiled set. Write path: warm-median propose flat at ~2ms from 1k to 100k claims, against ~1.3s interpreted; unreferenced noise invisible. Contention: 16 workers posting into ONE shared period drop from ~10.7 to ~1.19 retries/commit with throughput up from ~10 to ~2,450 commits/s - below the predicate-disjoint floor that value-level partitioning could never reach. The negative control held: a compiled WHERE with nothing to seek on keeps the relation-level SIRead lock and a flat retry rate, so compilation plus an index is the unit that pays, not compilation alone.

**The correctness argument ran as designed.** A same-snapshot differential - the kernel's verdict and both compiled stages judged inside one transaction - swept the whole-in-fragment gallery programmes with eval_totality-style argument vectors over a governed frontier: 412 probes, zero disagreements, and the census puts 74 of 99 gallery invariants inside even this deliberately minimal fragment. Case-boundedness diverges from the kernel exactly once and one-directionally, pinned by test: over dirty history (the rule-version-adoption stand-in) a non-worsening write is admitted where the kernel refuses, never the reverse. The harness is break-checked - a deliberately mis-compiled comparator reddens it.

**What the measurement taught beyond the numbers.** Two planner incidents in one spike: a JIT-compilation tax (~118ms of JIT for a sub-ms plan once cost estimates cross the threshold; the check transaction now sets `jit = off`) and an ORDER-BY-matches-primary-key plan flip that scanned a whole predicate to find nothing (fixed by ordering on the witness expressions, which also match the derived indexes). Together they prove the per-compiled-invariant plan-shape regression test is mandatory - exactly the query-planning sensitivity prior-art recorded up front. Witness identity wants the weakened contract (rule name, version, and variable set strict; values observational): symmetric self-joins and fresh-subject bodies lawfully diverge, and the strict ordering that would restore identity is the same one that baited the plan flip. The residual-risk list the real feature opens with: `pre(...)` needs two-state SQL; `Defined` inlining must keep projection dedup observable under `Sum`; or/xor multiplicity inside sums stays refused until differential-proven; checked-arithmetic parity is undecided (PG numeric is wider than the kernel's decimal, so the kernel's out-of-range error has no SQL analogue); index lifecycle belongs to the runtime end to end; and audit's `invariants_checked` needs a decided meaning for residual-skipped invariants.

### span(P3M) and date subtraction: the calendar the contract already counts in

**Forced by:** the DUoS trial retrospective, which named civil-date arithmetic the substrate's sharpest gap with two lived cases: a backbilling horizon ("twelve months before the bill") that had to be a declared date maintained from outside, and standing days ("how many days in this period") entering as the last caller-stated quantity - figures the record could compute, exactly the class Morpholog exists to make unsayable. The carrier is `examples/17_covenant_reporting/`: a facilities agreement's reporting calendar, where test dates roll three calendar months from the prior date, a compliance certificate is due within forty-five days of each period end, and an overdue notice prices default interest by the day.

**Landed:** two arithmetic rows and one literal kind, no new `ValueExpr` node. `span(P3M)` is `Value::CalendarSpan` - whole months plus whole days, the calendar-side twin of `duration(...)` - and shifts a `Date` under spelled-out semantics: months walk first with the day clamped to the destination month's last day, then days step; the walk is neither reversible nor associative around clamped month ends, and the tests pin the trap directly (two `P3M` hops from 30 November reach 28 May where one `P6M` shift keeps the 30th - the worked example's own teaching, and why its rolling invariant is named `periods_follow_three_month_anniversaries` rather than pretending to be a general quarter calendar). Date subtraction is the signed count of actual days as a decimal - the ACT numerator, so ACT/360-style fractions are plain division, while convention-specific counting (30/360) stays a contractual algorithm outside the kernel until a contract forces it. **The plan review reshaped three decisions before code.** The span grammar is Morpholog's own, not jiff's (jiff's parser accepts lowercase, signed, fractional, and time-unit forms the surface must refuse; one kernel function serves the parse diagnostic and the evaluator, so they cannot drift). A span is expression-only and *enforced* as such, not merely undeclarable: the claim, intent, derived-row, and transition-argument boundaries each refuse it by name, validation refuses a hand-built declaration carrying the kind, and the smuggling paths (an `Any` slot, a collection element) are pinned by tests. And the example was renamed from covenant *compliance* to covenant *reporting* - it governs the calendar and the certificate, deliberately not the covenant test itself, and its README says so. The refusal matrix stayed deliberately small: `Date + Duration` is a category error (exact seconds cannot shift a day-precise value), `Timestamp +/- span` needs a time zone the kernel refuses to guess, spans do not combine, order, sum, or extremise (equality alone is lawful, over the normalised value). Business-day calendars stay claims-track, unchanged. **The PR review then tightened four more:** the expression-only boundary became STATIC as well as runtime (`check` refuses a span escaping through an `Any` slot, an emit, a derived value, or a parameter whose inference lands on the span kind - an authoring mistake known from the programme must not survive `check` and become an operational proposal error); the parse-time span caps became representation-only (a span is not intrinsically out of range - whether a shift leaves the calendar depends on the date it lands on, so one `P10001Y` span lawfully crosses from the calendar's floor while `P500000M` from 2026 refuses per date); the example's Timely window gained its lower edge (a certificate cannot attest a period still running); and the fixed-quarter-ends prose was corrected - the anniversary rule cannot pin 31 Mar / 30 Jun / 30 Sep / 31 Dec, because 30 September plus three months is 30 December.

### morpholog session: the resident process, at the stdio rung

**Forced by:** the DUoS trial retrospective's operational numbers - the one limitation on its list no remodelling could dissolve. Every operation was a subprocess plus a fresh connection: ~20ms per call over their link, a 130-act seed taking minutes over a WAN, their test suite doubled to ~4 minutes, the whole presentation layer architected around one-read-per-page to ration calls. The roadmap had recorded "a long-lived propose-worker is not forced" next to the measurement that would decide otherwise; this was that measurement arriving. `propose --batch` was already ~90% of the shape - one pool, one `CompiledProgram`, receipts as the result - so the session is that loop given streaming stdin, no EOF requirement, and the read operations a page loop hits.

**Landed:** `morpholog session <file.morph>` - parse and validate once, one connection (capped at one: a lockstep protocol cannot use a second, and the cap bounds database load across many workers), then NDJSON requests answered with the already-pinned envelopes, one compact line per request, in order. Rung one serves `propose` (the batch receipt shape verbatim, per-request `explain_on_reject`) and the claims and derived reads (parameters mirroring the generated client exactly); the streaming reads stay one-shot, since the audit tail holds a read transaction and coverage a deferrable snapshot. Two new envelopes joined the triple-pin: the ready line (`model_hash` as the staleness token - the programme is pinned at start, rolling out a new model means new sessions - plus a `protocol` number distinct from the binary's version) and the coded error receipt, because a caller deciding whether a retry is safe must never parse prose (`serialization_failure` is the one re-submittable code; the batch's error classification grew kinds to feed it without changing the batch wire). Measured by the extended latency harness: one-shot ~11ms/call against a steady-state session median of ~3.5ms locally, startup paid once at ~7ms - and over a remote link the removed per-call connection handshake is worth more than the local ratio shows. **The plan review's decisive reframing:** a session is not "batch without EOF" but a stateful transport whose loss leaves a commit outcome UNKNOWN. That one distinction drove the client's whole lifecycle: the generated `Session` serialises callers with a lock, POISONS itself on any lockstep break (timeout, death, malformed or wrong-row response - a late line must never answer a newer request), and distinguishes three failures a retry policy must not conflate - a coded refusal, an operational error, and the outcome-unknown case where blind re-submission could duplicate a business action. The review also cut load-scope caching from the PR (unmeasured, and the seam is the adapter's own) and kept the wire honest in small ways: the connection string travels in the child's environment rather than a resident argv, stderr is drained so a flooding child cannot deadlock, request decoding refuses unknown fields, and the whole conversation is pinned as one golden transcript the Rust end-to-end records against a real database and the Python session tests replay from the other side. Attestation is the batch's, documented: `authenticated_by` is the session process's role, the actor stays a caller assertion, and per-caller lineage means one session per role. Deliberately deferred: retries (the caller's, as ever), correlation ids and multiplexing, socket/HTTP transport, key-sliced write routing - the `serve` rung, whose contract the session has now proven.

### if(when, then, otherwise): the value the record selects

**Forced by:** the DUoS embedder's top-ranked ask, arriving with the probe we had held the decision for. Their `define` could not export a parameter bound by equality - the collapse refused with the documented "a require match does not export its bindings" - so four caller-quantity/record-quantity line-act pairs stayed seven near-identical bodies, and every new price scope (a third had just landed) multiplied acts. Their framing was exact: either define-binding export or a conditional term does it, no preference, only the deletion count. The decisive distinction the plan review sharpened: RELATIONAL branching was already expressible (an `or` of tests over an already-bound value), but a value-PRODUCING branch was not - an act that must compute-then-admit the selected figure had no spelling, and that is the half that multiplies acts.

**Landed:** `ValueExpr::Cond`, surfaced as function-shaped `if(<prop>, <value>, <value>)` - contextual, self-delimiting, no precedence tier - with `examples/18_scoped_charges/` as the forcing example: a tariff pricing metered charges off the meter's own reading and caller-sourced ones off the proposal, one line act where there were two, the applied figure computed by the rules in the act AND re-checked by the invariant, and the wrong source's figure refused by name in both proposal orders. The semantics are the exists-test with `require`'s non-export rule (witnesses discarded), lazy branches (a caller-sourced line commits with no meter reading anywhere - the untaken lookup never evaluates), and a propagating condition error (undecidable never silently selects the fallback). Branch kinds unify beneath the equality machinery with no ordering allow-list, since selection is not ordering - subject tags are the point. **The road not taken:** Eq-as-binder was rejected because it inverts a pinned anti-capture guarantee (`define leaky(x): x = limit` errors on the unbound `limit`, deliberately - a binder `=` turns typos into silent bindings), breaks the documented Eq/Neq symmetry, and demands order-sensitive evaluability rules plus an exemption for the discipline-generated equality pairs. **The review's other correction that stuck:** a condition is not a sum body to the lint tier - it SELECTS the expected value, so a retractable pointer read only as a condition (present or under `not`) still decides what permanent history must satisfy, and the gate-vs-invariant walk now receives the condition's whole reference set, polarity-blind; a plain conditional claim still needs no totality companion, absence being the lawful other branch. Named residuals: the antecedent lints keep value position excluded, and the adapter's read footprint stays the eager union of condition and branches - laziness is an evaluation promise, not an I/O one.

### period_index(anchor, span, at): the period a date belongs to

**Forced by:** the DUoS embedder's case (c), written precisely on request the morning it shipped: distribution charging years run 1 April to 31 March, a run resolves band rates for exactly one of them, and a billing period crossing 1 April must refuse at admission - which shifts and day counts cannot express, because a day count does not identify a boundary crossing without division. Their prevalidation lived in embedder Python, governed data standing in for governable arithmetic's missing half. They offered two spellings with no preference - a `year_index` extractor or a `same_period` comparator - and the deletion count: their charging-year prevalidation plus, with monthly anchors, the calendar half of their billing-period conventions.

**Landed:** the generalised extractor, subsuming both offers and more: same-period is index equality, `year_index` is `span(P1Y)`, and same-calendar-month is a first-of-month anchor under `span(P1M)`. The semantics were the work: boundary n is computed ONCE from the original anchor with the span's components multiplied by n - never n repeated clamped hops, whose drift the civil-date entry pinned - so a 31 January anchor keeps its 31-March boundary and a leap-day anchor returns to 29 February in leap years; representable boundaries form half-open periods and a boundary beyond either calendar end acts as an infinity, CLIPPING the outermost periods so the extractor is total (the plan review supplied the reproducer where the naive two-representable-boundaries equation has no solution, and the clipped contract that fixes it). Exact anniversaries enter the new period; dates before the anchor take negative indexes; a zero span refuses by name at both tiers, the round-quantum pattern. **The review also kept the example honest twice over:** the anchor's 1 April is regulation but its year is merely an epoch - so `19_charging_years/` records `2026`, the charging year's own name, by adding the epoch year back, never a bare offset from an origin nobody regulates - and the period's inclusive-both-ends convention is stated beside the predicate, since the index-equality gate is correct for inclusive periods and wrong for half-open ones. Every semantics table row now also verifies the defining property through the IR itself (boundary(n) at-or-before the position, boundary(n+1) after it, infinities clipped), and the conformance fragment gives each of the extractor's three slots a predicate the others lack.

### Actor-assertion policy: who may speak a name

**Forced by:** the DUoS embedder's third ask, and by a vacuity in gallery 13 that had been recorded as this rung's forcing case before they raised it. Gateway attestation already recorded which login role vouched for an actor - honest lineage, and it stops there. Nothing restricted which actors a role could name, so the biometric-oversight example's Article 14(5) rule, "a decision needs two distinct verifiers", was satisfiable by one operator on one connection asserting two names in turn. The rule passed; it meant nothing. Their two-role case was the same shape arriving from production, with a constraint attached: their pens arm on first declaration, so an authentication rung had to let existing worlds stay lawful and be adopted claim by claim.

**Landed:** two reserved claims, recognised by name and all-Subject shape exactly as `AuditSigningKey` is, declared and governed by the operator's own transformations under their own authority gates. `ActorAssertionRestricted(actor)` arms an actor WHEN ADMITTED - not when declared, which is the whole backward-compatibility property: an actor with no such claim behaves as before, so adoption needs no migration. `ActorAssertionAuthority(actor, login_role)` grants one login role the right to speak one name. **They are two claims rather than one because arming by the grants would carry a silent downgrade:** retracting the last grant - what an operator does the moment they suspect trouble - would hand the name back to everybody. Here it locks the actor out, and returning to unrestricted means retracting the arming claim, a governed act on the record. A test breaks exactly that: implement the one-claim design and the lockout case fails.

The check runs inside the proposal's own SERIALIZABLE transaction, before any state is loaded or evaluated, so the policy is the one in force in the snapshot the kernel is about to use, and an unauthorised assertion leaves nothing anywhere - no audit row, no rejection-log row, no outbox row. That placement is doctrine, not economy: refusing after evaluation would let a caller manufacture a history of apparent attempts by an actor they cannot speak for. The rejection log stays a record of business refusals.

**The plan review caught the hole that mattered.** The guard was planned for the ordinary proposal path; the traced path opens its own transaction and calls the kernel itself, so `--trace` would have walked straight past it - a security gate one CLI flag from irrelevant. Both paths now open through one seam that begins the transaction, reads `session_user`, and settles the policy; the seam returns the role it checked, so the identity RECORDED in the attestation is the identity that was CHECKED, from a single read. The review also corrected the declaration hazard's direction: a misshapen declaration of a reserved name fails OPEN - unrecognised, never armed, and everything looks protected - unlike `AuditSigningKey`, whose equivalent mistake fails loudly at signing time. So the durable facades refuse such a programme themselves rather than trusting anyone to have run `check`, and both `check` and `check --json` report it.

**And it corrected the claim.** Restricting assertion binds callers who reach the record through the adapter. The runtime's writer role holds `INSERT`/`DELETE` on `morpholog.claims` and `INSERT` on `morpholog.audit`, so a compromised gateway writes claims and attestation-shaped audit rows directly and never passes the check; two verifier identities are distinct only when the two applications and their credentials are. The prose says adapter-enforced assertion policy, not proof of authorship - the same discipline the refuted deferrable-snapshot guarantee taught. `session_user` resists `SET ROLE` (pinned by a test that grants membership so the borrowing genuinely succeeds and the policy still judges the login) but not a superuser's `SET SESSION AUTHORIZATION`, which is the residue the writer-role census already accepts. **The PR review then found the example still bypassable, and the finding generalised.** The rung armed the two verifiers and left the DEPLOYER open - and every enrolment act gates on being the deployer, so a rogue application could propose as that unrestricted name, grant itself both verifier logins, and be both people again without touching SQL. The escalation was reproduced as a test before the fix, and it committed. The repair arms the deployer in the same transition that deploys the system (one transition, because arming first and granting second leaves a gap where nobody can act as the deployer at all, including to issue the grant), and the doctrine now says what the example only implied: every actor permitted to hand out authority must itself be restricted, or the restrictions beneath it are decoration. The same review also found that COMPENSATION never passes the facades' declaration check - it reaches the kernel with a decomposed transformation and no programme - so the fail-closed guarantee moved to where every durable path meets: an admitted policy claim whose shape the runtime cannot read now refuses at the authorisation seam, and a test drives a non-retryable delivery to prove the compensating transition is not written. The exhaustiveness tripwire on the session error codes was honest about its intent and dishonest in fact - a hand-kept array that would have compiled happily beside a ninth variant - so the enum and its published list are now generated from one macro list.

Deferred to later rungs, unchanged: signature mode, the canonical proposal contract, the presentation ledger, replay semantics, and purpose granularity finer than the actor. Deliberately NOT taken: a runtime rule refusing any transition that grants assertion authority unless its own actor ends the transition restricted. It would close the escalation for every future model rather than this one example, but it refuses after evaluation rather than before, and one worked example is not yet enough to justify a new refusal class - the doctrine and the regression carry it until a second model asks.
