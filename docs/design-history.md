# What the worked examples forced into the language

Design archaeology. Companion to [`scope-and-ambition.md`](scope-and-ambition.md) (forward-looking doctrine) and [`runtime-semantics.md`](runtime-semantics.md) (what the kernel means). This file records each design move and the worked example that forced it.

The entries are compressed - most a Forced-by/Landed stub, with the per-PR detail and the considered-and-rejected alternatives left to git, and the rationale that became doctrine referenced inline. The one fuller passage is the parser-arc narrative below, kept whole because it orients a reader of the parser rather than recording a single move. A new entry takes the same shape: what forced the move, then what landed and why, in a few sentences. How a PR was reviewed, and what each review changed, belongs to the PR; a rejected approach a reader might propose again stays, as one sentence.

## Pre-parser-arc IR moves (compressed)

These entries record kernel work in the order it happened. Each says what was forced and what landed.

### `Value::Subject` IR variant

**Forced by:** claim standing example. Purposes like `bank_debt_service` needed to appear as constant references in invariant bodies; pre-existing IR had only `Value::Decimal`.

**Landed:** `Value::Subject(String)` variant; matched in `unify_args` and resolved by `resolve_term`. Constant of any other kind earns its variant when a future example needs one.

### The `require`-vs-`invariant` semantic distinction

**Forced by:** claim standing example. A rule like "every X claim implies an active Y" written as an invariant would either reject revocations or cascade-retract history. Neither matches the real semantic (decisions made under valid standing remain valid even after revocation).

**Resolution:** `require` is the admission gate (checked at admission, never re-checked); `invariant` is the standing rule (checked again by every change that touches what it governs). The two answer different questions and are not interchangeable. This is the most reused doctrine in the codebase; pinned in [`runtime-semantics.md`](runtime-semantics.md).

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

Post-parser kernel and tooling moves, most a Forced-by/Landed stub (the per-PR blow-by-blow and the considered-and-rejected alternatives live in git; only the rationale that became reusable doctrine is kept inline).

### `Expr::Or` predicate-shaped disjunction

**Forced by:** chess `SingleCapturePerMove` (`after = before or after = before - 1`); landed a PR ahead of chess to keep that one on `Pre`.

**Landed:** `Expr::Or(Vec<Expr>)` mirroring `And`'s flattened shape; branch bindings concatenate without dedup. `find_failing_subexpr` returns `None` for `Or` - a failed disjunction failed every branch, so blaming one misleads.

### `Expr::Pre` transition-invariant primitive

**Forced by:** the chess transition-invariants example. State invariants (one state) and transition invariants (a pre/post pair) are distinct kinds; "MoveCount advanced by one" needs both states. Chess was chosen because no business example had operational state to compare across a transition.

**Landed:** `Expr::Pre(Box<Expr>)` - a subtree opts into pre-state evaluation; post stays default (the inverse of TLA+ priming, since Morpholog has one state at a time). `EvalError::PreStateUnavailable` is phrased about evaluation context, not AST position, so future both-state contexts share it.

### Insurance retrofit: `PolicyHeadroom` conservation via `pre(...)`

**Forced by:** closing the `Expr::Pre` PR's deferred business case with a real conservation rule (insurance, which already lives around aggregate-cap discipline).

**Landed:** `PolicyHeadroom(policy_id, remaining)` with uniqueness + existence guards; `authorise_settlement` binds, retracts, and re-asserts the decremented headroom. The transition invariant uses the sum-of-new-payments delta form, not a per-row equation - the per-row draft double-counted two same-amount settlements. Bundled the `EvalContext { state, pre_state, bindings, actor }` refactor, now honestly forced by `Pre`'s second contextual arg.

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

**Forced by:** the IR itself. The ordered comparators had grown to eight flat variants for what is structurally one operator over one of two ordered domains; every `Expr` walker paid an eight-way fan-out.

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

**Landed:** one opaque newtype per slice (`From` / `Display` / `as_str` only, no `Deref`, so call sites stay explicit): `Subject` first (deleting the runtime non-subject-actor guard), then `Var`, then `PredicateName` / `IntentName` / `TransformationName` / `InvariantName`, collapsed into one `opaque_id!` macro whose `PartialEq<str>` is the load-bearing ergonomic. The `Subject`/`actor` slices were hand-propagated by a sub-agent; the rest followed the [refactoring playbook](refactoring-playbook.md), which is the bar going forward.

### Trade lifecycle: phase as accumulated claims, forcing no primitive

**Forced by:** nothing in the kernel - the point again. The trade-lifecycle domain (capture, confirmation, price correction, settlement) was chosen as the first **external-embedder** target; the test was whether a multi-phase lifecycle could be modelled with no new IR shape.

**Landed:** `trade_lifecycle` models the phase as the accumulation of admitted claims, never a mutable status field, so as-of replay reconstructs any past phase. Official-price correction is restatement lineage: the confirmation *event* is split from the price *figure*, so a settlement under the prior figure stays standing. No declared parameter kinds were forced: every parameter flows into a typed claim position, so inference is total. The quantitative gate reuses `Compare`, the commodity-scoped authority reuses `Term::Actor`.

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

**Landed:** quantity moved onto a versioned `TradeTerms(trade, version, quantity, period, effective_from)`; `amend_trade_terms` admits a later version effective from a possibly-backdated date, the original staying on record. **Forced no primitive - the headline.** "Terms in force on `d`" is the *not-a-later-one* pattern built from existing constructs, and the cap became a running total by effective date. Bitemporality is *demonstrated, not built*: the valid axis is dates on claims, the transaction axis is the replayed audit log - no temporal-database columns, the floor the prior-art doc draws against XTDB-as-substrate.

### `IntentDecl` and `PredicateDecl`: structurally similar, deliberately not unified

**Forced by:** the question whether `PredicateDecl` and `IntentDecl` should collapse into one `Decl` (both carry only a name and a `Vec<ArgDecl>`).

**Landed:** kept two. The name fields use distinct opaque newtypes, so a generic `Decl<N>` would never range over both, and a `Decl { name: String }` would erase the distinction the kernel just paid to add. The two vocabularies describe distinct kinds of thing (admitted claim shapes versus outbox effect shapes). The two-struct shape carries no maintenance tax unifying would remove.

### The first embedder-facing kernel surface: per-transformation argument schemas

**Forced by:** the external ETRM embedder consuming the model from outside the workspace. The smallest honest surface is "given a validated programme and a transformation name, tell me what the outside world must supply."

**Landed:** `analysis::transformation_param_kinds` (the resolved input contract) and a `schema::transformation_arg_schema` JSON Schema adapter - **the kernel exports the inferred input contract, not a client protocol**. Forced no IR change: every `trade_lifecycle` parameter flows into a typed position, so inference is total. The accessor uses a *sibling* walker that never clones scope (a parameter used only inside `require` is still externally supplied), and `ParamKind` keeps `Concrete`/`Polymorphic`/`Unconstrained`/`Ambiguous` distinct, because collapsing the multi-kind case to one kind would make the schema lie about disjunctive-input transformations.

### `Value::Timestamp` and `Value::Duration`: the time arc opens

**Forced by:** `examples/12_laytime_demurrage/` - laytime is a time allowance, demurrage the priced excess, the Statement of Facts a minute-by-minute log. Modelling it needs exact instants, exact spans, instants shifting by and differencing into spans - none expressible over `Decimal` and civil `Date` without lying about the values.

**Landed:** `Value::Timestamp` (RFC 3339, `@...T14:00:00Z`) and `Value::Duration` (ISO 8601, `duration(PT6H)`), backed by `jiff`, with an arithmetic rule matrix enforced twice (`NoArithRule` at authoring, refused again at eval); `Sum` became type-driven. **Zone-less by design:** a `Timestamp` is an exact UTC instant; local calendars enter as admitted `PortDay` claims from an external authority, never a runtime tzdb, because a computation depending on which tzdb shipped cannot honestly replay. `SignedDuration` not `Span` (exact seconds only). Demurrage is *not* an invariant - running over the allowance is the normal priced outcome, so the excess is a derived claim floored at zero.

### `Value::Quantity` and `PredicateArgKind::Quantity(Unit)`: units land as labels

**Forced by:** the laytime example's third stage - cargo in tonnes against capacity, demurrage in dollars against what the delay is worth.

**Landed:** **units are contractual labels on exact decimals, not physical dimensions.** A `Decimal[USD]` is an exact decimal whose declared unit must match before arithmetic; no registry, no SI semantics, no compound symbols (`USD/day` is a field name and a formula, never a unit). Conversions are domain knowledge with provenance and time, so they enter as claims when an example forces one - the doctrine that keeps tzdb lookups out. The algebra is the smallest forced (same-unit add/sub/min/max/sum/compare, bare-decimal scaling, same-unit ratio, `Duration / Duration -> Decimal`). A unit can be declared, never inferred; the wire carries the bare amount; diagnostics always print `Decimal[USD]`.

### Example 13 (EU AI Act Article 12) and `inspect controls`: the zero-kernel statute

**Forced by:** Articles 12 and 14(5) of Regulation (EU) 2024/1689 - record-keeping and two-distinct-person verification as admission law. Its headline is what it forced: **nothing in the kernel or the surface.** Authority grant/revoke, validity windows, standing-by-verification, and exact instants composed directly into a statute - evidence that the governance patterns are domain-independent.

**Landed:** the two-person rule is deliberately *both* a gate and an invariant (the authority check in the gate so revoking oversight never reaches back; the distinctness requirement an invariant, sound only because verifications are append-only). No clock (timestamps supplied, never read), so the record replays identically. **`inspect controls`**, forced by the example's own statute-clause-to-rule mapping table: a control matrix pairing each transformation's `require`/`bind` preconditions with the invariant guarantees - the gate-side complement to `inspect guarantees`, pure rendering over the IR.

### `define`: named propositions, the language's second authoring tier

**Forced by:** the worked examples three ways at once - the clinical-trial gate (a long conjunction with no way to name its sub-conditions), the AI Act's two-verifiers proposition carried verbatim in two places, and the trade lifecycle's "terms in force on a date". One missing construct: declare a recurring condition once, under the business's own name.

**Landed:** `Definition { name, parameters, body }`, called as `Prop::Defined { name, args }`, surface `define name(params): body`, resolved by name after the whole programme is collected. **Relational substitution with projection, not macro expansion and not a closure:** the body sees only its parameters, and projection deduplicates. Bodies are context-free (`actor`/`pre(...)` inside are errors). **Every walker stays transitive through `Defined`**, with one canonical `definitions` module owning expansion. A definition edit changes invariant meaning without changing invariant text, so the programme hash, not the invariant list, names the rules in force.

### Claim disciplines: the doctrine moves onto the predicate declarations

**Forced by:** a census that found many hand-written uniqueness invariants, re-derived currentness pointers, and - the sharpest finding - models whose invariant-safety *rested* on append-only-ness asserted only in comments. Doctrine the runtime could not see.

**Landed:** the declaration clauses `unique by`, `append only`, `current pointer by` and `superseded via`, lowered to generated invariants where a state rule is needed, enforced statically where cheaper. `unique by` is full agreement (the SQL-UNIQUE reading); `append only` is a static retract-ban, free at runtime; `current pointer by` IS `unique by` plus the retractable-pointer class; `superseded via` is no-fork only. Generated invariants are *prepended*, so a root-cause rejection names the discipline, and carry `from:` provenance; their boring names are load-bearing. Rejected: disciplines as a general rule-template mechanism - they are properties of claim shapes only.

### The lint tier, opened by the gate-vs-invariant hint

**Forced by:** the disciplines PR's own promise - declaring append-only and current-pointer classes made the oldest doctrine in the repo (authority/currentness belong in gates, never invariants) mechanically nameable.

**Landed:** a `Lint` surface distinct from `ValidationError` on purpose - an error means the programme cannot mean what it says; a lint means it says something usually-but-not-always a mistake. `check` prints hints to stderr and passes; `--strict` promotes; stdout stays silent. The first lint is **forward direction only** and polarity-aware, descending through defined calls; it is a hint not an error because continuous-compliance is a legitimate reading (the KYC onboarding shape stays clean precisely because its pointer is not declared append-only). Every worked example is lint-clean, pinned by a cross-example test.

### `run --batch`: the import shape, and an exit code that tells the truth

**Forced by:** the external embedder's import path (rows in, a receipt per row, each its own SERIALIZABLE commit) and the first throughput lever the bench's contend axis names.

**Landed:** NDJSON rows naming their own transformation and actor, decoded by the same codecs through the same adapter calls into the same envelopes - plus a `row` number, so the batch contract cannot drift from the single-run contract. A malformed row is a receipt, not a process failure (the rows after a corrupt line still run), and **the exit code tells the import's truth** - zero whenever every row was processed (partial admission is an import's normal outcome), non-zero reserved for operational failure. The contrast with single `run` (exit 1 on rejection) is documented, not smoothed over.

### Source-located diagnostics: every finding points at the line

**Forced by:** the asymmetry every author hit - parse errors arrived with ariadne carets while validation errors, lint hints, and runtime rejections said *what* but never *where*. The Glasshouse authoring workflow (models far larger than the worked examples) made the decision due.

**Landed:** spans survive parsing in a `SourceMap` returned beside the IR (`parse_program` stays the discarding wrapper). **The map lives beside the IR, never inside it** - the kernel stays source-agnostic, carrying only a statement *index* the surface resolves to a span. Declaration + top-level-statement granularity; unresolvable findings degrade to a plain line, never block; the runtime rejection location is a stderr courtesy, leaving every pinned stdout envelope byte-identical.

### The client is a projection too: `generate python-client` and `schema --result`

**Forced by:** convergence - two independent Python embedders hand-wrote the same codecs, envelope models, subprocess adapter, and manifest-driven models, none embedder-specific, all able to drift. `.morph` is the truth and the client was the one projection still hand-maintained downstream.

**Landed:** `morpholog generate python-client` plus the `schema --result` envelope contract it consumes. **Static templates are emitted verbatim, only models generated** (the tested file IS the emitted file). **One sample set holds three artefacts together:** goldens byte-equal to real serialization, validated against `result.json`, loaded by the client's own tests, so binary, schema and client cannot drift apart without a test naming it. Stdlib-only with a CI-enforced 3.10 floor. **Whole-run refusal:** non-concrete kinds, Duration/Collection/Any and keyword names are all listed and nothing is written.

### inspect coverage: which of these rules has ever actually done work?

**Forced by:** the bitemporal-vacuity lesson made real (an effective-time invariant went silently vacuous when no governing claim existed), and every literature survey ranked rule-quality reporting high-leverage. The verification arc's opener, kernel-free.

**Landed:** `morpholog inspect coverage` - the audit log replayed through a coverage tracker. **The verdicts are bounded by what committed history can honestly show, and the report says so:** *fired*, *never fired* (the headline), *always on* (prohibitions, whose work is invisible); *constrained* awaits the rejection log. The shape classifier reuses the lint tier's polarity walker (transitive through definitions); the delta prune is the cost model, but `pre(...)` antecedents are exempt because their firing lags the delta by one transition. Reads under `SERIALIZABLE READ ONLY DEFERRABLE`. Counts transitions, not binding witnesses.

### The rejection log: refusals become operational evidence

**Forced by:** coverage's own legend - `constrained` was structurally unknowable because a rejection rolls back and vanishes.

**Landed:** `morpholog.rejections` - one row per refused proposal, coverage's `constrained` counted from it, `inspect rejections` to list it. **Recorded after the rollback:** one autocommit insert, at-most-once, operational not legitimacy - the audit log stays the only legitimacy-grade record, "a floor, not a census". **Structure at the source, string at the boundary:** `RejectionReason`'s Display *is* the pinned wire string, and the table's kind/rule/version columns match the variant, never parse display text. One recording site (`finalise_outcome`), so every propose path records for free. `constrained` is the strongest verdict, reaching always-on prohibitions invisible in committed history.

### The audit read contract: a projector leans, the tail gets blessed

**Forced by:** Glasshouse's projector began tailing `morpholog.audit` directly, the moment the embedder contract had reserved language for.

**Landed:** `inspect audit` streams NDJSON transitions, `--after <transition_id>` resumes, `--named` decodes claim arrays only, and the Python client grew `audit()`/`audit_named()`. Lossless resume rests on a **start-time watermark** (minimum `xact_start` over open transactions, coalesced with `now()` in the same statement, computed before the snapshot): rows at or above the horizon are withheld, never lost. One opaque cursor token, a transition id. Rejected: `SERIALIZABLE READ ONLY DEFERRABLE` as the resume guarantee, because `GetSafeSnapshot` retroactively blesses the original snapshot, so an in-flight writer's row would be skipped forever.

### The CLI meets its first stranger: propose, check --ir, and help that earns its keep

**Forced by:** reading `morpholog` with no arguments as a newcomer - the help was a wall of rustdoc, and the first external embedder regenerating its client made this the last cheap moment for from-scratch naming.

**Landed:** every command's first doc line is one plain sentence; a header and getting-started footer; journey ordering. **`run` is now `propose`** (a change is a proposal the rules may lawfully refuse; "run" suggested the imperative execution Morpholog exists to prevent) and **`parse` folded into `check --ir`**. **The rename stops at the wire:** the `run_outcome` `$defs` key and Python `parse_run_outcome` keep their names - envelope identity is contract identity.

### The unsupplied-antecedent lint: a declared-supplier smell, not a vacuity proof

**Forced by:** the authoring smell the lint tier exists to catch - declaring a predicate, referencing it in an antecedent, never giving it an admission path (a typo, or a transformation dropped in a later version).

**Landed:** a second lint occupant, `UnsuppliedAntecedent`, hint-grade: a declared-supplier authoring signal on a fresh ledger, not proof an antecedent can never bind - state is admitted claims that outlive any source file. It does *not* catch the bitemporal wound (a missing *claim*, not missing syntax). The detector lives in `analysis` (Boolean-structure evidence algebra so the diagnostic names a true cause), reuses `collect_implications`, and flags authored invariants only.

### The SQL views generator: a read contract, not a convenience dump

**Forced by:** the read side - BI and the embedder's projector want typed SQL, not hand-read positional JSONB.

**Landed:** `morpholog generate views` - a pure renderer (regenerate-and-diff golden), one typed view per base predicate. **Read-only by construction:** each view wraps its source in a CTE, which disqualifies PostgreSQL auto-updatability, so writes can't bypass the kernel - a read interface, not a security boundary. Atomic `BEGIN; ... COMMIT;` application; metadata-first columns, so appending a declared field is a compatible `CREATE OR REPLACE`; a hash-pinned `_morpholog_catalog`; the kind->SQL map exhaustive with no `_` arm. jiff's signed negative durations, which PostgreSQL's interval parser rejects, are stripped and negated by the extractor. Base predicates only.

