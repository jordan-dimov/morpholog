# What the worked examples forced into the language

Design archaeology. Companion to [`scope-and-ambition.md`](scope-and-ambition.md) (forward-looking doctrine) and [`runtime-semantics.md`](runtime-semantics.md) (what the kernel means). This file records each design move and the worked example that forced it.

The early entries are compressed. Detailed PR-by-PR retrospectives for the pre-parser arc lived here while the kernel was being shaped; once the parser arc started, the relevant rationale either moved into doctrine (and is referenced inline) or was distilled to a stub. The parser-arc entries below are kept at fuller detail because that work is still active and recent.

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

- `Neq`, `In`, and claim-call arguments are `Term`-only in the IR, so `a + 1 != b` and `Foo(x + 1, y)` are parse errors, not silently ill-shaped IR.
- No `true`/`false` literals: the IR has no `Value::Bool` to lower them to. Reserved at the lexer, rejected at the parser.
- `value Pred(args) [default expr]` is claim-pattern-shaped, not a general `value(target | body)` query - the latter would be more expressive than `Expr::ValueOf` can represent. (A doctrine-table row had the wrong shape; corrected before any parser code committed it.)
- Transformation parameters carry no kinds, because `Transformation.parameters: Vec<String>` doesn't.

**Layout.** Blocks are indentation, not braces. Spaces only - a tab in indentation is a diagnostic, refusing the tabs-vs-spaces ambiguity rather than guessing. Parens disable layout, so a long expression spans lines freely inside them. No virtual `Newline` token: every statement and declaration starts with its own keyword, which is the boundary. Nested blocks (a `for` body inside a transformation) use a `recursive` parser bounded by matched `Indent`/`Dedent` pairs; each `: <body>` consumer (invariant, `exists`, `forall`) accepts inline or indented form via a local `(Indent body Dedent | body)` choice.

**Literals and quantifiers.** `@YYYY-MM-DD` for civil dates, `#NAME` for subjects - sigils resolve the arithmetic ambiguity (`2026 - 05 - 22`) with no lexer complexity; decimals are carried as strings to preserve exactness. `forall x in <source>` auto-lifts a bare-variable source to `Expr::In(Var(x), source)` so the kernel can iterate, while a claim-shaped source passes through. `in` is positionally disambiguated: structural inside `forall`/`for`, a membership comparator elsewhere. Quantifier bodies are greedy to the end of the enclosing expression; composition with outer terms needs parens. `sum`'s target is restricted to a variable name.

**`on_or_before` for date comparison.** Civil-date `<=` lowers to `Expr::DateLe`, a separate kernel primitive from decimal `Le`. Rather than dispatch `<=` on operand kind (the parser has no type environment, and the `DateLe` design deliberately separated the two), the surface uses a keyword that reads as a regulatory clause: `effective_from on_or_before randomisation_date`.

**Round-trip property test.** `tests/round_trip.rs` runs every `all_programs()` entry through `format_program -> parse_program -> assert_eq`, catching formatter drift and parser regressions together; a new worked example extends coverage for free. Building it forced the formatter to emit canonical *parseable* text (infix booleans, `admit`/`bind` verbs, `#`/`@` sigils, `on_or_before`) instead of debug-style output, and surfaced the actor-shadowing fix: transformation parameters named `actor` collided with the auto-mapped `Term::Actor`, so they were renamed `principal` - clearer anyway, since the parameter is the *subject* of an authority claim, not the proposer.

The arc is complete for v0: every worked example parses end-to-end. `morpholog run <file.morph>` (proposing a user programme against PostgreSQL) and the enriched `morpholog check` build on the closed surface.

## After the parser arc

Recent moves, kept at fuller detail.

### `Expr::Or` predicate-shaped disjunction