### The derived read cache: the kernel computes, SQL projects

**Forced by:** the obvious next step (derived-claim views) and the wall it hit - a derived value is computed by the kernel exactly, and recomputing it in SQL would force exactness-vs-coverage and commit the project to a second evaluator forever.

**Landed:** **SQL must not be a second evaluator.** The kernel already computes derived rows via `enumerate_derived`; `morpholog refresh derived` materialises those into a `morpholog_read` schema kept separate from governed state. A read model never read by `propose`, so the cache cannot become evidence; exact by construction (tagged JSONB shaped like `morpholog.claims`); freshness is a `source_snapshot` marker, not a high-water; generation-based atomic publish, with the kernel compute held outside any transaction. The *governed* form (invalidation modelled as claims) stays a deliberate non-goal.

### Derived views: the read surface, completed

**Forced by:** the read cache existing but BI still reading its tagged JSONB by hand.

**Landed:** `generate views` now emits base predicates (over `morpholog.claims`) and derived predicates (over the `morpholog_read` cache) in one atomic script. No kind inference: a derived view's typed columns come straight from the declared head predicate through the *same* `column_sql` extractor, so it generalises the renderer over a second source. A derived view filters on the generated model hash, so a cache refreshed for a different model shows zero rows rather than mis-projecting.

### The gate-protection map: which gate front-loads which invariant

**Forced by:** the verification arc's gap - `inspect controls` listed gates and invariant guarantees side by side but unconnected. The cross-link: this `require` is the front line for that standing rule.

**Landed:** a gate `G` of transformation `T` **front-loads** an implication invariant `A implies C` when `T` admits a predicate `A` rests on and `G` positively references a predicate `C` also references, reusing the lint tier's polarity walker. It is a syntactic correspondence through shared positive predicates, never a proof the invariant is unbreakable, so the field is `front_loads`, not `protects`. The quantity cap's honest non-pairing (a `sum` consequent has no positively-required predicate) falls out of the rule rather than being special-cased. The output DTOs became `#[non_exhaustive]`.

### The invariant-side front-line coverage: the control matrix, completed

**Forced by:** the deferral above - an auditor needs the inverse: "where is the front line for this standing rule, and where is there none?". The data already existed, so this inverts it; no new analysis.

**Landed:** `ControlMatrix::front_line_coverage`. The decisions all guard a quiet overclaim: **implication-shape granularity** (`(A implies B) and (C implies D)` with only the first front-loaded must not read as covered, so the inverse keys on the failure shape); **backstop vs dormant** (an empty `front_loaded_by` is either a transformation-can-trigger-but-no-gate backstop or a no-transformation-triggers-it dormant rule - intersecting footprints tells them apart); and honest domain in the wording (only authored implication invariants, so a prohibition a gate happens to guard is outside the view, not mislabelled a gap).

### Error/outcome surface hardening: `#[must_use]` and `thiserror`

**Forced by:** the "make Rust carry the doctrine" direction after the compile-time-SQL arc.