**Forced ahead of an example.** Second ahead-of-example deviation in the codebase, after [`Transition.actor`](#transition-value-object-and-auditactor). The chess transition-invariants example landed in the next PR uses `Or` for `SingleCapturePerMove` (`after = before or after = before - 1`); landing it standalone kept that PR's design conversation focused on `Pre`. Independent rationale: every other predicate-shaped composer (`And`, `Not`, `Implies`, `Exists`, `Forall`) is first-class. De Morgan's `not (not A and not B)` works but punishes the natural surface form for a minimalism that was never the point.

**Landed:** `Expr::Or(Vec<Expr>)` mirroring `Expr::And`'s flattened shape. `find_disjunction` concatenates each branch's binding extensions, no deduplication (matches `find_conjunction`'s convention). Surface keyword `or` between `and` and `implies` (standard precedence). `find_failing_subexpr` returns `None` for `Or` - when a disjunction fails, every branch failed; picking one to blame would mislead.


### `Expr::Pre` transition-invariant primitive

**Forced by:** the chess transition-invariants example, after Murat Demirbas's [Chess invariants](https://muratbuffalo.blogspot.com/2026/05/chess-invariants.html) post. State invariants (over one state) and transition invariants (over a pre/post pair) are distinct kinds; the kernel could only express the first. "MoveCount advanced by one" or "PieceCount fell by zero or one" needs both states.

Chess was chosen as forcing example over a business retrofit: the ledger has no admitted balance predicate (only the `TrialBalanceRow` derived claim, which invariants can't see); a per-journal conservation rule would duplicate `balanced_posted_entry`; a clean retrofit needed admitted operational state, which is its own design move. Insurance retrofit followed in the next PR.

**Landed:** `Expr::Pre(Box<Expr>)` - a wrapper that opts a subtree into pre-state evaluation; the default outside remains the candidate state. Chosen over TLA+'s primed-variable style because Morpholog has only one state at a time by design; the wrapper inverts that: post is default, pre is opt-in. Three knock-on benefits: zero migration (existing invariants unchanged), composes with `Sum` / `ValueOf` / quantifiers without IR surgery on them, and preserves the deliberate non-commutativity of `pre(forall ...)` vs `forall ... pre(...)`.

`EvalError::PreStateUnavailable` is the diagnostic when `Pre` is reached without a pre-state in scope (derived-claim body, transformation `require`, standalone evaluator call, inner of nested `pre`). Phrased about evaluation context rather than AST position so future contexts that carry both states (transformation postconditions, trace assertions) can share the primitive without IR change. No new invariant kind in the IR - state vs transition is descriptive, derivable from the body.

**Considered and rejected:** an `EvalContext` struct refactor before adding `Pre`. ChatGPT pushed for it; rejected on subtraction grounds (one new context value is three away from the abstraction earning itself). The next PR added a second contextual arg and the refactor became honestly forced.


### Insurance retrofit: `PolicyHeadroom` conservation via `pre(...)`

**Forced by:** closing the loop on the `Expr::Pre` PR's deferred business case. Insurance was chosen over ledger (ChatGPT's call): the example already lives around aggregate-cap discipline, `Policy` is a natural sibling to a new operational counter, and the ledger retrofit would need a wider remodel.

**Landed:** new admitted predicate `PolicyHeadroom(policy_id, remaining)` paired with `at_most_one_headroom_per_policy` for structural uniqueness and `paid_implies_headroom` for existence (the latter added in response to Copilot's review - the conservation invariant's pre/post guard is vacuously true when no headroom claim exists for a policy). `issue_policy` admits initial headroom = aggregate_limit alongside `Policy`. `authorise_settlement` reads current headroom via `bind_one`, retracts it, and asserts the decremented one; the existing aggregate-limit `require` is kept (admission gate, distinct role from conservation).

Transition invariant `headroom_consumed_by_payment`: per-policy delta conservation, `after = before - sum(amt | SettlementPaid(p, _, s, amt) and not pre(SettlementPaid(p, _, s, amt)))`. An earlier per-row draft (`after = before - amt`, one binding tuple per new payment) was caught as too weak in ChatGPT's review: a multi-payment transition admitting two same-amount settlements while decrementing headroom once would pass each per-row equation while consuming twice the headroom it credits. The sum form catches that and the rest of the delta-conservation bug family.

**`EvalContext` evaluator refactor (bundled, first commit).** With `Pre` landed the evaluator had two contextual args (`pre_state`, `actor`) plus state and bindings; `find_matches` carried five parameters. `EvalContext<'a> { state, pre_state, bindings, actor }` with `with_bindings` and `enter_pre` helpers collapses recursive call shape and folds the "enter a Pre subtree" pattern into one method. No behaviour change; preserved across sync and PG test surfaces.

**What stays out:**

- A `PolicyHeadroom(p, r) implies r >= 0` state invariant. Not added; the existing require's admission gate plus the conservation invariant cover the business case. Would land if a worked example forces a non-require path to headroom mutation.
- A retrofit onto the ledger. The structural reshape would be wider; deferred until a worked example actively needs it.


### Enriched `morpholog check`: kind/type compatibility

**Forced by:** the input/output boundary work shifted the leverage. With non-Rust integrators able to hand Morpholog a `.morph` file and get a deterministic outcome back, authoring trustworthiness became the bottleneck. Decimal-in-Subject-slot, `<=` against a date, arithmetic on a subject literal, `Eq` between incompatible kinds - all only surfaced at runtime as `EvalError::TypeMismatch`.

**Landed:** `morpholog_core::kindcheck` module; `validate_program` now merges its output with the structural pass into a single `Vec<ValidationError>`. Four new variants (`PredicateArgKindMismatch`, `OperandKindMismatch`, `VariableKindConflict`, `ExpectedValueExpression`) share the existing CLI emission path. Inference: `InferredKind { UnknownOrAny, Known(PredicateArgKind) }` with refine-on-observation. `Any` stays unconstrained (a variable seen first through `Any` refines on its next concrete use); the alternative ("`Any` everywhere lets later uses slip through") was rejected for losing the most valuable refinement pattern.

The walker mirrors the runtime quartet: `Require` against a cloned env (no export), `BindOne`/`Let` against the live env. `Sum`, `Forall`, `Exists` shadow their binding via `KindEnv::with_shadow` so the loop-local name cannot collide with an outer variable of the same name; refinements to *other* variables in the body leak through normally. Without that fidelity the kind checker would either reject correct shadowing or pass programmes the runtime rejects.

**Considered and rejected:**

- A symmetric `ExpectedPredicateExpression` variant for value-shaped-at-predicate-position. Deferred without a forcing example; the runtime still catches it as `NotPredicate`, and Layer 2 (unbound-variable detection) is the natural home.
- Bundling Sum/ValueOf with comparators. Sum's binding-flow story is its own slice.
- A `--strict` flag for hint-grade output (unused declarations, sum-binding-not-in-body, fuzzy suggestions). That is Layer 4; Layer 1's job is the runtime-error mirror, no more.

**What stays out:**

- Source spans on `ValidationError`. The IR drops parser spans on lowering; threading them through is its own design conversation.
- Element-kind correlation for `Collection`-typed variables. The current `In` check pins the source side to `Collection` but does not yet propagate element kinds to body bindings.


### `IntentDecl`: outbox vocabulary as a declared first-class kind

**Forced by:** the KYC sanctions/PEP screening example, where every interesting moment fires an intent to a distinct downstream consumer - `ScreeningRequested` to the external provider, `MatchRaised` to the analyst queue, `CustomerOnboarded` to core banking, `CustomerRejected` to compliance reporting. Misspelling any of those is a real failure mode: a mistyped `SARFiled` would silently route to nowhere, no analyst would ever see it, and the regulatory breach is invisible until a downstream auditor notices the gap. Predicates already had `PredicateDecl`; intents were still stringly-typed, and the kindcheck layer skipped `Stmt::Emit` entirely.

**Landed:** `IntentDecl { name, args: Vec<ArgDecl> }` mirroring `PredicateDecl`; `Program.intents: Vec<IntentDecl>`; parser surface `intent X(args)`; formatter round-trip; structural validation (undeclared / arity / duplicates) tagged via the new `VocabularyKind { Predicate, Intent }` enum; kindcheck wiring for `Stmt::Emit` that calls the same arg-kind checker as `Stmt::Assert` but against the intent vocabulary. Strict from day one: every `emit X(args)` must target a declared intent. Every pre-existing worked example gained intent declarations.

The `VocabularyKind` enum was the foundational move. Predicates and intents share the same diagnostic shapes - undeclared reference, arity mismatch, duplicate declaration, arg-kind mismatch - so rather than parallel intent-specific variants, the existing predicate variants were renamed (e.g. `UndeclaredPredicate` -> `Undeclared { vocabulary, name, context }`) and parameterised. The two vocabularies share those diagnostics today; a third declared vocabulary would slot in without further renaming. `PredicateArgDecl` was simultaneously renamed to `ArgDecl` since the struct is structurally generic - the "Predicate" prefix would read as wrong the moment `IntentDecl` reused it.

**Considered and rejected:**

- *Reusing the predicate namespace for intent shapes.* Predicates describe admitted claim vocabulary; intents describe outbox-effect vocabulary. Both pass through the same kindcheck mechanics but the distinction is load-bearing in the audit trail.
- *Four new intent-specific error variants.* Larger surface for the same diagnostic shape.
- *Optional / opt-in IntentDecl for v0.* Lower migration cost short-term, but the whole point of the layer is that misspellings fail validation.

**What stays out:**

- An `inspect intents` CLI subcommand mirroring `inspect predicates`. Useful but not load-bearing; deferred until the legibility tooling work picks it up alongside the other inspectors.


### Static-analysis pass: one visitor, binding flow, actor context, and a depth floor

**Forced by:** the kind/type-compatibility check had landed as a second walker bolted next to the structural pass, and the checks due next - unbound variables, the value/predicate shape mirror, actor-in-wrong-context - all needed the same traversal and the same notion of "what is in scope here". Running them as separate walks would mean re-deriving binding scope each time and drifting from the runtime's binding rules each time. The forcing function was honest: the moment a second binding-aware check came due, the two-walker shape stopped paying.

**Landed:** the `kindcheck` module became `check`, and `validate_program` is now a duplicate-declaration pass plus a single `check_program` traversal - the structural, kind, binding-flow, shape, and actor-context checks all ride one walk over each body. The visitor (`CheckCtx`) carries the declared vocabularies and accumulates errors; a `Scope { kinds: KindEnv, bound: BoundEnv }` threads kind inference and runtime-binding state together, cloned at the same boundaries (`require`, `sum`, `for`, `or`-branches) so the quartet's non-export rules fall out of the structure rather than from special-casing.

Unbound-variable detection (`UnboundVariable`) follows the evaluator exactly, which is where the subtlety lived. Two rules had to match `find_conjunction`/`find_disjunction` precisely or they would reject correct programmes:

- A disjunction exports only the *intersection* of names its branches bind. The runtime threads each conjunct's witness into the next, so after an `or` a name is guaranteed bound only if every branch bound it. The KYC `(clean or adjudicated_clear) and (on_date on_or_before expires)` invariant relies on this: both branches bind `expires`, so it reaches the comparator. A "branches export nothing" rule flagged it; a "first branch exports" rule would pass programmes the runtime rejects.
- `in` is a generator, not a use. `sum(x | line in lines and LineAmount(line, x))` leaves `line` unbound at the `in`, and the runtime binds it to each item; treating the element as a use flagged the settlement-netting example.

Both were caught by running the check over every worked example as the regression gate - a false positive there breaks `program.validate()` on real `.morph` source, so "zero example regressions" was the bar, above the unit tests.

`ExpectedPredicateExpression` landed here too, where the kind-compatibility entry predicted it would: the mirror of `ExpectedValueExpression`, reusing the same `short_expr_shape` label, flagging a value-producing expression at a predicate position before the kernel raises `NotPredicate`. Actor-in-wrong-context (`ActorNotAvailable`) flags `Term::Actor` in an invariant or derived-claim body - the static face of `EvalError::UnboundActor`.

Separately, `Program::validate` gained a nesting-depth floor (`NestingTooDeep`). The recursive evaluator and the check walk both descend one stack frame per nesting level, so a pathologically deep body - a long `not not ...` chain, deeply nested `for`s - could exhaust the stack during `propose`. The guard runs first and short-circuits, because the walk it protects is itself recursive; its own depth measure spends a fixed budget and bails the instant it runs out, so it cannot overflow on the input it exists to reject. This is the enforceable form of "validate untrusted IR before proposing it": `propose` does no programme-level check of its own and trusts the IR it is handed.

On the persistence side, the duplicate-intent collision - one transformation emitting the same intent twice, colliding on the deterministic outbox idempotency key - is now `PgError::DuplicateIntent` rather than an opaque `PgError::Database`, so a caller can tell a modelling bug from a transient database error without string-matching.

**Considered and rejected:**

- *Keeping the two-walker shape and adding more walkers.* Each binding-aware check would re-derive scope and risk its own drift from the runtime. One traversal over one scope was the subtraction.
- *Populating `BoundEnv` during the unification commit, before unbound-variable detection forced it.* The unification was proved to preserve behaviour first; the bound-env field was added only when the next layer forced it.
- *A parser-side input-depth guard in the same change.* The `propose` path commits state and is the documented untrusted-IR contract, and it is now covered; the `.morph` parser is a weaker threat in v0 (you author your own files), and chumsky exposes no recursion-depth hook, so the guard would be a grammar-coupled heuristic. Deferred.
- *An allowlist over the bench's SQL.* On review every bench statement is a static query string with bound parameters and bounds-checked integer conversions - no dynamic SQL to harden, and an allowlist would have been structure without a problem.

**What stays out:**

- `--strict` lint-grade hints (unused declarations, `sum(x | body)` with `x` absent from `body`, fuzzy "did you mean?" suggestions). The remaining check layer; this work's job was the runtime-error mirror.
- Source spans on diagnostics. Unchanged: the IR still drops parser spans on lowering.


### Counting via a constant `sum` target (and dropping `Sum.binding`)

**Forced by:** the chess example wanted to count - "how many pieces are on the board?", "how many kings does white have?" - and nothing in the kernel could express it. The doctrine had already flagged the relaxation (`sum`'s target was "restricted to a variable in v0; relax when a worked example forces it"); a material census was that example.

**Landed:** the surface `sum` target now accepts a decimal literal as well as a variable, so `sum(1 | body)` counts the bindings the body produces. The evaluator already resolved the target term once per match, so a literal target sums 1 each time - counting needed no evaluator change, only a parser relaxation and a formatter that renders the target from the term. Chess gained `piece_count_matches_board` (the hand-kept `PieceCount` must equal `sum(1 | PieceAt(...))`, so the counter can never drift from the board), `exactly_one_white_king` / `exactly_one_black_king` (count `= 1`, which forbids capturing a king - strictly stronger than the at-most-one rule it replaced), and an at-most-eight-pawns-per-colour bound. So chess now forces counting in addition to `Expr::Or` and `Expr::Pre`.

The same change retired `Expr::Sum`'s `binding` field. It duplicated the target variable's name, the evaluator ignored it, and the formatter carried an `assert!(value == Var(binding))` with a "cannot round-trip" caveat. Once the formatter rendered the target from `value`, `binding` was vestigial - a loose end the counting work exposed and removed.

**Considered and rejected:**

- *Strict comparators (`<`, `>`, `>=`) for chess.* Tempting, but derivable: `a > b` is `not (a <= b)`, and `a >= b` is `b <= a`. They are surface sugar, not new capability, so they have not earned their place.
- *A dedicated `Expr::Count` primitive.* Redundant with `sum(1 | body)`; the doctrine's anticipated move was to relax `sum`, not add a sibling that does the same arithmetic.

**What stays out:**

- A general expression as a `sum` target (`sum(debit - credit | ...)`). The IR's `value: Term` holds only a term; an expression target awaits an example that needs it.