**Landed:** `#[must_use]` on the proposal outcomes: `Rejected` is a successful outcome, so a dropped one silently treats a refused change as committed. `#[must_use]` does not carry through a tuple or an awaited future, so the rejection-state path got a named return struct. `thiserror` for the leaf kernel errors, messages unchanged. Left manual by design: `RejectionReason`'s Display is the pinned wire string. Not done: threading `ValidatedProgram` to the commit boundary, blocked by the `'static` compensation path.

### Subtraction review: test-helper duplication, not structural churn

A workspace subtraction review found no architectural seam worth splitting in this pass. The useful duplication was in integration-test helpers (`test_pool`/`reset_db`/`expect_committed` across many files), centralised into per-crate `tests/common` modules, plus a `begin_isolated_tx` helper unifying the isolation-level sites. Larger candidates (splitting `postgres/lib.rs` / `check.rs`, unifying the IR-walker family, table-driving operator precedence) were rejected as relocating code without reducing conceptual surface. (It does not rule out a split a later ownership boundary creates - see `CompiledProgram` below.)

### CompiledProgram: the owned, indexed programme

**Forced by:** the cleanup arc's next step - a single validated, indexed model object to source by-name lookups from. Architectural, not performance.

**Landed:** `CompiledProgram` owns a `Program` and builds its by-name lookups once. It does not replace `ValidatedProgram` (the cheap borrowed `Copy` proof-of-validity the analysis API consumes - `CompiledProgram` hands one out via `validated()`, and constructing it *is* the validation gate), and its indices are positions, not references (an owned struct holding `&Transformation` into its own field is self-referential). The CLI's by-name paths (`propose`, `explain`) build one; the kernel/PG signatures are untouched.

### The proposal facade over CompiledProgram

The PG proposal path threaded `(transformation, transition, invariants, definitions)` as four arguments; now it takes `(pool, &compiled, &transition)`, resolving the transformation by the transition's own name - `PgError::UnknownTransformation` the one new error. The kernel stays decomposed (the facade lives only at the adapter), and the compensation path keeps calling private `*_inner` fns (it has no full programme to compile). `controls`/`lints`/`guarantees` also moved to `&CompiledProgram`. Migrating every propose call site to build a `CompiledProgram` also closed a gap where ad-hoc `ir_builder` proposals never went through validation.

### Splitting the PostgreSQL adapter into modules

Once the proposal facade settled the public surface, the grown `postgres/src/lib.rs` was split into one module per concern (`error`, `txn`, `propose`, `claims`, `audit`, `rejections`, `outbox`, `derived`, `as_of`, `verify`), `lib.rs` reduced to module declarations plus `pub use` re-exports. Pure code movement, no behaviour change - every consumer compiled untouched, the proof no public path leaked. The one risk (the compile-time SQL cache) was a non-issue: `.sqlx/` files are content-hashed by query text, not source location, so moving a `query!` between files leaves the cache valid.

### Separating structural match from environment generation in the evaluator

`unify_args` cloned the whole `base` binding map on entry, so lookup sites asking only "does this match?" cloned a map and threw it away. A private `match_args` now does the structural check and returns borrowed refs; `unify_args` clones once only on a verified match; `claim_matches` is the boolean guard that never clones - one shared core so the boolean and binding paths cannot drift. **Measured before shipping:** a null result (the wasted clones are real but tiny - `base` size does not grow with state). Kept for the smaller reasons (the lookup sites read clearer, a failed match no longer clones). Perf work here stays deferred until a benchmark forces it.

### Tamper-evident audit: the Merkle history-tree foundation

**Forced by:** `verify` already caught an *uncoordinated* edit (claims and audit disagreeing) but not a *coordinated* edit of both.

**Landed:** a Crosby-Wallach / RFC 6962 **Merkle history tree, not a naive hash chain** - the tree yields logarithmic inclusion and consistency proofs, and a chain would force every writer to read the previous row's hash (a contended chain-head, SSI aborts) whereas the tree is built entirely read-side. An `audit_checkpoints` table bounded by the resume watermark, `create_checkpoint` under `SERIALIZABLE READ ONLY DEFERRABLE`, `verify` extended to recompute every root. **The threat model:** recomputing the root catches an editor who did not *also* rewrite the checkpoints; the real trust anchor is a checkpoint that has **left the database**, pinned by a test where a coordinated rewrite passes bare `verify` and is caught only by the saved anchor.

### Evidence-pack export: the Merkle tree leaves the database

**Forced by:** the foundation's root was inert inside PostgreSQL - nothing outside could consume it. An accepted control should travel with a verifiable account of the history it was validated on.

**Landed:** `evidence export` writes a complete checkpointed prefix as JSON; `evidence verify` checks it with **no database access**. Rows travel as whole `AuditRow`s, so the offline verifier recomputes each leaf with the same `audit_leaf_hash`, and the live and offline verifiers share one `verify_tree`. An offline verifier consumes *untrusted* JSON, so `verify_pack` validates the envelope before the crypto core, and a malformed pack is a distinct error, never a crypto verdict. `--tree-size` means an exact existing checkpoint, never an arbitrary prefix off a later one. A v1 pack is a complete prefix, not selective disclosure.

### Scoring a candidate against history: the evaluator pointed backward

**Forced by:** every read surface so far points forward (gate, prove, explain). `evaluate` runs the same evaluator backward - replaying the committed log under a *candidate* programme that is not deployed, reporting which already-admitted commits each candidate invariant would have refused. The fitness signal a discovery loop climbs; no kernel primitive.

**Landed:** a *sibling* of `coverage`, not a reuse: a candidate was never enforced, so the signal is exactly where history *violates* it. **The semantics is fresh violation:** a commit "would be refused" only when the post-state violates the invariant and the pre-state held; without that a single bad subject explodes the count. That carry is valid only for state predicates, so `pre(...)` candidates are rejected rather than mis-scored. The report carries a format version, a `semantics` tag and the canonical `program_hash`. `evaluate --pack` scores with no database, the pack verified before scored, and `evaluate --packs <dir>` batches many cases; labels, fitness and selection stay in the consumer.

### PostgreSQL 18 as the floor: a survey that forced no code

Raising the substrate floor to 18 prompted a survey of what 18 buys Morpholog, recorded so it is not re-surveyed: real things, none needing code. Async I/O and B-tree skip scan don't touch the hot path (the read path is a full-prefix index scan plus CPU-bound JSONB decode; skip scan needs an *omitted* prefix column, which we never omit), and a bench re-run confirmed every shape unchanged. `RETURNING OLD/NEW` has no site. Native `uuidv7()` is refused for the reasons that put minting in the kernel (deterministic, seeded-reproducible, I/O-free). Temporal `WITHOUT OVERLAPS` is refused (an invariant belongs to the kernel, not DDL); virtual generated columns deferred (we use none).

### Signed tree heads, then keys as claims: attribution for the audit log

**Forced by:** the anchor proved *consistency* but not *attribution* - it was an unsigned hash. The `cadence` power-markets embedder forced the gap: a REMIT participant must hand a regulator a record that is complete, untampered, **and attributable**.

**Landed:** a pure `signing` module (Ed25519, PKCS#8 PEM, `keygen`) with a typed, length-delimited, versioned signed payload (DSSE-style), so a tree-head signature can never be replayed as another artefact; checkpoints gained an optional `signatures` array, unsigned ones byte-identical. Then a reserved `AuditSigningKey(key_id, purpose, public_key)` predicate the operator admits and retracts under its own gate. Authority is resolved **as of the checkpoint's own prefix**: a key valid when a checkpoint was signed stays valid for it after later revocation. One `authorized_keys_as_of` fold serves the live and offline verifiers. Signing makes key authority governed; it does not conjure a root of trust. Not done: key compromise as a retroactive layer distinct from revocation.

### abs(): a surface affordance forced by a two-sided position limit

**Forced by:** the `cadence` embedder wanting to cap a signed net position to a symmetric band - `abs(net) <= limit` - where the verbose form was already legal but wrote the sum twice. Of the forms that could close the gap (unary minus, chained comparison, `abs`), `abs` is the one that removes the repeated sum.

**Landed:** a real `ValueExpr::Abs` node, a unary unit-preserving magnitude (decimal, quantity, duration) evaluated once, with its own `AbsKind` error. Rejected: desugaring to `max(x, 0 - x)`, which evaluates the operand twice, fails on quantities (`0 - x` is barred there) and cannot round-trip back to `abs`. The forcing example is a net-position limit on `examples/10_trade_lifecycle/`, pinning that `abs` catches the short side. Its `limit` field showed the views generator refusing SQL reserved words needlessly; since every identifier is already double-quoted, they are now emitted quoted (`AS "limit"`).

### Gateway attestation: the audit row learns how the actor was established

**Forced by:** ahead of its worked example, owned as the PR #35 class of deviation: the audit's strongest sentence about authorship was "the application asserted that Jordan did", and `system_actor()` was a placeholder the roadmap itself named. The durable wire shape is the expensive part; every later authentication rung lands additively on it.

**Landed:** every durable commit path takes a `Proposal` (transformation, args, `ActorAttestation`); the adapter derives `Transition.actor` from the attestation so actor and lineage cannot disagree; the kernel is untouched. Gateway mode records `{"mode":"gateway","authenticated_by":<session_user>}`, read inside the committing transaction, immune to `SET ROLE`, never caller-supplied. The lineage is inside the Merkle leaf, and the leaf version is derived from the row's own content, so every historical root still verifies. Rejected: a stored format column, which could stamp the old version beside an attestation and leave the field outside the hash. The attestation is covered as opaque bytes, so new modes need no new leaf version.

### Typed empty sums: the seed-claim ritual retired

**Forced by:** a CLI play session. A fresh programme with the laytime capacity shape - `sum(v | Brew(_, c, v)) <= cap` over `Decimal[L]` - failed on its *first* commission: the empty sum evaluated to bare decimal zero, which no quantity comparison accepts, invisible to `check`. The mitigation was a ritual of zero-valued *seed claims*, taught nowhere near the failure and in conflict with strict positivity bands.

**Landed:** the empty sum is the typed zero of the summed variable's declared kind, resolved statically by a lowering pass (`lower_sum_seeds`, run in `parse_program`, idempotent, descending into definition calls) that stamps a `seed` on the `Sum` node - no evaluation-time declaration lookup and no public API change. The laytime seed claims, their parameters and the ritual's prose are deleted. A count sum (`sum(1 | ...)`) and a variable no declaration decides stay decimal.

### Chained comparisons: the range that reads as spoken

**Forced by:** the borrowing-base advance-rate band. `examples/11_borrowing_base/`'s README described the rule as `0 <= rate <= 1`, while the `.morph` had to spell it `(0 <= rate and rate <= 1)`; a doubled bound is also easy to get subtly wrong.

**Landed:** a parser-only chain - `arith (cmp_op arith)+` - lowering to the `Prop::And` of pairwise `Prop::Compare` links. The opposite call from `abs`, for symmetric reasons: the desugared form is *identical* to what authors already write, every link is an ordinary domain-checked comparison, and the formatter's canonical output is the expanded form (accepted chained, never re-sugared). Only the ordered comparators chain, every link must point the same way, and a mixed-direction chain or an `=`/`!=`/`in` link is refused with a split-it-with-`and` diagnostic. Unary minus stays deferred.

### Windowed evidence packs: the consistency-proof tier

**Forced by:** the `cadence`/REMIT embedder's reporting need - a participant hands a regulator a record for a *period*, which must prove it is a faithful continuation of the prior period, not a quietly restated history. `evidence export` was complete-prefix only.

**Landed:** RFC 6962 consistency and inclusion proofs in the pure `merkle` module (pinned to the standard's vectors), and a `WindowEvidencePack` (v2) covering `[from, to)` between two existing checkpoints, verified offline. A window pack proves two separate things: a consistency proof (the prior period was not rewritten) AND a per-row inclusion proof (each exported row is genuine), since consistency alone authenticates no rows. Signing-key authority is a full-prefix property, so a window checks signatures cryptographically only. Endpoints are checkpoints, never arbitrary tree sizes. It proves log integrity for the interval, never business completeness, and it is not the ACER submission format.

### Worked-example accessors generated from the `.morph`

**Forced by:** a maintainability pass, not an example. Each worked example carried a hand-written `crates/morpholog-examples/src/<name>.rs` of mechanical by-name accessors, plus a manual `all_programs()` list a new example had to be added to or the cross-example property tests silently skipped it.

**Landed:** a `build.rs` scans every `examples/<NN_dir>/*.morph` for line-leading `transformation` / `invariant` / `derived` declarations and generates the accessor module and the `all_programs()` registry into `OUT_DIR`; `lib.rs` brings each in with an `example_module!` line. Scanning the *source* keeps the generator simple: it sees authored declarations only, so the few accessors a `.morph` cannot yield (a generated discipline invariant, domain-symbol constants) are hand-written supplements in the `example_module!` body rather than coupling the generator to the lowering's naming.

### Train/test history splits: the overfitting guard for discovery

**Forced by:** the discovery harness. `evaluate` scored a candidate against the whole history, so a rule discovered from that history graded itself on its own answer key.

**Landed:** `evaluate --train-until <transition-id | timestamp>`: one continuous replay (a reset would mis-seed the fresh-violation carry), each violation attributed to the slice containing its introducing transition, the resolved boundary in the report. The boundary resolves inside the replay's own `SERIALIZABLE READ ONLY DEFERRABLE` snapshot, so an in-flight writer cannot land a row on the wrong side of it. Pack mode resolves against the pack's canonically-ordered rows, pinned byte-equal to the live split. Refused with `--packs`: per-case packs are already the unit a harness assigns to slices. The unsplit report is byte-identical, and the score reports joined the pinned-envelope contract.

### The sealed view surface: verify learns to see an in-place redefinition

**Forced by:** the embedder verify leg. The catalogue inventory catches a dropped or renamed view; a view *redefined in place* under the same name passed everything.

**Landed:** the generated script seals itself: in the transaction that creates the views it records each one's definition hash as PostgreSQL stores it (`pg_get_viewdef`, never the emitted DDL) into a `_morpholog_view_defs` table. `verify --views-schema` cross-checks inventory, seal and live views, so a redefinition is `mismatched`, a dropped view or deleted seal row `missing`, and an unsealed surface `not_sealed`, which passes. The renderer stays pure, and the verdict rides the consumed-surface contract. The seal lives in the database it attests, so this is drift evidence, not a root of trust. Not done: signing the surface.

### Selective disclosure: prove what you show, show nothing else

**Forced by:** disclosure practice - a participant asked to evidence three trades should not have to hand over the period's whole book. The only pack shapes were the complete prefix and the whole window.

**Landed:** the v3 selective pack - a chosen subset of the rows a covering checkpoint commits to, each with the windowed tier's per-row inclusion proof, so no new cryptography. The selector is a transition-id list (`evidence export --transition`, repeatable); finding the ids stays the discloser's judgment. Rejected for now: subject/predicate selectors, which tempt readers to see completeness in the selection. Bad selections are export errors, never verifier verdicts; a row's position is proven by its Merkle path, never pack order; undisclosed rows are absent in any form. No completeness claim, signature checks cryptographic only, and disclosed indices reveal positions and count. An unknown future pack version is named as too new, never misread as a malformed v1.

### The governing-selection lint: the vacuity lesson becomes a check

**Forced by:** the bitemporal-vacuity lesson becoming a pattern: every "the version in force at a coordinate" antecedent passes vacuously when no version exists there, and a billing engagement was about to author several.

**Landed:** the lint tier's third occupant. The detector recognises the bounded not-a-later-one shape in an implication antecedent (a dated claim bounded on-or-before a coordinate plus a negated `exists` excluding a strictly later version, related by variable identity) and flags it when no OTHER authored invariant carries the recognised backstop. Evidence stays branch-local, and suppression is a must-guarantee algebra (`and` unions, `or` intersects; conditional, negative and undated witnesses contribute nothing; an invariant never backs itself). A shape smell, not a vacuity proof: coordinate agreement is unverified, and the unbounded "current version" spelling does not fire.

### Body `let`: naming an intermediate value without teaching the kernel about it

**Forced by:** the billing trial's rounding define, which repeated `(a * b) / divisor` across its sign branches because nothing in a define or invariant body could name an intermediate value.

**Landed:** `let name = (value)` lines opening an indented define or invariant body, desugared by substitution before the IR exists, so "surface no more expressive than IR" holds by construction and sugared and hand-desugared sources share one `canonical_hash`. The parentheses are grammar (they turn layout off and stop a trailing decimal absorbing the next line's identifier as a unit); substitution is algebraic, not hygienic. Every edge is refused rather than half-supported: duplicates, binder collisions, `actor` as a name, self- and forward-references, computed values in term-only positions, transitively dead bindings, and expansion past a node budget.

### round(x, quantum): the payable figure becomes computable in the rule

**Forced by:** the billing trial. Money rounding was first written as one sign-branched policy define. Real use reversed that: the define repeats its product across both branches, every programme that rounds must re-derive the sign-branch trick (whose naive single-branch form is WRONG on negatives), and the trial's VAT increment multiplied the copies.

**Landed:** `ValueExpr::Round { value, quantum }` with `examples/15_metered_billing/` as the forcing example. One mode: nearest multiple of the quantum, exact halves away from zero (`0.125 -> 0.13`, `-0.125 -> -0.13`). Decimal-only in v0. A non-positive quantum is refused by name at both tiers. A dedicated node, not a desugaring, for the `abs` reasons. The example pins per-line-then-sum as law (two 0.4p lines total 0.00 per-line but 0.01 aggregate-rounded; the sealed-total invariant refuses the rival figure) and the VAT totality pairing, the vacuity lesson taught in-gallery.

### Checked arithmetic: the whole matrix honours the round() contract

**Forced by:** hardening `round` against a division-overflow panic led to probing the shipped arms: the plain rust_decimal operators in the decimal and quantity Arith arms panic on overflow too (`8 / 1e-28`, `1e20 * 1e20`). Well-typed adversarial input could panic the kernel through any declared transformation, unseen by the totality harness.

**Landed:** every arithmetic site routes through checked variants - the decimal matrix via one `checked_decimal_op` helper, the quantity arms, the duration ratio, and both Sum accumulators. An out-of-range result is `EvalError::ArithOutOfRange`; zero divisors keep their own name; the time-arm range errors moved from `TypeMismatch` to the same family. Remainder has no reachable overflow, so the suite pins its boundary exactness instead. The totality harness gained the decimal-maximum witness: the out-of-range family is lawful at extremes, while every other kernel error, and any panic, still fails the suite. No IR or wire change.

### const: the figure the whole rulebook shares

**Forced by:** the billing trial's VAT rates living as an application dict AND as magic decimals in the morph, and the penny quantum spelled three times in our own metered-billing example.

**Landed:** `const name = (value)` at top level, substituted away at parse time by the body-`let` machinery generalised, reaching derived clauses and transformation statements; named and inlined spellings share one canonical hash. Collisions are refused programme-wide, and body lets substitute first, so a local never silently wins over a programme-wide name. Initialisers are literals and earlier consts only (`sum`, `value` and `actor` refused - a state-reading figure is a rule's job). Pattern positions refuse const names outright, because a const substituted into a distant rule's pattern turns a binding into a literal filter.

### Refusal witnesses: the values, not only the rule

**Forced by:** the trial's operator question: "invariant X violated" named the rule and nothing about why, so the next step was always the database by hand, while the failing bindings were live at the failure point and discarded.

**Landed:** one descent carries both the responsible sub-expression and its bindings, run only after `eval_invariant` has returned false, so the accepting path pays nothing. The witness is sorted by variable (unsorted would flake the byte-pinned envelopes) and sits beside the reason, never inside it: the reason string is the pinned wire format. An absent witness, never `[]`, keeps every earlier envelope byte-identical. A witness is only as informative as its rule's bindings, so wildcarding a subject has a diagnostic cost. Not done: every failing witness rather than the first, the require-gate path, persistence to the rejection log, and carrying the offending claim rather than the bound variables.

### `--where`: a read narrows by argument

**Forced by:** the billing trial reading a whole predicate to find one invoice's lines, the same missing filter the ETRM embedder had already flagged.

**Landed:** `--where field=value` on `inspect claims` and `inspect derived`, repeatable and conjunctive, equality only. An undeclared field is a hard error naming the declared ones, never an empty result that reads like "no such rows". Decimals compare as numbers (`--where net_gbp=13.5` finds a stored `13.50`); quantities and collections are refused rather than silently mismatched. The boundary is stated, not left to be discovered: this reduces transfer, not scanning. The comparison runs in the database, but no index covers argument positions, so cost follows the predicate rather than the answer.

### The aggregate `max` / `min`: a truth test becomes a key

**Forced by:** the trial's effective-date parameter: a caller supplied the effective date of the rate to price at, a figure the record already held. An invariant only needs a truth, which `not (exists later: ...)` gives; a transformation needs a value to look the claim up by, and `require` does not export bindings while `bind` needs a determined key. The aggregate turns the truth test into a key, so the parameter is deleted rather than validated.

**Landed:** `ValueExpr::Extremum` - `max(target | body)` and `min(target | body)`, spelled like `sum` because it is the same shape, and told apart from the two-argument `min(a, b)` / `max(a, b)` by the `|`. An empty extremum raises `EvalError::EmptyExtremum` rather than answering: an empty sum has a typed zero, but "the latest version in force" over nothing is no value, and an invented one would flow silently into a `bind` key. An author who means "none in force is a lawful rejection" writes the `require` in front. Ordered kinds only, by an allow-list checked at validation (`UnorderedExtremum`).

### The derived-claim refusal: a read model is not evidence

**Forced by:** `bind` on a derived claim passed `check --strict` and failed only against a live database, an hour lost to a design the checker had blessed.

**Landed:** any rule that references a derived head is refused - `bind`, `require`, `for`, an invariant body, `admit`, `retract`, another derived's domain - since nothing admits a derived head. A derived output cannot carry a claim discipline either: a read model replaced wholesale on refresh honours none, so the clause is refused at the declaration. This is a modelling rule, not a proof the reference could never match: rows admitted under that name by an older programme may exist, which is the reason to refuse - the name would have two sources.

### `effective by` / `total over`: time across slots, and the companion that keeps it honest

**Forced by:** the trial hand-writing "the latest version on or before D" six times, each needing its own totality companion or the rule goes vacuously quiet. Their framing: `current pointer by` governs corrections within a slot, `effective by ... on ...` governs time across slots.

**Landed:** `Discipline::EffectiveBy { keys, on }` generates the in-force selector as a definition plus the keys-and-date uniqueness invariant; example 10 lost its hand-rolled define with tests unchanged as the equivalence proof. It brought `DefinitionOrigin::{Authored, Discipline}` and discipline lowering in two passes (definitions before call resolution, invariants after). `invariant N total over P` declares the backstop; an effective-dated predicate without one earns a hint, not an error, since a partial predicate can be a correct model. The check is positional and selector-reading, so a selecting invariant cannot satisfy its own demand; `total over` naming an undeclared predicate is a validation error. Rejected: requiring the target to carry `effective by`.

### Named gates: a refusal identifies the rule, not its wording

**Forced by:** the trial twice: a unit check moved into an invariant so a test could name it, and two tests passing for the wrong reason because a `bind` failure and a gate failure were told apart only by expression text.

**Landed:** an optional `name:` prefix on `require` and `bind`, carried through the reason, the trace, the rejection log's `rule` column, `inspect controls`, and a `rule` field on the rejected envelope, populated for invariant refusals too, so nobody parses prose. Optional, because a require is a statement, not a declaration. `rule` is absent for an unnamed gate, so a caller never reads a value a rewording can change. Names are unique per transformation, not per programme, since two acts may share a gate verbatim. Not done: a lint for unnamed gates, and naming `admit`, `retract` or `emit`, which do not refuse.

### The SQL spike: invariant checking compiled to the substrate, measured

**Forced by:** commit latency scaling with in-scope state and value-partitioned writers anti-scaling under SSI, plus an embedder's evidence-relation ask on the same mechanism. Branch `spike/sql-invariants` (archived, never merged) holds the code and the full verdict.

**Landed:** a measured verdict of go. A fragment compiler of denial-oriented violation queries, with the delta written first inside the SERIALIZABLE transaction so real indexes serve the checks. Warm-median propose stayed flat at ~2ms from 1k to 100k claims against ~1.3s interpreted; 16 writers into one shared period went from ~10.7 to ~1.19 retries per commit. Compilation plus an index is the unit that pays. A same-snapshot differential swept the gallery with zero disagreements; case-boundedness diverges in one direction over dirty history. A JIT tax (~118ms; the check transaction sets `jit = off`) and an ORDER-BY plan flip made per-invariant plan-shape tests mandatory. Witness identity takes name, version and variable set as strict, values as observational.

### span(P3M) and date subtraction: the calendar the contract already counts in

**Forced by:** the DUoS retrospective naming civil-date arithmetic the sharpest gap: a backbilling horizon maintained as a declared date from outside, and standing days entering as a caller-stated quantity. Carrier: `examples/17_covenant_reporting/`.

**Landed:** `span(P3M)` as `Value::CalendarSpan` (whole months plus whole days) shifting a `Date`: months first with the day clamped to the destination month's end, then days. Not reversible or associative around clamped month ends, pinned directly (two `P3M` hops from 30 November reach 28 May where one `P6M` keeps the 30th). Date subtraction is the signed count of actual days as a decimal, the ACT numerator; 30/360-style counting stays a contractual algorithm outside the kernel. A span is expression-only, enforced statically and at every storage boundary. Refused: `Date + Duration`, `Timestamp +/- span` (it needs a zone the kernel refuses to guess), and combining, ordering, summing or extremising spans.

### morpholog session: the resident process, at the stdio rung

**Forced by:** the DUoS retrospective: every operation a subprocess plus a fresh connection, ~20ms per call over their link, a 130-act seed taking minutes.

**Landed:** `morpholog session <file.morph>` - parse and validate once, one connection, NDJSON requests answered in order with the pinned envelopes; rung one serves `propose` and the claims and derived reads. Two envelopes joined the triple-pin: the ready line (`model_hash` as the staleness token, a `protocol` number) and the coded error receipt, so a caller deciding whether to retry never parses prose. Measured ~3.5ms steady-state against ~11ms one-shot locally. A session is a stateful transport whose loss leaves a commit outcome unknown, so the generated `Session` serialises callers, poisons itself on any lockstep break, and tells a coded refusal, an operational error and an unknown outcome apart. Rejected: a transparent session behind a constructor flag with subprocess fallback, which would hide a different failure model and could re-submit a proposal that had committed. Not done: retries, correlation ids, socket or HTTP transport.

### The session rehearses: explain through the resident process

**Forced by:** the representation-search experiment, an external loop that asks the record many times whether the evidence it holds permits a conclusion, between its own computations, with nothing recorded. The one-shot `explain` already answered that soundly; the session could answer it only as a `propose`, which commits when the answer is yes, so a loop that committed through the session spawned the binary for every rehearsal.

**Landed:** `{"op": "explain"}` on the session, answered with the pinned `explanation` plus the request row: the proposal row's decoder, the one-shot command's one-statement read and in-memory kernel, nothing recorded whatever the verdict. A kernel error stays a verdict in the envelope; a bad request is the coded receipt a proposal gets; a database failure aborts. The generated client's `Session.explain()` is non-commitful, so a lost response is a plain error, never outcome-unknown. The explanation definition gained an optional `row`. No kernel change, no migration. Documented beside it: an admissible answer is permission to attempt the act, not an authorisation - no actor-policy check, no held snapshot, no promise that the later `propose` commits.

### if(when, then, otherwise): the value the record selects

**Forced by:** the DUoS embedder's top ask: a define could not export a parameter bound by equality, so caller-quantity and record-quantity line acts stayed seven near-identical bodies. Relational branching was expressible; a value-producing branch was not.

**Landed:** `ValueExpr::Cond`, surfaced as `if(<prop>, <value>, <value>)`, with `examples/18_scoped_charges/` as the forcing example: one line act where there were two, the applied figure computed in the act and re-checked by the invariant. It is an exists-test with `require`'s non-export rule, lazy branches, a propagating condition error and branch kinds unified with no ordering allow-list. The condition's whole reference set feeds the gate-vs-invariant walk, polarity-blind, because it selects the expected value. Rejected: Eq-as-binder, which inverts the anti-capture guarantee (`define leaky(x): x = limit` errors on the unbound `limit` deliberately) and breaks Eq/Neq symmetry. The full semantics are in [`runtime-semantics.md`](runtime-semantics.md).

### period_index(anchor, span, at): the period a date belongs to

**Forced by:** the DUoS embedder's case (c): charging years run 1 April to 31 March and a billing period crossing 1 April must refuse at admission, which shifts and day counts cannot express without division.

**Landed:** `period_index(anchor, span, at)`, subsuming `year_index`, `same_period` and same-calendar-month. Boundary n is computed once from the anchor with the span multiplied by n, never as n clamped hops, so a 31 January anchor keeps its 31 March boundary. A boundary beyond either calendar end acts as infinity, clipping the outermost periods so the extractor is total. Exact anniversaries enter the new period, dates before the anchor take negative indexes, and a zero span is refused at both tiers. `19_charging_years/` records the charging year's own name via the epoch year and states its inclusive-both-ends convention, since index equality is right for inclusive periods and wrong for half-open ones.

### period_start_of(anchor, span, index): the boundary date back from the coordinate

**Forced by:** the DUoS residue (#300): the rules could compute a date's index but never the boundary date back from an index.

**Landed:** the sibling builtin `(Date, CalendarSpan, Decimal) -> Date`, sharing one `period_boundary` with `period_index`, with the inverse law `period_index(a, s, period_start_of(a, s, n)) = n` pinned wherever the boundary is representable. The asymmetry is deliberate: `period_index` clips because a date always belongs somewhere, but an index whose boundary leaves the calendar has no honest date and is refused, never clamped; a fractional index is refused at both tiers. Example 19 gained rate sheets with identity (`RateSheet(sheet, year_starts_on)`), and the run records which sheet it priced from, because a presence-only gate proves availability, not use. The round trip is not total, since `period_index` clips its lowermost period, pinned by an edge test.

### Actor-assertion policy: who may speak a name

**Forced by:** the DUoS embedder's third ask, and a vacuity in gallery 13: nothing restricted which actors a login role could name, so "two distinct verifiers" was satisfiable by one operator asserting two names in turn.

**Landed:** two reserved claims, recognised by name and all-Subject shape like `AuditSigningKey`. `ActorAssertionRestricted(actor)` arms an actor when admitted, so adoption needs no migration; `ActorAssertionAuthority(actor, login_role)` grants one role the right to speak one name. Two claims rather than one, because arming by grants alone would hand the name back to everybody when the last grant is retracted; here it locks the actor out. The check runs inside the proposal's SERIALIZABLE transaction before anything is loaded and leaves no audit, rejection or outbox row, so a caller cannot manufacture apparent attempts by a name it cannot speak for. Every actor that hands out authority must itself be restricted. The claim is adapter-enforced assertion policy, not proof of authorship: `session_user` resists `SET ROLE`, not a superuser's `SET SESSION AUTHORIZATION`. Not done: a runtime rule refusing grants whose actor ends unrestricted.

### sum(probability * loss | ...): the expression-valued sum target

**Forced by:** `examples/20_operational_information/`: expected loss, a `sum` of probability times loss, must be the sum's own target. The workaround stored a figure the record can compute.

**Landed:** `ValueExpr::Sum`'s target widened from `Term` to `Box<ValueExpr>`, evaluated once per body witness. The target binds nothing and never changes which witnesses exist; an unbound target variable is refused at authoring time; equal values from distinct witnesses each contribute; one witness's error fails the whole sum. Every walker matching `Sum { body, .. }` had ignored a `Term` target and would have skipped an expression one, so the conformance corpus gained a target that hides a lookup. Two refusals came with it: `EmptySumUntyped`, where seed lowering cannot type an empty sum, rather than seeding a wrong zero; and `WildcardAsValue`, for `_` anywhere a value is produced. `Extremum` is deliberately untouched.

### Named-field claim patterns: match by name, not by counting underscores

**Forced by:** the gallery's walls of consecutive wildcards and the billing embedder's 13-ary `InvoiceLine`, where a positional typo was the live near-miss of a silent wrong-position bind. The embedder contributed `..`.

**Landed:** parse-time sugar: `ChargeLine(invoice: inv, net_gbp: n, ..)` lowers to the positional pattern. Resolution is per vocabulary at the site (`emit` speaks intents, claim sites speak predicates); definitions and `value Pred(args)` refuse the form. The formatter re-sugars a pattern exactly when it carries two or more consecutive wildcards; `..` is an authoring-time assertion the formatter may erase. `format_program` renders named while `canonical_hash` stays positional byte for byte, so no programme's ruleset identity moved on upgrade, pinned by pre-feature hash literals. Duplicate field names within a declaration, previously legal, are refused.

### The benchmark suite becomes the instrument the perf arc is held to

**Forced by:** how to keep an AI agent doing the optimising from overfitting the benchmark, asked before the compiled-invariant arc because its spike had met a ~118ms JIT tax and an ORDER-BY planner flip. The bench was a good explorer and a poor instrument: one-shot timings truncated to milliseconds, no repeats, no provenance.

**Landed:** `--repeat` with identical logical pre-state per repeat (the fixture rebuild excluded from the timed sample), `first` / `steady median` reporting, `ANALYZE` after fixtures, and fractional milliseconds; a metric-typed result layer with an `implementation` column; and the frozen `suite` matrix of named complement cases (`write/noise`, `read/grouped`, `asof/retract`, `contend/shared` vs `/disjoint`) with per-family provenance and `suite_contract=1`. Two consumer-derived scenarios joined: `import/core`, the cumulative journey behind `propose --batch`, whose quadratic is the finding; and `wide`, the 13-ary claim shape. The discipline is in [`benchmarking.md`](benchmarking.md), headed by the frozen-ruler rule: a performance PR may change the machine, never the ruler.

### Derived heads project, and value lookups bind by name

**Forced by:** two embedder reports: a reconciliation script rebuilding in Python a join its programme had declared, because the derived's head had to drag coordinates through every row; and a view widened only to carry coordinates its `value` lookups had to state positionally. No head-totality rule existed in code. What forced the dragging was value expressions checked against the full domain scope while the runtime evaluates them under head keys only, so a programme could validate and then fail at refresh; and a `value` lookup's hole being the first wildcard only.

**Landed:** the checker mirrors the runtime: values are inferred against a scope holding just the head keys, and a domain-bound non-key variable in a value expression is refused with both remedies named. `ValueExpr::ValueOf` gained an explicit `extract` index: the positional form computes it as the first wildcard, so every existing programme is byte-identical, and the named form marks the hole with `field: _`. Rendering is forced, never chosen: a first-wildcard hole prints positionally in every mode, so no canonical hash moves; any other hole prints named in every mode, because positional text would reparse to different IR. The carrier is the borrowing base's asset register, made lawful by a one-pledge-per-asset discipline. Not done: the issue's third report, value-less derived pressure, moved to its own issue.

### Compiled invariant checking, rung 1: the checker and the gate that can prove it wrong

**Forced by:** the spike's go verdict, and the correlated grouped aggregate whose interpreted cost is quadratic. Rung 1 ships no performance: the compiler as reviewed code and the instrument that can prove it wrong, with production behaviour, schema, indexes, audit and CLI untouched, because `invariants_checked` lives inside the audit Merkle leaf and index lifecycle needed a home.

**Landed:** the kernel's staging seam (`propose_stage_delta` / `finish_staged_delta`; `propose` is exactly their composition), `write_claim_delta` split from `write_accepted`, and the compiler as a test-gated private module taking a `ValidatedProgram`, so a `CompileRefusal` means "valid programme, outside the fragment", with typed `CompileReason` variants. Argument equality became an exhaustive per-kind proof in place of the spike's jsonb catch-all. The differential stages the body once, so `new Subject()` cannot make the evaluators see different candidates, and every probe rolls back. Governed history is strict three-way (verdict, first failing rule name and version, witness variable set; values observational); dirty history permits one direction of divergence (case-bound may accept where the others refuse, never the reverse). Flipping the compiled `<=` to `<` survived the whole gallery sweep, which set the rule: probe count is not semantic coverage, and every compiler primitive needs a forcing discriminator in the differential corpus, probed on both the matching and the mismatching side. The boundary generator lives in `morpholog_test_support::differential`, shared with eval_totality.

### Claims keyed by digest: the ceiling nobody declared

**Forced by:** an embedder's register (etrmbiz, #320) refused with `index row size 2800 exceeds btree version 4 maximum 2704` on a subject argument of about 3,700 bytes. The argument array was the claims primary key, so the btree row limit was an undeclared, unpredictable ceiling on argument size, surfacing as a raw database error before any decision.

**Landed:** identity stays `(predicate_name, arguments)`; the key becomes `(predicate_name, arguments_hash)`, a stored SHA-256 of the canonical jsonb text through one `claim_digest` helper shared by the generated column, the admit's conflict target and the retract. The digest agrees with jsonb equality for every lawfully encoded argument because the codec writes decimals and quantity amounts as strings. The helper goes through `convert_to`, since the text-to-bytea cast parses backslash escapes. Retraction matches digest and array, so a collision retracts nothing. The derived read cache lost its key for a plain index. Migration 012 migrates the old key, leaves the head shape alone, refuses anything else, and proves the head shape including every stored digest against the helper, because replacing a function never recomputes stored values; the helper is created when absent and refused when different, never replaced. Scans that ordered by the old key now order by the digest, pinned by the first plan-shape test. Validation now refuses duplicate derived heads on a hand-built programme.

### One error receipt for batch and session

**Forced by:** grid-mysteries' two-role rehearsal (#307): their tests could match an actor-assertion refusal by the session's stable code but only by prose through `propose --batch`, which was throwing away a classification it had.

**Landed:** the batch error receipt is the session's typed receipt, `{code, error, row, status}`, one struct and one code set, so the two surfaces cannot disagree. The schema publishes the codes once as `error_code`, and the contract test that walks the enum checks both references. The unauthorised-assertion hint lost its second remedy, retracting the restricting claim, which the runtime cannot know is possible under an append-only declaration.

### Verifier-pinned signing keys: an intersection with the fold, never a bypass

**Forced by:** grid-mysteries' two-role rehearsal (#307). The keys-as-claims fold is only as trustworthy as the set of credentials able to propose: any of them can bring an ungated programme that admits its own `AuditSigningKey`.

**Landed:** `--require-signing-key <file>` and `--require-signatures-from <tree_size>` on both verifiers, as one `SignaturePolicy` applied only over an intact tree. The pin is an intersection with the as-of-prefix authorisation, never a bypass of the claim fold, which would give a pack two meanings of "authorised". On a window or selective pack, which check signatures only cryptographically, the pin is refused by name rather than quietly weakened. The live verifier applies the policy inside the snapshot the intrinsic verdict came from. The threshold is the caller's policy, not derived from the first authorising claim, which would rest it on the governance whose weakness prompted it. A policy flag never masks the intrinsic verdict: the refusal comes only after an intact verdict, pinned by a test that a corrupted window and a malformed envelope verify byte-identically with and without the pin.

### Shared writers across programmes: the collision is the write, not the gate

**Forced by:** grid-mysteries' two-role rehearsal (#307): two programmes proposing into one database, an ungated `AuditSigningKey` registration in one escaping the gates of the other.

**Landed:** `check a.morph --against b.morph`, as a lint. The finding is a shared writable predicate, not differing gates: a reader is not a collision, and two programmes with identical gates are still two admission authorities. It fires whenever both sides write, a retraction included, and says of each writer only whether it carries a top-level admission gate, the line the controls surface draws. Hint by default, `--strict` for CI, `--json` in the pinned check report, anchored on this file's transformation. A broken `--against` file is an unanchored error naming its path, since the JSON report has one `file`. Comparing a file with itself is refused. Not done: gate fingerprints, and a database-side census of which programme hashes wrote a predicate.

### Witnessed checkpoints: when, from a clock the operator does not own

**Forced by:** grid-mysteries (#321), about to stamp its manifests from a shell script because the buyers of its packs ask "when". A signed checkpoint proves the operator's key vouched for a head; time still rested on the operator's clock and word.

**Landed:** rung one, RFC 3161. A checkpoint carries `witnesses` of `{scheme, proof, submitted_to}`: the authority's exact response bytes, so attested time and standing are always read from the proof by a verifier with its own trust anchors. The witnessed bytes are a separate witness payload, so a countersignature can never be replayed as a checkpoint signature or the reverse. Submission is CLI-only and after the commit (`audit checkpoint --witness rfc3161:<url>`, `audit witness --tree-size N`), attaches by `(tree_size, checkpoint_hash)`, and checks the response is over this head before storing it. Verification answers `verified` / `untrusted` / `unverified` / `unsupported` / `invalid`; only `invalid` is a judgement and a failing standing, and `earliest_attested_at` rests on verified witnesses alone. `verified` means what RFC 3161 and RFC 5280 mean, including a certification path validated at the attested time. `verify-pack` wraps its verdict only when asked, so no existing caller sees a changed shape. Not done: OpenTimestamps, and an offline end-to-end of the success path.

### A pre-decision database error is a known non-commit, not an unknown outcome

**Forced by:** #323, the second effect of #320: an embedder's client could not tell a database error raised before the decision from a failure at commit, treated both as an unknown outcome, and refused every further write until a human looked. Before the decision, nothing could have committed.

**Landed:** the classification is decided at the proposal's `tx.commit()`. A PostgreSQL error response to COMMIT means the server rolled back, a known non-commit; a commit call that failed without a server verdict is `CommitOutcomeUnknown`, the one proposal-path error that is not a known non-commit. It permits a false "unknown", never a false "not committed". The promise is scoped to the proposal path; a failed rejection-log insert is `RejectionLogFailure`; the receipt mapping is exhaustive, so a new adapter error fails compilation until someone decides whether retry is safe. `not_committed` and `commit_outcome_unknown` join the receipt codes on the batch and the session; one-shot `propose` exits 3 for the unknown case, since 2 is clap's usage error. The generated client raises `MorphologOutcomeUnknown` without poisoning the session on a receipt; a lost response still poisons.

### Several proposals as one decision: `transact`

**Forced by:** the billing embedder's ghost-declaration incident (#317): a multi-step sequence interrupted partway left a state no single step would have produced.

**Landed:** `transact`, on the one-shot CLI, the session and the generated client: one SERIALIZABLE transaction, one state load over the union of every act's scope, each act proposed against the candidate the act before it produced, every accepted act's delta, audit row and outbox rows written together, a refused act rolling everything back and recorded alone, and one commit through the commit classifier. Only `serialization_failure` is retriable: retrying an unknown batch whole could apply it twice, and nothing identifies a proposal durably enough to deduplicate. The machine contract is three-way, with a coded `atomic_error` object on stdout and exit 3 for the unknown case. Every act shares `committed_at`, so audit order within a batch rests on transition ids created in act order. A refusal at a later act describes it against the staged prefix, so its witness may name values the rollback erased.

### Self-describing audit rows: the signature outlives the programme

**Forced by:** an audit embedder's retired act (#301): rows stored positional arguments and the reader took the names from the current programme, so once an act was retired every historical row lost its signature, and the reader could no longer say what a call did not contain.

**Landed:** the runtime stamps the transformation's parameter names, in declaration order, on every row it writes, inside the Merkle leaf as a self-describing encoding chosen by content like the others, frozen by an independently derived vector. Fresh and upgraded databases describe the column identically, with the constraint added `NOT VALID` as the one-way boundary. Names without an attestation, or not matching the argument count, are no encoding at all, refused at the database boundary and before any pack row hashes. `--named` was left alone: the names are historical row data, and a keyed arguments object would be a second wire representation. Proved in the real chronology: one tree spanning every regime verifies, a renamed parameter in a pack is tamper, and a retired act's row still names its parameters with no programme.

### Hashes are a type: a value that exists is well-formed

**Forced by:** every hash in the tamper-evidence axis was a `String` in the `sha256:<hex>` convention, parsed again at each point of use with its own refusal, and two pack tests forged "hashes" that were prose.

**Landed:** `Digest`, parsed once at every boundary a hash crosses (a checkpoint from the database, a pack from a file, an anchor handed to a verifier) and refused there. Inside the boundary nothing is left to refuse. The wire and the frozen signing and witness payloads are untouched, pinned by goldens and vectors, because the type renders exactly the string it replaced. Not taken: the rules-identity hash (`model_hash`, `canonical_hash`), a separate concern, and the `Tampered` verdict's `recomputed_root`, which can carry a sentence.

### One owner per repeated traversal

**Forced by:** a subtraction round over the workspace: the same structural recursion over the IR written several times, a keyword vocabulary spelled three times, a keyset SELECT written four times, and copied blocks per pack kind.

**Landed:** one position-aware walk for the surface's observation passes; one keyword list yielding the enum, the rendering and the reserved-word map; one depth walker and one duplicate-name helper in the validator; the pack module's shared operations extracted by responsibility rather than behind a trait. `hex` replaced our encoder and two parsers. CONTRIBUTING gained the rule: structural questions use the shared walk, semantic authorities match the IR directly. Rejected: folding the four-way audit pager into one query, because under a generic plan every page would walk the index from its start, and a null-cursor sentinel would hide a row; and replacing the outbox's clock trait with paused tokio time, since the clock also supplies the lease wall time.

### Contract hardening: one authority per trust-boundary decision

**Forced by:** drift the subtraction round made visible: the score report spelled a UTC instant `+00:00` where every other envelope spelled it `Z`, a short audit log shared an error variant with an oversized lease, and the one-shot and the session checked `where` preconditions in their own words.

**Landed:** one commit per observable difference. An instant has one wire spelling - RFC 3339, UTC, `Z`, the shortest of zero, three, six or nine fractional digits that loses nothing - owned by one adapter module and pinned by vectors; a domain timestamp inside a claim keeps the kernel's codec. Precision goes past the database's microseconds, because the witness axis and a user-supplied boundary can carry more, and a canonical boundary must not be lossy. An incomplete audit prefix is its own typed error, named for the condition rather than as tampering. A `where` clause is read by one resolver on both surfaces. A test records what would be lost if the hand-written result schema were generated.

### The candidate state is the pre-state plus what changed

**Forced by:** a measurement for #193: every `propose` rebuilt the candidate state from scratch, 163 ms over 100,000 in-scope claims and 30 s of a 370-act `transact` batch.

**Landed:** `State` is a shared base behind an `Arc` plus a bounded overlay of admissions, tombstones and dead marks. A candidate shares its pre-state's base and copies only the overlay; past a churn threshold the overlay folds into a fresh base. Cost scales with the accumulated overlay, not the current delta; a persistent-map dependency was declined until a measured curve demands it. Logical order is base minus retractions, then additions; a re-admitted claim moves to the tail. `State::claims` returns a view rather than a slice, the one deliberate public API change. A property test drives random histories through several compaction policies against the naive rebuild. Measured after: 16 us for the proposal that took 163 ms, 23 ms for the batch that took 30 s.

### A replay advances one state

**Forced by:** the layered state's second customer: coverage, scoring and as-of reconstruction each kept a replay set and snapshotted it per step by copying every live claim.

**Landed:** the replays advance one `State` in place through the kernel's own `apply`, and the replay set is gone. A historical read's order is now audit replay order: a claim retracted and re-admitted moves to the tail. The scorer refuses a candidate that reads `pre(...)` at construction, so it never snapshots a pre-state, and coverage clones one only when `pre(...)` tracking needs it. The first cut cloned the state on every scoring row and was slower than what it replaced: a cheap snapshot is cheap relative to the book, not to a tiny live set, and a replay that observes every row must not snapshot at all. On a 100,000-row log, `inspect coverage` went from 3.8 s to 0.9 s, `evaluate` from 4.1 s to 1.1 s, and as-of reconstruction from 1.06 s to 0.86 s.

### The execution plan is built once beside the programme

**Forced by:** #277 rung 2: invariant evaluation over the whole book on every act dominated a batch, and the first step was deciding which path a programme runs on, once and reported.

**Landed:** `PgProgram`, the adapter's programme object: the validated core programme beside the PostgreSQL execution plan built once from it. Every invariant compiles or none does, and the interpreter runs the whole programme when one is outside the fragment. `check -v` reports the plan - `compiled`, or `interpreted` with each refusing invariant and its construct. No production path read the decision at this step. The object lives in the adapter, never in the substrate-neutral core; load scope stays one backend-aware authority; diagnostics stay interpreted; index provisioning lands before the production switch.

### The compiler names its indexes; provisioning reconciles them

**Forced by:** #277 rung 2's second step: compilation plus something to seek on is the unit that pays, and the per-programme index set does not fit the static migration model.

**Landed:** the compiler emits an index specification wherever its SQL filters or joins, from the same representation object that renders the query's extractor, so an index uses the representation the SQL reads with; one per (predicate, position), named in a reserved namespace. `morpholog provision indexes` reconciles them: keep, create, repair an interrupted build, satisfied by an operator's equivalent index (never pruned), stale under `--prune` only, or a conflict under Morpholog's own name, left alone with a non-zero exit. Builds and drops run concurrently outside any transaction; one advisory lock serialises provisioners, polled rather than awaited, because a session blocked in `pg_advisory_lock` holds a snapshot a concurrent build waits for, which deadlocked the first run. Migration 015 adds the registry, with a separate requirement relation so a database serving two programmes keeps both. The plan-shape gate proves every required index is used by the query-side renderer and chosen by the planner on a populated ledger; a probe spelled from the specification's own expression drifts with it and proves nothing.

### Proposals take the compiled route

**Forced by:** #277 rung 2's third step: the plan was built and reported and the indexes provisioned, and no proposal path read either.

**Landed:** every proposal facade takes the adapter's programme object, and a programme whose invariants all compile takes the compiled route: load the body's reads only, stage once, write the delta into the SERIALIZABLE transaction, check each invariant in programme order against the claims table, then write the audit and outbox rows. A violation becomes the kernel-shaped invariant rejection; `invariants_checked` lists the whole programme either way, so the audit leaf does not depend on the route. Trace, explain-on-reject and outbox compensation stay interpreted, since an explanation needs the state the invariants read. The commit path runs the same code the differential proves. A SQL error in a check is operational, never a quiet switch to the interpreter, which would hide a compiler defect as a slowdown. A bare top-level negation names no witness on either route.

PostgreSQL's `numeric` is wider than the kernel's decimal, so a compiled sum could admit what the kernel refuses with a range error. Closing that found a kernel defect: implication and `forall` returned at the first failing binding, so whether a later binding's error surfaced depended on claim order. The kernel rule is now error over false, pinned for both operators under both claim orders. On the compiled side representability is data, never a thrown error, since the planner's row order promises nothing: each sum is a LATERAL total with a range flag, and on a violation one unordered query asks whether any row in scope has a range error. The test is the whole decimal domain, scale at most 28 and coefficient under 2^96. Sums compile only where both evaluators share an evaluation boundary. The differential now carries kernel errors through and requires both stages to report the same typed error. No migration.

### The benchmark changed the decision

**Forced by:** #277 rung 2's fourth step. The plan, its indexes and the production compiled route had landed without a measurement under the frozen ruler; the spike's verdict, taken with stage 2 running, was the only evidence.

**Landed:** one selector on the bench, `--implementation`, choosing the kernel interpreter, the compiled route with no compiler-required index, or the compiled route with every index it names. The bench establishes the index condition itself, after every reset and outside every sample, and refuses a configuration it cannot honour; the suite contract did not move. The tables are in the bench README, under "Three-way verdict on the compiled route". The verdict changed the decision: indexes are necessary but not sufficient. Unindexed compilation is strongly superlinear and far behind the interpreter. With indexes, stage 1 tracks the interpreter on the ledger write path, linear with the same slope, and reduces no retries under contention; it wins only where in-memory evaluation is the expensive part (two and a half times on the wide case at a hundred thousand rows). The spike's flat curve and ten-fold fewer retries were stage-2 effects: stage 1 proves every invariant over the whole relation, so its cost is linear in state and its SERIALIZABLE read footprint is the whole predicate. Import gained nothing from indexes because a table growing from empty keeps the empty table's statistics until it is analysed; the import ruler stays, since that is what a fresh deployment sees. #277 stayed open for rung 2b.

### One authority for the cases a delta touches

**Forced by:** #277 rung 2b's first step. The benchmark attributed the spike's gains to the case-bound stage 2, and the review settled that case-bound admission must live in core, so that whether inherited dirt blocks a transition never depends on compilation eligibility.

**Landed:** the compiler's occurrence binder moved into core as `ImpactPlan`, built once per invariant from the IR through the shared fold, knowing nothing of SQL. Applied to a delta it answers untouched, bounded to partial bindings of the invariant's case variables, or unbounded. An occurrence that cannot constrain a case variable widens to the whole invariant, an unproved construct widens a non-empty delta to unbounded, and an empty delta touches nothing because the candidate is the pre-state. The compiler renders a bounded answer onto its antecedent columns. No behaviour changed: the stage-2 pins hold byte for byte and the differential agrees. The kernel did not consume the plan at this step.

### Admission is case-local

**Forced by:** #277 rung 2b's second step. Making case-bound checking the production rule on the compiled route alone would have let inherited dirt block a transition or not depending on whether its invariants happened to compile.

**Landed:** admission is case-local revalidation, in core, on both evaluators. The kernel computes the effective delta, the admitted-set change between pre-state and candidate, so a duplicate admit, a retract of what is absent and a retract-then-readmit change nothing. Each invariant's impact plan classifies it as untouched, bounded to cases, or unbounded; the interpreter evaluates only the touched cases and the compiled checker bounds its SQL to the same cases, reading the effective delta back from the claims table. Touching a dirty case without repairing it still refuses; an unbounded impact checks the whole invariant; a transition that changes nothing is admitted whatever the history holds. `eval_invariant` stays a whole-state predicate, so `evaluate`, `explain` and the verifiers still ask what a state satisfies. Witness, trace and range errors are scoped to the obligations evaluated. `invariants_checked` is unchanged, its meaning now stated. Scoring became `case_bound_admission_v2`, charging a commit to a candidate iff the obligation admission would have raised fails. The differential's dirty-history law became equality between the kernel and the case-bound check, with the whole-state check named as the one that lawfully differs. The rule is in [`runtime-semantics.md`](runtime-semantics.md).

### The case-bound check reproduces the spike

**Forced by:** #277 rung 2b's third step: the same frozen ruler again, judged on what the rung-2a verdict said stage 2 owed - flat compiled scaling with state and fewer serialisation retries.

**Landed:** the stage-1 binary at 6174bef and the rung-2b binary at 1746237 ran back to back in one session, so the within-session ratios are the claim; machine load roughly doubled every database-bound number that day. The tables are in the bench README, under "Case-local admission, paired against the whole-state check". Semantics: inherited violations outside the affected cases no longer block admission, on either evaluator. Scaling: an ordinary posting on the indexed compiled route costs 8.96, 6.80 and 7.44 ms at one, ten and a hundred thousand entries, against stage 1's 32, 205 and 1,583 ms. Concurrency: at sixteen shared-key writers, 0.72 retries per commit at 1,882 commits per second, against 2.39 for the disjoint control and stage 1's 11.03 at 22 commits per second; narrowing the read footprint removed the relation-wide dependency stage 1 suffered under SERIALIZABLE. Import from an empty table runs at about 5.3 ms per transition, no longer dependent on PostgreSQL's statistics. The spike's flat curve, reduced retries and need for indexes are confirmed; its ~2 ms at 100k is confirmed as a shape, the constant being the day's. The limit: case locality removes scaling with unrelated state, not with the size of the affected case. The wide case at a hundred thousand rows, where every row is the affected case, still costs 30.2 s per proposal (stage 1: 327 s).

### The public Rust API is a committed snapshot

**Forced by:** three reviews in one day (#338, #341, #342) caught the same mistake - a refactor that removed or reshaped public Rust items presented as an internal change - and nothing in the gate would have. #345 asked for a mechanical finding instead of a review discovery.

**Landed:** the public surface of the library crates committed as text under `api/`, rendered by `cargo public-api` from rustdoc JSON with all features, and checked in precommit and CI. A surface change is an ordinary diff of `api/` in the PR that makes it, and additions are as visible as removals. The crate list is explicit; the nightly toolchain and the tool version are pinned in the script alone, so stable and the declared floor remain the compiler contract, and the check never skips. Rejected: `cargo-semver-checks`, which missed the month's largest break (every propose facade moving from `&CompiledProgram` to `&PgProgram` in #353) because it does not check parameter types, and errors on crates absent at the baseline. Marking the public enums `#[non_exhaustive]` is a separate decision.

### The adapter runs on one time library

**Forced by:** #346. The kernel's domain timestamps were jiff while every operational instant in the adapter, the outbox and the CLI was chrono, so the workspace carried two clock types and converted by hand. Once `wire_time` had frozen the bytes, the swap could be proved rather than trusted.

**Landed:** `jiff::Timestamp` for every operational instant, through `jiff-sqlx`, with one line of `sqlx.toml` mapping `TIMESTAMPTZ` for every query macro. chrono remains only inside the witness crate, which needs it for certificate dates, and leaves the public API. Nothing moved: the wire-time vectors, every golden envelope, pack byte-equality and the signing and witness vectors are unchanged, and the same instants stored identically through both libraries; the PostgreSQL 2000 epoch case is now a test. `wire_time::parse` accepts a small fixed shape checked in front of jiff, RFC 3339 without leap seconds or fractions finer than a nanosecond, because jiff accepts an open-ended set of spellings; a table pins what it accepts and refuses. `--as-of` and `--train-until` read through it, so the CLI has one timestamp parser.

### One paging contract for the audit log

**Forced by:** #347. Every walk over the audit log in replay order owned its own keyset loop, and two owned their own SQL. The subtraction round named the duplication and left it.

**Landed:** one crate-internal pager with two projections that share one cursor and bound contract: a narrow row for the claims replays (as-of, coverage) and `AuditRow` for the whole-row walks (the tail, scoring, pack export, checkpoint leaves, signing-key rows). Each consumer keeps its own fold, and a new replay writes no SQL. The pager owns the walk's range, so as-of reads through its target transition in SQL and no caller can apply a row past it. Rejected: one row type for every walk, because a reconstruction would then read, and fail on, columns it never judges. Each bound stays a literal query, since a flagged single query loses the index condition under a generic plan (#338); a plan-shape test pins that. The paging tests run with index scans off, because the audit index returns tied rows in id order by itself and hid a dropped tie-break. Measured with `scripts/bench_ab.sh main`: every as-of and replay row within noise.

### A known non-commit is something the binary says

**Forced by:** etrmbiz's upgrade of its register to v0.0.11 (#323, reopened). The generated client decided "nothing committed" by elimination, so a binary killed after it sent COMMIT, exiting by signal with empty stdout, read as a proposal that had not committed.

**Landed:** the binary states every outcome it knows, and the client trusts nothing else. One-shot `propose` and `transact` print an error object (`request_error`) for every failure before or inside the proposal. The client reads a decided envelope or a published code as a statement, and everything else - silence, a signal, garbage, a code it does not know, a timeout - as unknown; the exit code agrees but decides nothing. Batches follow the same rule, which closes #330: each receipt is flushed as its row finishes, a timeout keeps the output that arrived, and the client returns the receipts only when every row has one, otherwise `MorphologBatchIncomplete` names the receipts, the rows that may have committed and the rows that never ran. A rejection whose record could not be written is now `not_committed`, since the rollback comes before the record. The only uncoded failure left is a receipt that could not be written after the adapter returned, which stays unknown because the row may have committed.

### Which incarnation of a role asserted each actor

**Forced by:** Grid Mysteries (#385, via etrmbiz 2026-09-23). Attestation recorded the asserting role by name, and the actor policy grants by name, so anyone able to create roles who recreated a dropped one would inherit every grant to the name, and no audit row could tell the two apart.

**Landed:** the attestation records the role's OID beside its name, read in the same statement as `session_user`. The attestation is hashed whole into the leaf, so new rows commit to the OID and older rows hash as before; no leaf version, no migration. A session whose role was dropped, or dropped and recreated, cannot read `session_user`, so it can never record its successor's OID; a test holds that. The verifiers report `role_rebindings`, each role name seen under a new OID, as a finding that never changes the verdict, drawn only from rows an intact verdict established; `verify-pack` now always prints its report. Detection is the boundary: refusing a recreated role at commit belongs to the signed-proposal rungs built on #202. The OID protects only from its first recording on, and it is local to the cluster, so a restore can show a change that is not one.

### A complete pack a laptop can check

**Forced by:** Grid Mysteries' certificates (#388, via etrmbiz 2026-09-23). A complete-prefix pack of 137,384 transitions was 660 MB, and `verify-pack` peaked near 6 GB of memory. The person a certificate is for checks it on an ordinary laptop, and only a complete pack shows nothing was left out (#380).

**Landed:** the complete-prefix pack is written one line at a time: a manifest, one line per checkpoint, then every covered row in log order. Export streams it from one snapshot, and `verify-pack` reads it the same way through a verifier core shared with live `audit verify` and older single-document packs, each entry point's own rules pinned apart. The verifier keeps a Merkle frontier instead of every leaf and one running set of authorised signing keys, so its memory follows the checkpoints and findings, not the rows. The order failures are reported in is written down and held against the old verifier. gzip, not the format, removes the repetition of each value in `arguments` and `asserted_claims`, because the leaf hashes both as written and a decoding step in the trusted path would be a second thing to get right; `verify-pack` reads gzip itself, several members as one stream. On a synthetic history of the same length, peak memory for `verify-pack` went from 1.9 GB to 8 MB, and for export from 1.3 GB to 16 MB.

### Compiled checks order quantities and instants

**Forced by:** Glasshouse (#392, a power-trading ledger on v0.0.11). Four of its invariants kept the whole programme interpreted: `unique by` over `Decimal[MW]` and `Decimal[EUR]` positions, a quantity that must be positive, and a period whose start must precede its end. Under twenty organisations proposing at once the interpreted route retried eight times per capture (#396 has the table).

**Landed:** a quantity position joins and filters on its amount as `numeric` and its unit as text; the index is the amount, the unit a residual. An ordered comparison compares amounts for quantities and `morpholog.timestamp_nanos` (migration 016) for timestamps, numeric nanoseconds since the epoch, because the stored text does not sort (`.5Z` before `Z`) and `timestamptz` rounds to microseconds and stops short of the kernel's year range. Where the kernel raises while comparing (two units, or a stored value of another kind), the SQL carries that as data: the violation query returns the row, and a second query names the first erroring binding in the kernel's own candidate-state order, so the kernel's own rule reports the exact error; naming a different pair of units than the interpreter would weaken the differential's error-parity contract. Glasshouse's programme now compiles whole. Code review found compiled equality by declared kind wrong for a value of another kind at a position; this rung guarded it fail-closed with `morpholog.declared_kind`, and the next entry replaced the guard with a canonical key.

### One equality key per claim position

**Forced by:** the review of the compiled quantities (#397, #398). A compiled check read each position by its declared kind, so a value of another kind there joined as NULL or by the wrong reading where the kernel compared the values it found; #397 had only failed closed.

**Landed:** `morpholog.value_key_v1`, one function of the stored tagged value whose equality is the kernel's for every value the codec writes: decimals and quantity amounts lose their scale, a quantity's unit rides beside its amount, a collection's key is its elements' keys. Every join, literal filter and case filter compares keys, and a literal's key is the same function, so there is no second implementation. The index holds the key's digest, not the key, because a subject or a collection can exceed a btree entry (why claims moved to a digest key in migration 012); a pattern join compares digests, the claims table's own identity assumption, and an equality in value position compares the keys. The version is in the name because the index expression carries it: other semantics means a new function and a rebuild. Migration 017 drops #397's guard. Collection and `Any` positions now join; ordering over them still refuses. Rejected: the exact key as a residual beside the digest in every join, measured five times slower on a group-sum rule while adding no assurance the primary key does not already rest on. Not done: `inspect claims --where` still reads positions per kind.

### A proposal reads the rows it can observe

**Forced by:** Glasshouse's classroom (#396). Twenty organisations proposing at once against one ledger, none reading another's records, retried eight times per capture at ten thousand claims. Every proposal loaded every claim of every predicate its body read, so any two writers conflicted on a relation-level lock however disjoint their cases.

**Landed:** a static read plan per transformation, in core, resolved with a transition's arguments and actor into a storage projection. A claim pattern fixes some positions before the body runs (a literal, `actor`, or a parameter no statement rebinds), so the load keeps, per predicate, only the rows matching some pattern's known coordinates; a pattern that fixes nothing, or a key the body binds as it runs, makes its predicate whole. Definitions are followed by an environment, not substitution. Admits are kept apart from reads: only the interpreter needs an admitted predicate's membership. The loader is one statement, one select per predicate joined by `UNION ALL`, because a single mixed scan loses the keyed predicates to a sequential plan; values are bound through the key function, never rendered into SQL. Every programme, interpreted ones included, provisions the indexes its loads seek on, since an unindexed keyed read is slower than the old whole read. Proof: the scope differential, a loader differential, a plan gate, and an SSI pair showing two keyed proposals commit together with no relation lock. Sixteen writers on one shared predicate went from about 8.5 retries per commit at 60 commits a second to 0.08 at 5,500 on the compiled route with its indexes. Not done: the interpreted route's invariant reads stay whole until a forcing example.

### A check bounded to a case seeks on the case's columns

**Forced by:** Glasshouse's per-backend lock sample on v0.0.12 (#396). With every invariant compiled and keyed loads in place, a relation-level SIRead lock was still held, by two case-bound checks: one keyed by columns no managed index covered, one seeking the least selective of three digest positions because the ledger had no statistics over the new expression indexes.

**Landed:** a compiled invariant requires an index on every column its case can be keyed by, from the impact plan's bound variables, so the case-bound filter seeks the way a join does; the plan gate asserts it, and a SERIALIZABLE test on a populated ledger asserts no relation lock remains. `provision indexes` analyzes the claims table on every run that applies. Two hypotheses fell first, the actor-policy shape guard and page-lock promotion inside `transact`; a sampler joined to the running statement named the two checks.

### Orderings anywhere in a scope

**Forced by:** Glasshouse's effective dating (#407). Its generated in-force selector puts date comparisons inside a `not exists`, and the compiler refused any comparison that could raise unless it closed its scope, for parity on dirty history. The same rule kept several gallery rules interpreted.

**Landed:** an error plan beside the truth query. Each way of raising is a probe in a scope. A conjunction keeps its conjuncts' scopes in stage order, since the kernel runs every binding through one conjunct before any reaches the next; whatever the kernel evaluates whole per enclosing row (an implication's consequent, a `forall` body, `exists`, `not`, a nested implication) lifts its plan into one scope of that row, binding-major. Every pinned violation query stayed byte-identical. Sums keep their closing boundary, marked on the rendering. A claim pattern after an ordering in one conjunction is refused by name, since a binding that raises with no later join has no row to return. Rejected: querying a consequent stage by stage, which names the wrong error when an earlier antecedent row raises at a later stage. The work also fixed an older gap: a consequent that joins then filters binds its variables in the witness only where a row joined, as the kernel's does.

### Dates order in the compiled checks

**Forced by:** Glasshouse's effective dating (#407), and in the gallery 17's `overdue_notices_precede_any_delivery`, the one rule a top-level date comparison alone kept interpreted. Two assumptions fell first: that stored dates would text-order, when the codec writes signed six-digit years outside 0000-9999; and that date order needed no coordinate of its own, when a `date` cast refuses those same years.

**Landed:** `morpholog.date_ordinal` (migration 018), the year, month and day as one integer, NULL for another tag and an error for text the codec never writes, guarded like the equality key by marker and body digest. Proof: the coordinate against jiff over the extremes, the leap rules, year zero and negative years; a dirty-history probe; the upgrade probe. 10's `settled_date_has_effective_terms` and 19's `runs_run_forwards` compiled too, with no new scope machinery.

### Compiled checks follow defined calls

**Forced by:** Glasshouse's effective dating (#407): its generated in-force selector is a definition, so any rule calling it kept the whole programme interpreted. In the gallery, 06's `consent_obtained_before_randomisation` and 13's `decision_rests_on_two_distinct_prior_verifications` were refused for the call alone.

**Landed:** a call joins its body into the caller's chain under the call's frame: a parameter bound to a caller column is a correlated equality, one bound to a literal a filter, a free one binds from the body, and only parameters come back. The body's errors are asked whole per caller row. A call under a sum stays refused, since a sum would count a call's internal witnesses where the kernel counts each projection once. The impact plan follows calls too: an argument carries a case variable or literal into the body's patterns, a call creates no case variable, and the plan with no definitions stays the conservative baseline. Scoring moved to `case_bound_admission_v3`, format version 3. The witness now comes from a diagnosis run only once a rejection is established, replaying the kernel's failing path against SQL level by level, because a violation row ordered for the planner can fail at another depth; the case-bound witness is held to the kernel's exactly. An exhaustive kernel oracle checks the impact plan over the invariant's own case space.

### Statistics on every seek position

**Forced by:** a rejection-latency check before merging defined calls (#424). At 100,000 ledger entries a posting took about 150 ms where the 2026-09-18 verdict had recorded 7 ms; bisected to the equality-key change (#399), shipped in v0.0.12.

**Landed:** the equality key made every seek the digest of an expression over a partial index, and PostgreSQL does not estimate a filter from a partial index's statistics, so past about twenty thousand entries the planner walked the predicate; the `ANALYZE` #412 added gathered statistics the planner does not consult here. `provision indexes` now keeps one statistics object per claim position its indexes seek on, over the same expression across the whole table; a same-named object with another definition stops the run before any change. The posting at 100k is back to a few milliseconds. Statistics over the predicate name and the expression together estimated no better, so the single-expression form shipped. The planner gate now populates twenty thousand entries and goes red without them. Deployments pick them up by running `provision indexes` again for every programme.

### The index report as an envelope, over every programme at once

**Forced by:** two embedders on v0.0.12 (#408). Glasshouse parsed `provision indexes`' printed plan, a bridge over a surface with no envelope. Grid Mysteries served several programmes from one database and had to prune on the last run only, since the command took one programme.

**Landed:** `provision indexes` takes every programme the database serves and reconciles their union. One requirement relation answers every question, built from the named programmes' own specifications and the registry's rows for every other name; the report, the stale set, the prune and a dry run all read it. A conflict in one programme applies nothing for any, and two programmes under one name are refused. `--json` prints the `provision_report` envelope, pinned in `schema --result` and the goldens and returned by the client's `provision_indexes()`, its lists in one order whatever order the files were named in. Indexes a programme outside the call protects are listed as `required_elsewhere`, since the call did not reconcile them against the catalogue and cannot say they exist. Not done: a retired programme's requirements are released only by a run under its name.

### The generated client checks the binary at first use

**Forced by:** Grid Mysteries on v0.0.12 (#409). A client generated for v0.0.11 refused v0.0.12's new error object as contract drift, but only when an error happened, though the package carries the version it was generated for and the binary knows its own.

**Landed:** the `hash` report carries the binary's version beside the rules hash. `open_client()` and `open_session()` check both against the package's stamps: the one-shot client asks `hash` once before its first call, through the one subprocess seam every call takes; the session reads the version its ready line carries. A mismatch is refused naming both sides, with `MORPHOLOG_BIN` and direct construction as the ways out, and a refused client stays refused. It detects deployment skew at first use; it does not bind a binary or programme replaced later. The version in an envelope is read before the strict parse and the protocol check, since a binary of another version is why either would differ. Only the exact two-field `hash` report of older binaries is named as old; an envelope stating no version is malformed.

### Statistics go when no programme needs their position

**Forced by:** #425, filed when the seek statistics landed. A requirement an operator's own index satisfies recorded no position, so nothing could say no programme needed a statistics object, and a retired programme's statistics stayed for good.

**Landed:** a requirement records the argument position its specification seeks on (migration 019, a nullable column, so an older binary keeps recording without it), resolved from the named programme's specification, the recorded row, or the managed index of the same specification, in that order. Statistics are reconciled for every programme at once: each object is created if required and missing, kept if required, a conflict if its definition is another's, and stale when no programme is known to require it, dropped under `--prune`. Only an object whose definition matches Morpholog's exact naming is ever stale. An unresolved position is reported under `positions_unknown_for`, and no statistics object is stale while any is named. A registry that disagrees with itself about a position is refused before any change. `pruned` names every managed object a run dropped.

### Every database-backed command refuses a schema behind or ahead of the binary

**Forced by:** Glasshouse (#393), which runs a web service and a nightly job as separate processes on one database, so every upgrade has a window where two binary versions share one schema. On v0.0.12 neither direction refused.

**Landed:** one check, `require_current_schema`, asked by every command that opens a database except `init` and `migrate`; the resident session asks before its ready line, the outbox worker before its first drain, and `init --least-privilege` before applying the floor to an existing schema. A database with no schema names `init`; one ahead is refused as newer than the binary, asked first so an older binary never advises a migration against a database it does not understand; one behind names `morpholog migrate`. On a proposal the refusal is the `not_committed` object. The head schema file records the migrations it embodies, held to the binary's list by version and name, and is applied in one transaction. Older binaries cannot be retrofitted; the install guide's upgrade section is the operating sequence: back up, ask, migrate, verify, provision, restart every process, and the backup is the rollback.

### A mixed route, per invariant

**Forced by:** #407's second rung. One invariant the compiler refused put the whole programme on the interpreter, with whole-predicate invariant reads and the retries those bring, and many gallery programmes still stood on it (`or` in a consequent, arithmetic, a builtin call, `pre`).

**Landed:** invariants are compiled one by one, and the backend is one shape whatever the count: the compiled set, the interpreted ones, the refusals, and the runs, contiguous ranges of like kind in programme order, built once at load. The two homogeneous routes run as before; the interpreter-only route still decides in memory before it writes. A mixed execution stages the body once, writes the claim delta once, takes the effective delta from the table, and walks the runs; the first violation or error in programme order is the answer, so a mixed programme names the invariant the pure kernel would. The effective delta has one authority, the table's, lent to the kernel through a new entry. `check -v` reports `mixed, N compiled, M interpreted` with each refusal; diagnostics keep the interpreter. The `mixed_route` harness holds the mixed route to the kernel, including interleaved evaluators and an error in one with a violation in the other, in both orders.

### PreparedProgram, and the law between the levels

`CompiledProgram` validated a programme, indexed its lookups and precomputed the impact plans; the SQL compiler lived in the postgres crate, so the name pointed at the wrong thing. It is `PreparedProgram` now, and `PgProgram` is named for what it is: the PostgreSQL binding of a prepared programme to its execution plan (the crate-private `ExecutionPlan`, formerly `InvariantBackend`, in the field `execution`).

The rename came with the law it was for, written once in [`runtime-semantics.md`](runtime-semantics.md): the programme has identity, the prepared programme is a recomputable cache, the plan chooses machinery, and preparation and execution may change cost, never meaning. The law speaks of the same semantic outcome (change, intents, rejection with its rule, version and witness, or error), not only the same verdict. An impact plan lives in the prepared programme because it is preparation derived from the programme, not part of its meaning. The existing differentials are named as instances of the law rather than a proof of it.

### The execution seams, named

The mixed route needed two kernel operations no ordinary caller should use: an admission over one run of the invariants, and finishing a staged delta under an effective delta the database had already established. They had joined the public core API as hidden items. They now live in a public module, `morpholog_core::execution`, whose documentation states the admission test: an operation belongs there only when an execution layer supplies a derived fact or partition that claims to preserve semantics established elsewhere, the kernel cannot verify it from its own inputs, and a false but well-formed value can change the semantic outcome. Each operation states its obligation (the exact run the plan assigned to the kernel, in programme order; the true change in the admitted set, neither understated nor overstated). `finish_staged_delta_with` stays at the root, since it computes its own effective delta. The internals stay crate-private, and core has no hidden items left.

### `check --json` reports each invariant's route

Two embedders, Glasshouse and Grid Mysteries, held their programmes to the compiled route by reading `check -v`'s text, a bridge over an unpinned surface. The check report now carries `invariants`, one entry per invariant in programme order, each `compiled` or `interpreted`, and an interpreted one carries its refusal: a `kind` from a closed list and the message `check -v` prints. One private projection, built from the production plan, feeds both the JSON and the verbose text, so the two cannot drift. There is no summary field, since compiled, interpreted or mixed follows from the entries. The kinds are the report's own contract: a compiler reason the list does not name is an error until the schema names it. The route belongs to the programme and the binary together, so a deployment that must stay compiled pins both.

### Claiming no more than it can witness

The scope document now states the principle the design history had settled case by case: Morpholog claims no more than it can witness. Applying it to the same document corrected two older sentences. The constitution's last line, "bad state cannot be committed", said more than case-local admission guarantees: a commit cannot introduce a violation, or leave one unrepaired, in a case it touches, while violations inherited outside those cases may remain. And "delivered at least once", repeated across the docs, said more than the outbox does: workers attempt delivery, retry transient failures so a delivery may repeat, and record as failed one the deliverer judges permanently impossible. A first rewrite still said "retried until delivered"; a failed delivery was not delivered, so the wording is about attempts.

### A case as wide as its pattern makes it

**Forced by:** Glasshouse (#396, rounds two to four). A relation-level lock traced to `Terms(delivery_start: s, delivery_end: e, ..) implies s strictly_before e`: the case is what the rule's pattern binds, and `..` binds nothing, so every trade sharing a delivery window was one case. Naming the trade cut retries per commit from 1.6 to 0.4.

**Landed:** a lint variant, `CaseWiderThanClaim`, hint-grade. Eligibility is the invariant's own impact plan: not conservative, one claim pattern, binding a case variable. Identity is the disciplines' `uniqueness_keys_for`, the same clauses lowering turns into invariants, so lowering and the lint answer to one authority. The hint fires when no enforced key is fully constrained, by a variable or by a literal the impact plan actually compares (a Subject or a Decimal), and names the fields missing from the closest key. Rules admission checks whole get no hint, since naming a field narrows nothing there, and defined calls are excluded, since the pattern to fix might live in the definition. The gallery rules with the shape were respelled by naming their key.

### Every commit names the programme that admitted it

**Forced by:** the proof story. An audit row named its rules by name and author-declared version, so a rule body edited without a version bump left history unable to say which rules admitted what, and receipts, replay and re-admission had no join key back to the rulebook.

**Landed:** migration 020 adds `audit.model_hash`, the canonical hash of the whole programme, stamped by the one audit write every commit path shares; on migration 014's pattern, nullable, never backfilled, required on new rows by a `NOT VALID` constraint. Merkle leaf version 4 is version 3 plus the hash, chosen by presence. The writers take the `PreparedProgram`, so a durable admission cannot be written without its programme. Compensation now owns a `PreparedProgram` rather than accepting a hash, since a configured hash could name rules unrelated to the ones checked. A new field covered by a Merkle leaf is at once a leaf-format change and an evidence-container compatibility change: each pack kind gained a version chosen only when the pack discloses a stamped row, and `AuditRow` refuses unknown fields, since an older verifier would otherwise drop the hash and report tamper on honest history. Validation holds every generated item to the exact lowering (`GeneratedNotFaithful`): canonical identity may omit generated machinery only when validation proves that machinery is a deterministic function of what the identity covers. A present `null` is malformed for every leaf-rung field.

### Fresh subjects are an input

**Forced by:** the second gap the vision assessment found. `new Subject()` minted a UUIDv7 inside the kernel, so the kernel read the clock and a random source, and the same programme, state and proposal could produce different bytes.

**Landed:** a `SubjectSource` passed to every kernel execution entry point; any iterator of subjects is one. Running out is `EvalError::SubjectSourceExhausted`, and the draw order is specified and pinned by a test. UUIDv7 is now an adapter convention (`runtime_subjects()`), not kernel semantics; `morpholog-core` no longer depends on `uuid`, and `scripts/kernel_purity.sh` refuses randomness crates and clock or randomness reads in it - a tripwire, not a proof. There is no kernel default source, so the input stays visible at every call. The claim is semantic: ambient clock or randomness never supplies a governed value or affects a decision; the kernel's hash maps still seed from host randomness, a defence worth keeping. Explain-on-reject replays the refusing execution's draws. Not done here: recording the draws on the audit row.

### The computational model, written down

**Forced by:** #441, and the third gap the vision assessment found. The reasons Morpholog gave up general computation were scattered across validation, the evaluator and this history, and nothing stopped a convenient feature from eroding them.

**Landed:** [`computational-model.md`](computational-model.md) states the guarantees, each with what makes it true, what checks it, and where the statement outruns the code. Termination is argued by an evaluation rank, not expanded syntax size, because `xor`'s lowering copies its operands. Two new checks: a construct census with no wildcard arm, so a new IR variant does not compile until someone names what it ranges over, and a test that a transformation never reads what it admits. `scripts/kernel_purity.sh` also refuses I/O, network, process, environment and thread access in `morpholog-core`. Writing it down found gaps, filed as #449-#452.

### Validation measures depth before it walks anything

**Forced by:** #450. Validation searched definition bodies for cycles with an uncapped walk before measuring their depth, so a definition nested a million levels deep overflowed `validate()` instead of being refused.

**Landed:** every body's own depth is measured first, with the capped measure the depth guard already used, so a body too deep on its own is refused before anything else walks it - transformation bodies too, since checks beside a cycle finding descend every `for`. `xor` is measured symbolically, at the depth of its lowering, because building the lowering to measure it overflowed. The rule it leaves: structural safety precedes semantic diagnosis - local depth, then definition cycles, then expanded depth, then the name and semantic checks. A test on a fixed small stack pins the deep cases.

### Evaluation takes a validated programme

**Forced by:** #449. The termination argument starts from a validated programme, but the kernel's evaluators and several adapter reads took raw rules, and cyclic definitions handed to `eval_invariant` overflowed the stack.

**Landed:** validation is a capability. Execution hangs off `PreparedProgram` and read-side evaluation off `ValidatedProgram`, each looking the rule up by name, so a transformation from one programme cannot run against another's rules; an undeclared name answers `None`, apart from an evaluation error. `Admission` comes only from a `PreparedProgram`, with private rule fields, and `load_scoped_state` takes a transformation name. The raw evaluators are kernel-internal. The rule left behind: raw IR can be built, parsed, formatted and analysed, but every public operation that takes part in evaluation gets its rules from a validated capability, and the public-API snapshot shows it. Rejected: a public "unchecked" alternative.

### Explanations are held to the decision they explain

**Forced by:** #452, the last evidence gap the computational model named. An explanation's determinism, and its agreement with the decision, rested on reading the code.

**Landed:** the trace differential explains every gallery case twice, from separately prepared programmes, and requires the two explanations to be equal and to name the decision the same execution reached. The outcome is the oracle; no second explanation evaluator was written. Two fixtures add what the gallery lacks: a refusal inside nested loops and a runtime division by zero. Agreement is checked as well as determinism, since two identical wrong explanations would otherwise pass. No change to `explain` was needed.

### The kernel names its semantics

**Forced by:** #451. Since #447 each commit names the rules that admitted it, but nothing named the interpreter contract they ran under, and the kernel's meaning has changed before.

**Landed:** `SEMANTICS_VERSION`, version 1. It moves when the result of any public semantic evaluation can change for the same inputs - proposals, invariant reads and derived reads alike. Its check is a golden of fingerprints over a fixed corpus of named cases, one golden per version: a new answer to the same question fails until the version moves, and a changed corpus fails until regenerated, so lost coverage is visible. Answers are encoded by meaning, never `Debug` or `Display`: a gate by its rule name, an error by its typed kind, so rewording never moves the version. The subjects a proposal draws are part of the question. Rejected: keying the golden by programme hash, whose rendering was itself unversioned (#457).

### Every commit records the semantics that decided it

**Forced by:** the second half of #451. A recorded decision still could not say which contract produced it.

**Landed:** `morpholog.audit.semantics_version` (migration 021, nullable, never backfilled, requires the model hash, activated by a `NOT VALID` check). The acceptance writer stamps `SEMANTICS_VERSION`. Leaf V5 is V4 plus the version as four little-endian bytes, chosen by presence, and evidence packs gained a rung per kind. The audit-row envelope, its golden and the Python client carry the field. A row naming a semantics version this binary does not implement still hashes and verifies, because integrity reads bytes, not the evaluator they name.

### The programme hash is frozen

**Forced by:** #457. Every commit carries `model_hash`, but the encoding behind it was unversioned: a formatter change could give an unchanged rulebook a new hash, and every row stamped with the old one would stop naming its programme.

**Landed:** `sha256:` names canonical encoding 1, frozen as it already shipped, so no hash moved. Its bytes are pinned over a carrier programme built directly as IR, and a gallery golden refuses a moved hash for an unchanged source even when regenerating. Freezing found two fields the encoding leaves out without validation fixing them: validation now refuses an invariant version other than 1, and refuses any sum seed lowering would not give. The rule they share: anything the encoding leaves out is irrelevant to meaning or fixed by validation. Rejected: an encoding-version constant beside the hash - the prefix names the encoding, and a break is a new identifier scheme.

### A derived read becomes a statement anyone can recompute

**Forced by:** #403, and the lender's shape in the borrowing-base example: a lender wants the borrower's facility utilisation and has no reason to trust a figure the borrower sends.

**Landed:** evaluation receipts. `audit receipt` evaluates a derived read over a complete-prefix pack and states the programme's hash, the semantics version, the checkpoint, the query and the answer. `audit verify-receipt` verifies the pack as `verify-pack` does, then checks completeness, the checkpoint, the hash, and recomputes the answer, reporting each layer apart. No new semantics: producer and verifier share `enumerate_derived`. A receipt names the checkpoint, not the pack's bytes, and has one canonical form, anything else malformed; an undeclared query is `query_unknown`, another semantics version `not_re_evaluated`. A receipt proves its statement, not its origin. The assurance verdict is assembled in the CLI where the evidence is verified, never by a library function trusting a caller's report. Details in [`runtime-semantics.md`](runtime-semantics.md) ("Evaluation receipts").

### Every commit records the subjects it drew

**Forced by:** #461, the gap left under guarantee 5. The subjects `new Subject()` handed a commit vanished once the act ran; several draws come in an order that matters, and a draw may reach only an intent or nothing at all.

**Landed:** `morpholog.audit.drawn_subjects` (migration 022, guarded like 021). Every durable execution draws through a recording source only the adapter holds, and the writer takes only that record, so a recorded list always comes from a real execution. Leaf V6 adds the list, chosen by presence, and each pack kind gained a rung. A test re-runs every commit made through every durable path in the kernel from the recorded inputs and requires the same change. The record now holds every input; re-running still needs the programme its hash names and an implementation of its semantics. Unused draws are kept, since refusing them would add a language rule to solve an evidence question.

### FreeTSA witnesses verify

**Forced by:** #386, asked three times by one embedder: freetsa.org's witnesses on every checkpoint reported `unsupported`.

**Landed:** three fixes, each found behind the last. ECDSA P-384 over SHA-512, which the CMS crate cannot parse, has its signature judged here over the exact signed bytes with RustCrypto's P-256 and P-384 crates; every other token stays on the CMS crate, and P-521 stays `unsupported`. Path validation chooses the algorithm from the issuer's key, not the certificate's own, so an RSA root can vouch for an ECDSA leaf. Carried certificates are read as their own bytes, since the CMS crate's re-encoding broke their hashes. The common thread: never discard a distinction the verifier depends on - unchecked against wrong, issuer key against subject key, bytes received against a reconstruction. #469 then made an unjudgeable path `unsupported` and path search try every issuer under a name.

### Each deployment's roles are its own

**Forced by:** #384. Two deployments on one laptop both got the fixed `morpholog_writer`, which then held INSERT on both audit logs. Roles belong to the cluster, not the database.

**Landed:** `init --least-privilege --role-prefix <p>` names the roles `<p>writer` and `<p>reader`; the default prefix keeps the old names. The database records its pair (migration 023, `morpholog.deployment_roles`), and `migrate` re-applies the floor to the recorded roles only and never creates a role. `init` refuses an existing role name this database does not record; creating, recording and granting are one transaction. `migrate` warns when the recorded roles reach another database. Older deployments are recorded with the default pair only when the database grants that floor's fingerprint to them by name. A test proves the boundary across two databases. Rejected: adopting an existing clean role, which nothing could serialise across databases.

### A witness that cannot be genuine is invalid, and an identifier the standard forbids is refused

**Forced by:** #499 and #471, from the FreeTSA work. A signer key of another family than the signature algorithm it names was reported `unsupported`, which never fails a command; and an ECDSA algorithm identifier carrying parameters, which RFC 5758 requires absent, was accepted.

**Landed:** a key-family mismatch is `invalid`, naming the key and the algorithm; an algorithm or curve the build lacks stays `unsupported`. An ECDSA or SHA-224 identifier with parameters is `invalid` before any signature is judged, on the signer and on every carried certificate, read on the bytes as received. The real DigiCert and FreeTSA tokens verify as before.

### The schema baseline is v0.0.14

**Forced by:** a pause, not an embedder. No deployment older than v0.0.14 needs support, yet the binary carried every migration since the first release, guards for databases that no longer exist, and the migration-023 forecast from #514.

**Landed:** the binary starts from migration 23, the head v0.0.14 provisioned, and carries only the migrations beyond it. A database not recording the baseline is refused by name, with the remedy, before ahead or behind is judged. The old migrations, their guards and tests, and the forecast went. `migrate` re-applies the privilege floor on every run. The v0.0.14 schema, vendored as a fixture and migrated to the head, must equal a fresh one. The Python floor rose to 3.12.

### One evidence-pack format

**Forced by:** the baseline's skipped item, and #510 from Grid Mysteries: a pack newer than its verifier came back `malformed`, naming neither the format read nor the release that wrote it. The version ladder existed only so an old verifier refused a newer row, and v0.0.14 as the floor made that moot.

**Landed:** one format, 17, for every kind. A manifest announces it beside its kind and the writing release; the kind is read from the manifest, never the number. Any other format is `unsupported_pack`, judged from a lenient head before the strict parse, naming the format read here, the writer the manifest claims (unverified) and the remedy. The ladder went whole. Not done: reading the older formats - packs already issued in them must be exported again, Jordan's call.

### Closed-loop execution: the record exposes what it could not prevent

**Forced by:** #479, the first rung of the enforcement arc. Record enforcement and credential custody cannot see a second credential or a login used by hand; the strongest deployment takes the venue's record back in as claims and reconciles it with what was authorised.

**Landed:** `examples/21_closed_loop_execution/`. Three parties, each bound to its own login, under a trust root in the biometric example's shape. One intent, `PlaceOrder`, carrying the order's own subject as the venue's client reference; a `Deliverer` driven through the real outbox lease is bound to one venue and fails closed on any other. Five derived reads reconcile, with `Unauthorised` the headline and `Matched` requiring every term. No invariant ties a report to an authorised order, on purpose: the bypass must be admissible as evidence. `audit receipt --derived Unauthorised` is the worked receipt (#508). The outbox doctrine gained the rule that a privileged deliverer's target is never chosen by the intent's arguments. No kernel primitive. Recorded pressure: each finding carries a figure because a derived read must compute a value (#313).

### Admission bounds arithmetic and dismisses a change outside a rule's reads

**Forced by:** the options desk's second report (#527). A risk rule with arithmetic in its consequent was checked whole on every act, so one past breach refused every later act, exits included.

**Landed:** two rules in `morpholog_core::impact`. Arithmetic, an extremum and a builtin read nothing themselves, so they no longer widen a rule's cases; and a conservative plan first asks whether the delta touches any predicate the rule reads, through the canonical footprint walk, so a change outside a state rule's reads leaves it unchecked. A rule over `pre` keeps no footprint, since it judges the change itself. The fingerprint corpus saw decisions change, so `SEMANTICS_VERSION` moved to 2, its first move, and the scoring format to 4. Not done: a sum over an expression, a value lookup and a conditional value still widen.

### A refused gate carries the bindings it was judged under

**Forced by:** the options desk's first ask (#520). A gate refusal carried only the rendered expression, where an invariant refusal carried the bindings; a trader's tool wants values, not prose.

**Landed:** `RejectionReason::Require` and `BindNone` carry a witness: the bindings at the sub-expression the failure walk blames, sorted by name, the actor never among them. The trace's failing part and the witness come from one descent, so they cannot disagree. The witness rides on the rejected envelope, the receipts, the rejection row and `explain`'s gate verdict. Migration 024, the first beyond the baseline, drops the log's gate-witness constraint. The witness is diagnosis, so `SEMANTICS_VERSION` did not move. The compared operands were split to #530.

### Two hints: a figure that re-checks history, and a name the client refuses

**Forced by:** the options desk's third and fifth asks (#522, #524). Tightening a risk limit kept in an invariant beside append-only entries was refused by old entries; and a field named with a Python keyword passed `check --strict` and failed at `generate python-client`.

**Landed:** `ChangeRechecksHistory`: an authored implication whose antecedent joins an append-only predicate with a retractable one and whose consequent reads a value the retractable pattern introduces. Presence alone is not the shape. The hint names both answers: keep it if re-checking history is intended, otherwise gate the admitting act. `ClientName`: a field or parameter the generated client would refuse, with the keyword and member lists moved into the kernel so `check` and the generator share one. The gallery answered both: two permanent events became `append only`, and laytime's `from` became `starts_at`. Not done: following the shape through defined calls.

### A refusal carries the two values its failing comparison compared

**Forced by:** the other half of the options desk's first ask (#530). The witness named the inputs, but printing "risk 152,000 exceeds 97,911" still meant reproducing the rule's arithmetic in the application.

**Landed:** `Compared { op, left, right }` on every `RejectionReason` variant, the envelopes, both `explain` verdicts, the rejection row (migration 025) and the Python client. The operands are evaluated under the bindings the diagnosis stopped at and kept only when they fail the comparison, so a refusal never names one that held; one descent yields the rendered part, the witness and the comparison. The compiled route reads the operands off the rows its diagnosis chose, and the compiled differential holds them to the kernel's by value. `SEMANTICS_VERSION` stayed. `Compared::holds` is a second reading of the operator vocabulary, worth folding into the evaluator's comparison when something forces it.

### The surface syntax on one page

**Forced by:** the options desk's sixth ask (#525). They rebuilt the grammar from the surface-to-IR table, which is written for the kernel, and found `let x = value Limit(#name, _)` only after writing eight gates that each re-bound the limit.

**Landed:** [`surface-syntax.md`](surface-syntax.md), every form once, each inside a small complete programme, with a test that parses, validates and holds lint-clean every block, and a second that proposes the prose's claims through the kernel - the grammar test cannot see a valid programme that is a bad model. It carries the idioms: many small claims, never widen one (#533), and gate or rule. Not done: a generated sheet, which would show spellings without the sentence that says what each is for.

### check names the rules a change may check whole

**Forced by:** the options desk's second report (#528). A compiled rule the plan could not bound refused every later act after one violation, and `check -v` said only the route. The third report asked that `explain` be billed as the second verb after `propose` (#536).

**Landed:** per invariant, `cases`: `bound`, or `whole` with the one construct responsible, in `check --json` and `check -v`. It is read off the impact plans admission already holds, so it cannot drift from the commit gate. `bound` is a property of the plan, not an end-to-end promise: the SQL route can widen a case it cannot seek on. The embedder guide bills `explain` beside `propose`, and says what it is not: an authorisation of the login, or a reservation of the state.

### A dry run names the case a refusal would

**Forced by:** #541, from billing `explain` as the second verb: its invariant verdict dropped the witness the structured reason held. The desk's third report asked for the path when a predicate needs one more field (#533).

**Landed:** the invariant verdict carries `witness` as the gate verdict does, held to the refusal's by a test, and the rendering prints the bindings for both verdict kinds. A client generated before the field must be regenerated, since generated clients are strict. The embedder guide says a predicate with history is not widened: a sibling claim keyed the same way is the move.

### The rows that mention a subject

**Forced by:** the options desk's first report (#523): "why did this order happen" was a string search over the whole audit tail. The same report's lint finding (#526): the committed client package failed the embedder's ruff.

**Landed:** `--mentions <subject>` on `inspect audit` and `inspect rejections`, and `mentions=` on the client's reads. A tagged subject matches at any depth in a row's arguments, claims or intents (audit), or arguments, witness or compared values (rejections), never the actor. The filter has statements of its own, so the unfiltered plans cannot change, and the watermark and cursor apply unchanged. It is an investigation's filter, not an incremental consumer: a sparse subject walks the tail. The client package passes a pinned ruff with no suppressions.

### Either kind of record: `or` in a consequent compiles, and bounds

**Forced by:** two external registers (#379): `A(x, s, c) implies (B(c, s) or C(c, s))` kept a whole programme on the interpreter, and `check -v` named such rules as whole because of `or`.

**Landed:** `or` compiles as the last conjunct of a consequent, directly or as a definition body whose parameters the call binds, every branch evaluated as the kernel does. A conjunct after it, or binding a parameter through it, is `disjunction_shape`. The impact plan treats an `or` outside the top-level antecedent as inert, since case variables come only from the antecedent's conjunction. That moved `SEMANTICS_VERSION` to 3; the scoring identity is `case_bound_admission_v5`. The KYC screens stay whole and interpreted, by design. Not done: `or` in an antecedent, under `not`, `exists` or a sum; `xor`; binding export from a disjunction.

### The session carries the reads a loop wants

**Forced by:** the options desk's third report (#535). A watch loop on the resident session still spawned the binary for the audit tail and the rejection log, and wanted a ping.

**Landed:** session requests `audit` (the one-shot tail, one array per request, capped by `limit`, which also bounds what the database reads), `rejections` and `ping` (checks reachability and that the recorded migrations still suit the binary). A correctable request is a coded receipt; an operational failure aborts. The protocol stays 1. The generated session's `ping` poisons the session on a handshake mismatch. Rejected: a `hash` request, since the session's hash is fixed at its handshake.

### Coverage names what refused, and keeps undeclared names apart

**Forced by:** the v0.0.11 upgrade reports (#382): coverage said "refused: 7 proposal(s)" without naming the gate (#486), and on a shared database listed many transformations another programme declares (#487).

**Landed:** each transformation's coverage carries `refusals`, one entry per kind and rule as recorded, invariants included, summing to `proposals_refused`. A gate's rule stays as recorded, name or rendered expression, since the log cannot tell which. The prose puts undeclared names in their own section, saying the record cannot tell another programme from an earlier version; the JSON is unchanged. Rejected: filtering by `model_hash`, which would hide this programme's own history from before its last edit.

### `check` follows the runtime into closed propositions

**Forced by:** a negated domain in example 21 (#479) showed `check` and the runtime disagreeing (#519): `require not Seen(item) and item = tag` passed `check`, and the first proposal stopped with "unbound variable: item".

**Landed:** `not`, `implies`, `exists` and `forall` export nothing at runtime, so the checker walks each in its own scope, carrying out only kind evidence about names bound before it, as `if(...)` already did. Declaring a quantifier's name no longer binds it; only the body, or `forall`'s source, does. `pre(...)` exports and is unchanged. `check` is stricter: it refuses programmes whose binding flow can raise an unbound variable at runtime. No decision changes, so the semantics version stays.

### `migrate` points at the index check

**Forced by:** an embedder's v0.0.14 upgrade (#507). `provision indexes --prune` created four indexes the earlier release had not required, and nothing had said the requirements could change.

**Landed:** every `migrate` that applies, with or without a migration to run, ends with a `hint:` on stderr naming `provision indexes --check <programmes>`; the report on stdout is unchanged and `--check` prints no hint. `migrate` cannot compute the requirements itself, because they depend on the programmes. A release now plans the gallery's indexes under the previous binary and the new one, and its notes list any difference.

### A claims read names the transition that made each claim present

**Forced by:** etrmbiz's certificate needs "the transition that closed this publication's import", and recovered it by folding the evidence pack (#389).

**Landed:** every claims read - `inspect claims`, tagged and `--named`, the session's `claims`, the client's `claims()` and `claims_named()` - carries `asserted_in`, the transition that admitted the claim while it was absent. Read now, it is the claims table's column, which no audit leaf covers and `audit verify` does not check; read as of a transition, it comes from the audit replay, which tracks it beside the state. Only the replay is checkable: one audit row listing the claim is not enough, since admitting a present claim is recorded too. The claims read has its own `$defs`; `claim_instance` stays the claim itself, shared by audit rows, receipts and derived reads. The field is always on, so a client generated before it must be regenerated. Not done: `audit verify` comparing the column with the replay, which would decide whether it verifies the recorded history only or also what the table holds now.

### Example 13 enforces the trust boundary its README described

**Forced by:** #496. Oversight could be assigned or revoked by any login, a verifier's name needed no restriction to verify, and any login could record a match under the system's name, so the model guaranteed two distinct names, not two authorised people.

**Landed:** the record holds one deployment, proposed by the deployer it names, which restricts both the deployer's and the system's names to their logins. The deployer alone assigns and revokes oversight, only to restricted names, and puts versions into service; the system alone starts and ends its uses. A verifier's name must still be restricted when they verify or decide, and the restriction is `append only`. One deployment, because the actor-assertion grants are global: a second deployment would have handed its deployer the enrolment acts and, with them, an existing verifier's name (found in plan review; pinned by a PostgreSQL test with separate logins). The adapter's own actor-policy tests moved to a minimal programme of their own, so they no longer lean on the gallery's rules. Not done: refusing one login two names, which would constrain deployment architecture rather than the record.

### The record decides an approval's route

**Forced by:** #493. Example 04's approval acts took the document's kind, and its amount, from the approver, so anyone holding both kinds of authority could approve a £4,300 invoice through the unconditional route, or state a smaller amount.

**Landed:** a document is registered once, with its kind and, if monetary, a positive amount (`Document`, `DocumentAmount`, both `unique by (doc_id)` and `append only`). The approval acts take only the document's id and read the rest from the record; a document with an amount is refused the sign-off route. Registering once matters as much as reading from the record: without it, a document approved by sign-off could be submitted again with its amount, and the route would change after the approval (found in plan review). The principle shared with example 13: a proposal carries the decision the caller may make, not the facts that decide whether they may, and those facts must outlive the acts they govern. Not done: who may register a document, which the README states as a trusted input.

### A deadline sealed by a witnessed checkpoint

**Forced by:** Gate Closure (#418): entries must be admitted before a round closes, and the close must come before the real auction clears, but the close rests on the operator firing it on time.

**Landed:** a documented pattern from shipped parts, in the embedder guide. The close emits an intent whose delivery runs `audit checkpoint --witness`, and any witnessed checkpoint covering the close carries an outside authority's time a stranger can compare with the deadline; the note warns off `earliest_attested_at`, which spans checkpoints from before the close. The note says what it bounds (the close, from above) and what it does not (an early close, a prompt one, a complete record). Rejected for now: carrying an RFC 3161 token in each proposal and checking it at admission, which would add cryptography to every commit and an input replay must re-read; it returns if a workload needs per-proposal times rather than a sealed boundary.

### The resident session behind a web server, measured

**Forced by:** Gate Closure (#421), a web site that proposes through one subprocess per request, with a few hundred entries in the minute before a daily close.

**Landed:** a measurement and a pattern, no code. Across three runs of `scripts/embedder_latency.sh` at 300 proposals, a resident session took about half the time per proposal of a one-shot `propose`, recorded in `docs/benchmarking.md` as a ratio with the absolute times as context. The embedder guide gives the web-server pattern with the session the client already has: one session per worker opened after fork, replace a dead session without replaying its interrupted request, retry only `serialization_failure`. Not done: concurrent workers and in-request timing, which depend on the deploying programme and belong to a measurement against it.

### Names for old audit rows stay outside the record

**Forced by:** etrmbiz's upgrade report (#382, #489): rows committed before migration 014 carry no `parameters`, and an optional `inspect audit --names-from <file.morph>` could fill them in, marked as inferred.

**Landed:** the embedder guide says how to read such a row - a candidate programme's `x-morpholog-arg-order` from `schema --all` - and that the names are that programme's interpretation. Nothing on a pre-014 row identifies the programme that wrote it (`model_hash` arrived with migration 020), and a matching name and argument count cannot show the parameters kept their order or meaning. Rejected: the flag with an `inferred_parameters` field carrying the hash of the file used. That hash would name the programme that made the inference, not one shown to have written the row, and the field would add to two pinned envelopes and every client for a closed set of old rows. The record shows what it does not know rather than supply a likely answer.

### A verb moves a deployment to roles of its own

**Forced by:** Grid Mysteries moved three records off the shared role pair by hand, with SQL copied from the install guide (#512).

**Landed:** `morpholog roles move --role-prefix <p> --login <l>...`, one transaction under an exclusive lock on the roles record, so a `migrate` re-applying the floor waits for it or sees its result. It creates the new pair and grants it the floor, then compares everything the old pair holds in this database with what the new pair was granted, and refuses if the old pair holds more, so nothing granted by hand is revoked silently. It also refuses while an old role is a member of another role, since that role may reach this database whatever the move revokes, and while PUBLIC holds anything the floor would revoke from it. Named logins keep their inherit and `SET ROLE` settings on the new roles; the admin option, a login that cannot log in, or one that is not a direct member are refused. The old roles and their memberships stay, since they belong to the whole cluster. Rejected: moving every member of the old pair, which would give another deployment's logins access here again, and a two-step handover, which needs a recorded half-way state no workload has asked for. No client method: the owner runs it, not an application.

### Decoded Python envelopes hold the types they declare

**Forced by:** mypy over the generated client found about 350 places where a decoded value reached a typed field unchecked (#481). An integer passed for a transition id; a decimal-tagged actor became the string `'1.5'`; a malformed tagged value in a batch receipt escaped `propose_batch` as a `CodecError` and lost the receipts before it. An options desk excluded the package from its own type checks for the same reason.

**Landed:** every envelope parser reads its fields through typed readers that hold each value to what `result.json` publishes - types, constants, enums, minimums, non-empty arrays - and coerce nothing; string formats such as a UUID are not read. A payload that breaks one of these raises `EnvelopeError` and nothing else. The value codec reads only the published wire formats and raises `CodecError` for anything else, which the envelope parsers re-raise as `EnvelopeError`. The generated read and payload models check each field's declared kind, and a named read's field set, with `CodecError`. A test mutates every node of every golden against a small walker over the schema, and every mutant the walker refuses must raise `EnvelopeError`; its acceptance is not taken as validity, so the goldens are the accepting side. The type gate checks the whole example package, the worked embedder and a client generated with every supported kind. A session error receipt with an unpublished code now poisons the session too. Review found two contract gaps a missing optional field had hidden: a `transact` refusal's `compared` was refused, which the one-shot client reported as an unknown outcome, and the schema's batch receipt left out the `compared` a batch or session refusal carries. Rejected: casts to quiet mypy, and validating against the schema at runtime, which would need a dependency the client does not carry.

### Three guarantees written down before v0.0.15

**Forced by:** the enforcement work (#474, #476) rests on the outbox delivering a privileged action as one stable intent (#361); civil-date arithmetic was pinned only by hand-picked cases (#360); an embedder with people in its record kept personal data out of claims by its own design, and the next one should find that written down (#422).

**Landed:** two processor tests in which the first worker loses its lease after the downstream answered, accepting or refusing, and a second worker reclaims and delivers: both attempts carry one intent id, idempotency key and payload, and the redelivery adds no audit row, claim or intent. Generated cases through the evaluator for the calendar laws the semantics promise (a zero span, day shifts undone, a month shift keeping the day or landing on the month end, month shifts composing where nothing clamps, period starts and indexes agreeing, refusal by name off the calendar). The embedder guide's section on personal data: opaque subjects, the mapping kept and deleted in the application, which is pseudonymisation and not anonymisation, and what it does not cover.

### A change of representation, verified and attested apart

**Forced by:** the question whether governed derivations need a first-class primitive: premises, a verification rule and a conclusion as a persistent, evidence-bearing relationship. Two textbook exercises (squares modulo 4, the legs of a Pythagorean triple) were the test case.

**Landed:** `examples/22_residue_classes/`, with no kernel, surface, migration or semantics change. The finite statements are invariants checked over a domain the record confirms complete; the theorems over all integers are a checker's reports admitted under a login-bound name, proved whole in a core Lean 4 file anyone can re-check and CI never runs; a conclusion is an append-only claim that three invariants hold to a check, a verified core, and the pairing the programme itself names (review found that grounds alone let any conclusion through); a derived read lists what still stands, and a receipt over it is the historical verification (the CLI suite verifies a receipt against its own pack and against the later history after a withdrawal). Not built: the derivation primitive, since premises are claims, the rule is an invariant and the conclusion an admitted claim; any inference from one claim to another; a conclusion the runtime writes itself, which is the construct-tier question (#279).

### A checkpoint that must contain one transition

**Forced by:** Gate Closure's witnessed close (#583). To witness the close, its deliverer needed a head whose prefix held the close, and read the close's position from `morpholog.audit` with SQL to know whether the head had reached it: the one reason an embedder read the audit table directly.

**Landed:** `audit checkpoint --covering <transition_id>` refuses, with nothing recorded, unless the stable prefix it read contains the transition. A committed row outside that prefix is a coded `checkpoint_refusal` (`transition_not_covered`, with the id and the prefix size) that the deliverer retries after the prefix advances; an id the log does not hold is an ordinary error. The judgement is made in the checkpoint's own snapshot and holds for every head the run can return, since a later head is a longer prefix of the same order. The generated client's `audit_checkpoint(covering=...)` raises `MorphologNotCovered`, retriable. The guide's witnessed-close pattern is two steps: checkpoint covering the close, then witness the returned tree size, whether or not the head was new.

### A definition called by the names of its parameters

**Forced by:** Glasshouse's gate on an `effective by` selector (#411). The generated selector takes the keys, an as-of date and every payload field, so the call read `trade_terms_in_force_on(trade, as_of, org, terms_version, _, _, _, _)`: one hole per field, nothing naming them, and the named pattern a claim may use was refused on a definition by design.

**Landed:** a named pattern on a definition call resolves against its parameters as it does against a predicate's fields, `..` leaving the rest as wildcards; `admit`, `retract` and `value` still refuse a definition. The selector does not exist when the parser runs, so the parser predicts its signature from the declaration through the one function the lowering also builds it from, and the scan is held equal to the lowered programme over every worked example. The IR stays positional, so the hash of a respelled programme is unchanged (the trade lifecycle's pinned hash proves it). The formatter prints a definition call named under the rule it already applies to claim patterns, two or more adjacent wildcards, and settles in one pass.

### One kind inference

**Forced by:** three passes inferred kinds over the same bodies and disagreed. The static checker learned every variable's kind and threw it away; the parameter-contract accessor walked again with its own alias classes and on five shapes gave a different contract, once offering a decimal field the kernel then refused; the seed pass walked a third time to type each sum's empty case, and validation re-lowered a clone to catch a forged seed. Then the accessor, reading its result off the checker, ran the whole check again on every call.

**Landed:** the checker is the one inferencer. Each name in a scope carries a cell its observations go to: a cloned scope keeps the cell, so `or` branches and `require` accumulate flat; `let x = y` shares one cell until a rebinding; a `for` or `forall` binding gets its own, which also types a collection's elements; a definition's signature carries every kind its body used a parameter as. An expectation an outer operator places on `min`, `max`, `abs`, `if` branches or a lookup default is recorded for the contract only and refines nothing, so validation stayed as it was. The checker also records each `Sum` node's typed zero; the seed pass stamps from that record and infers nothing, and a seed lowering would not give is `SumSeedNotFaithful`, naming both. Validation returns the contracts its walk observed and `ValidatedProgram` carries them, so the accessor is a lookup; the handle is no longer `Copy`. Decisions and the hash did not move. Validation widened only where the checker types what the old seed pass could not (an outer-bound quantity, a builtin over a duration).

The arc stopped there, deliberately. Every gallery invariant the compiler refuses, it refuses for a construct it has no rendering for, none for a kind it lacks, so retaining expression kinds for the compiler would be a solution without a problem. Refusing an ill-kinded proposal at the kernel boundary would be a language-behaviour decision, not a refactoring, and was not taken.

### A literal is a value

**Forced by:** the IR carried literal text into the semantic engine. `Value` held decimals, dates, timestamps, durations and spans as strings, so the evaluator parsed them against every candidate claim, the checker parsed them again for the period and rounding rules, the compiler through a fallible conversion, and a malformed literal in hand-built IR validated and then silently never matched. Interpretation was spread over parsing, validation, evaluation and compilation.

**Landed:** `Value` holds parsed payloads, with a decimal literal's identity including its scale. The source frontend and the IR builder read literal text once; the lexer fixes the shapes of decimals, dates and timestamps and the expression parser the `duration(...)` and `span(...)` constructors, and a text the type cannot hold exactly is a diagnostic at its span, never rounded in. The conversion to a runtime value is total, the evaluator's literal parsers are gone, and the checker reads values. The canonical encoding moved to 2: it renders each literal in its one spelling, so `span(P1Y)` and `span(P12M)` are one programme where they were two, and a programme whose literals were not already canonical got a new hash (in the gallery, the charging-years example). The identity golden records the encoding so that move is told apart from an accidental one. Decisions did not move. A literal is a valid value, not necessarily a surface-spellable one: hand-built IR may hold a negative decimal, and the round-trip promise is for programmes parsed from source.

### Unary minus

**Forced by:** Gate Closure's score rules (#582), each flooring at minus one hundred as `max((0 - 100), ...)`. The roadmap had held unary minus back for a programme that read better with it; a floor in every score rule was that programme.

**Landed:** surface sugar only. The expression grammar gained a unary tier between the primaries and `*`: `-x` lowers to the existing subtraction from the literal `0`, binds tightest, and nests. No IR node, checker arm, evaluator arm or compiler arm was added, so `-x` is lawful exactly where `0 - x` is, decimals only; a quantity or duration negation earns the subtraction's own refusal, and a unit-preserving negation waits for a programme that forces it. `-100` is that subtraction over the literal `100`, not a signed literal, so a term position still takes no sign and one spelling has one IR shape. The formatter prints a subtraction from the literal `0` as `-x` in source form only; `0.0 - x` keeps its spelling because the literal differs in scale. The canonical encoding and the formatter share one renderer, so the sugar is keyed on the source-form flag and a test holds a programme spelled `0 - x` to the hash it had before. The gallery carrier is the credit note example 15 had listed as not covered: a billed line reversed by an act that reads the committed figures and admits them negated, `-net_gbp`, so the caller states no figure and an invariant holds a credit to its line's own pennies, sign turned, never re-rounded. Decisions did not move; the sugar moved no hash, and the example's hash moved with its source.

### A grid over instants

**Forced by:** Glasshouse's pre-trade position limits (#563): a book's net MW must stay within its cap in every delivery hour. The rule was expressible, with the embedder computing the hours outside the rule book, passing them as a collection on every proposal, and three lines of encoding proving the collection was the window's grid.

**Landed:** `periods(from, until, step)`, a builtin returning the start of every step-wide period a window meets, anchored at the Unix epoch in exact nanosecond arithmetic, as a collection of timestamps; the checker types its loop binders as timestamps before their first use, through a `let` as well as directly in `for`. The grid's size is the rule's own, not the proposal's: a call is lawful only with a literal step and after a gate bounding its window with a literal duration, matched through the checker's cells, and validation refuses it otherwise. Review had proposed a kernel ceiling on the count; it was declined, since a resource constant would decide whether a proposal is decided at all and so become part of the language. The lint tier hints where a bound still allows a very large grid. A window off the grid meets one period more than its length holds, so alignment is one identity in the rule rather than three encoding lines. Not done: the invariant form, which needs a collection-valued `in` source, and the calendar generator the issue also asked for, now #602. The worked example is `examples/23_delivery_hour_limits/`: the cap hour by hour as a capture gate, a day product taking its window from a published calendar day of 23, 24 or 25 hours, and the sum-of-overlaps shortcut shown wrong by a trade that fits.

### Recorded in error

**Forced by:** an options desk's blotter (#534). A refusal that should not have happened left a cancel on an append-only record, true as an event and wrong as a conclusion. Retraction is banned by the discipline, and supersession fits a figure with a pointer, not an event. Every embedder would invent the idiom; it needed a name and a worked example.

**Landed:** no construct. An erratum is an append-only claim about a claim, `RecordedInError(claim_id, reason)`, naming the recorded line by the line's own identity, never the thing the line describes, recorded once and never withdrawn. Rules read the line through a definition of standing, the line with no erratum against it, so the record holds two things at once, both permanently true: the line was recorded and admitted, and it is not to stand. What was true is then admitted through the ordinary act. Two rules are written by hand: an erratum names a line that exists, and uniqueness over standing lines, which a declared `unique by` cannot express. Review caught that a rule whose antecedent is only definition calls binds no case variable and is checked whole; the rules start from the recorded claim instead, and the example's test holds every plan to bound. A discipline clause (`corrected via`) was declined: two hand-written lines in one example do not force one, and each rule chooses for itself whether it reads standing or history. The worked example is `examples/24_order_blotter/`.

### Rehearse

**Forced by:** an options desk and a research record (#521). `evaluate` scores invariants, and their discipline lives in gates: "what would a 5% risk cap have refused" is a question about the act, which only re-running the actual proposals answers. The live-ledger demonstration (#611) did it by hand, replaying a record row by row under the programme each row names.

**Landed:** `morpholog rehearse <candidate>`: one fold over the audit log, one ordinary `propose` per row against the state the rows before it build, one report. Each row is `same`, `different_delta` (effective deltas and ordered intents compared), `would_reject` (the first refusing rule, as `propose` stops there), `would_error`, or `unreplayable` (a missing act, an arity the parameters refuse, draws beyond the record; `SubjectSourceExhausted` is translated on purpose so a missing input never reads as a candidate defect). Recorded draws feed the subject source, so a minted id is the recorded one. The database and pack paths share the fold; the report is a pinned envelope with its golden and the generated client's `rehearse()`. Review set the contract before code: the prefix is the canonical audit order, not the snapshot a concurrent writer saw, and the report says so (`canonical_audit_prefix_v1`, with a unit test of two overlapping writers); whole programmes are compared, with no rule isolation; identity is promised only under the candidate's own hash, this binary's semantics, recorded draws and a serial history. A kind error inside execution is `would_error`, since the kernel has no contract check short of running the act; the counterfactual timeline stays #402. No kernel change.

### Retract by key

**Forced by:** Grid Mysteries' `drop_row`, binding fifteen cells to retract one row, and Gate Closure's submit-versus-resubmit pairs, two acts each because the resubmitting act "cannot retract the prior claim without binding the figure" (#390).

**Landed:** nothing in the kernel or the surface: a retract has taken a pattern since the first release (`retract Row(row: row, ..)` retracts the matching claim, every match goes, none matching is a no-op), and both embedders wrote the long form because nothing they read showed it. The syntax sheet's act example now does, and `check` hints `BindOnlyToRetract` when a `bind` matches a claim whose fields are read only by the retract that follows and whose kept fields cover a uniqueness key grounded before it, spelling the pattern form and saying that a `require` keeps the refusal a `bind` gave. Review caught the first draft hinting without the key: a `bind` over several matches errors where a pattern retract removes them all, which a runtime test now pins. The gallery was already clean of the shape. Discoverability, not capability: the third ask of this kind to dissolve into a feature the language had.

### Compiled arithmetic

**Forced by:** Gate Closure's recomputed scores (#419). The record should recompute each score an untrusted app submits and refuse one that is not exact, with `abs`, `min`, `max`, `round`, `*`, `/` and sums; but one such rule would turn an all-compiled programme interpreted. Twenty-two gallery invariants ran interpreted for arithmetic or a builtin, and no embedder had one.

**Landed:** decimal and quantity arithmetic and `abs`, `min`, `max`, `round` compile. Measured first, against exact fractions over random operations: rust_decimal 1.43 gives the exact result rounded half to even to the largest scale at most 28 whose mantissa fits in 96 bits, an error only when scale 0 does not fit, so the kernel rounds silently in addition and multiplication too, and its `round` chains steps that each round. Migration 026 adds five guarded SQL functions that do the same (`decimal_fit_v1`, `decimal_div_v1`, `decimal_round_v1`, `decimal_min_v1`, `decimal_max_v1`), held to the kernel by a property test over every operation; PostgreSQL's own division is never used. Each value is an amount and a unit read from its own tag, the amount NULL exactly where the kernel errs; SQL decides and pins the binding, and the kernel names the error and the reported values over it, through `eval_value_over`, which takes each sum's result by node so the kernel's own order decides which error wins (substituting a literal would not: in `(1 / 0) + sum(...)` the kernel names the division). Review moved `round`'s error to the branch taken and kept `min` and `max` off `LEAST`, which skips a failed argument, so they became two more strict functions, each operand written once. Code review then found a comparison with a computed side still judged by the declared flavour, so drifted values could disagree with the kernel: every comparison of decimals and quantities now reads both sides as an amount and the unit each value carries, which also closed the same gap in plain column comparisons, where two drifted values had failed with an operational error the kernel never raises. It also found nested builtins repeating their operands' SQL; with the strict functions a chain grows linearly, and a size budget refuses whatever still grows faster, such as a long chain of products, by name. Eleven gallery invariants moved to `compiled`; time arithmetic stays interpreted under its own name. Decisions did not move.

### One audit order

**Forced by:** the replay audit (#625). Rows sorted by `committed_at`, which is when a writer's transaction started, not when it committed. Two overlapping writers could therefore replay in the wrong order: a retraction that started first and committed last replayed before a no-op admission of the same claim, so `verify` kept a claim the table no longer held. The tail's watermark had the same root and needed every writer's session in `pg_stat_activity`, which a managed host hides, hence `--writer-role`.

**Landed:** migration 027 gives every transaction one place in a single order when it writes its first audit row, taken under a short global lock and held as a transaction-scoped advisory lock until commit, and each act an ordinal within it. Every row records the operations its commit performed on the claims table, and replay applies those, not the proposed lists, so it rebuilds the table even when writers overlapped. The watermark is the lowest order still held, read from the lock table, so `--writer-role` is gone. The migration ends the legacy prefix with control records: the claims the table held, each with the transition that admitted it, as `morpholog.baseline` chunk rows of at most ten thousand claims or four mebibytes, then `morpholog.activate` naming the legacy prefix, the chunk count and a digest over the chunks' leaf hashes, all at order 1 in one transaction (the baseline was one record's JSONB array until measurement found its ceiling near a million claims); a fresh database has none, since it is ordered from its first row. A genesis record for fresh databases was weighed and dropped, as it touched every test that writes history for one case. `verify` gained `indeterminate`, for a legacy history whose replay does not reach that baseline, which overlapping legacy writers can cause lawfully, and lists conflicts, such as a delete of a claim the history never held. Leaf encoding V7 and pack format 19 carry the new fields, and format 17 packs still verify; format 18, which only development builds wrote, is refused by name. Review set the plan before code, including that a no-op admission takes no lock and is never checked against the state.

### Staged migration

**Forced by:** measuring migration 027 on fabricated histories. Its `ALTER TABLE` and the legacy Merkle pass ran in one transaction, so the ACCESS EXCLUSIVE lock the `ALTER` takes held every plain read of the audit table for the whole pass: 34 seconds at three million rows, linear, with about five percent of it in the DDL that needed the lock. The same runs showed the activation record's single JSONB baseline failing near a million live claims, which the chunked record of a later change answers.

**Landed:** the migration prepares in idempotent steps with the database in service (catalog-only columns under a bounded `lock_timeout` retry, constraints added `NOT VALID` and validated afterwards, indexes built concurrently with invalid ones repaired, the order's functions; an existing object under one of the step's names is held to the shape the step would give it, another shape refused by name; the legacy Merkle frontier, built in passes below the census's start-time horizon and saved after every page, so a writer that commits late still lands at its place and the cutover hashes only the rows since the last pass, checking that the rows under the saved frontier are still as many as it holds; one runner at a time under an advisory lock, reading the migration state again once it holds it), recording each step in `migration_progress`, and only then runs its own transaction, which holds EXCLUSIVE locks: writers wait, readers do not. That transaction streams the claims table into baseline chunks, one in memory at a time, and writes the record last, so the runner's memory is bounded by a chunk whatever the table holds; the history check, live and offline, holds order 1 to that shape, `activation_faulty` offline, and as-of refuses a chunk's id as a coordinate. The gates that closed the work: the real binary killed at chosen statements (a cutover before its commit, a client lost after it, a cutover waiting for a writer, a frontier pass at its save, an index build at its last wait), the v0.0.16 release and a writer of its row shape at each side of the cutover, and the capacity harness at one and two million live claims and at wide claims, where the migrator stayed near 60 MB and `verify-pack` near 25 MB, while the live verifier, checkpoint and export took gigabytes: the audit pager reads a thousand rows a page, and one page holds every baseline chunk decoded at once; v0.0.16 checkpoints a million rows in 40 MB. Bounding a page by its chunk rows is the fix, left for its own change. The crash gate found one fault: a killed cutover had consumed order 1 from the sequence, which does not roll back, and every later attempt drew 2 and was refused; the cutover restarts the sequence in its transaction. `migrate --prepare` stops before that transaction; `migrate --check` carries a preflight with the preparation state, estimated costs from the measured constants, and refusals. The cutover fails closed on a census taken as two ordered statements, sessions then prepared transactions, so a transaction prepared before or during the census, or a session the login cannot see, refuses it; both are tests, and the prepared-transaction one is mandatory on CI.
