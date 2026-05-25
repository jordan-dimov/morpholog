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

Post-parser kernel and tooling moves. Compressed to Forced-by/Landed stubs once they settled into history (the per-PR detail and the considered-and-rejected alternatives live in git); the recent legibility arc, from the explanation engine on, is kept at fuller detail while that work is active.

### `Expr::Or` predicate-shaped disjunction

**Forced by:** chess `SingleCapturePerMove` (`after = before or after = before - 1`); landed a PR ahead of chess to keep that PR focused on `Pre`. Independent rationale: every other predicate-shaped composer was already first-class, and De Morgan's `not (not A and not B)` punishes the natural surface form.

**Landed:** `Expr::Or(Vec<Expr>)` mirroring `And`'s flattened shape; `find_disjunction` concatenates branch bindings (no dedup, matching `find_conjunction`); surface `or` at standard precedence. `find_failing_subexpr` returns `None` for `Or` - when a disjunction fails every branch failed, so blaming one would mislead.


### `Expr::Pre` transition-invariant primitive

**Forced by:** the chess transition-invariants example (after Murat Demirbas's [Chess invariants](https://muratbuffalo.blogspot.com/2026/05/chess-invariants.html) post). State invariants (over one state) and transition invariants (over a pre/post pair) are distinct kinds; "MoveCount advanced by one" needs both states. Chess was chosen over a business retrofit because no existing example had admitted operational state to compare across the transition.

**Landed:** `Expr::Pre(Box<Expr>)` - a wrapper opting a subtree into pre-state evaluation; post stays the default, pre is opt-in (the inverse of TLA+ priming, because Morpholog has one state at a time by design). Zero migration, composes with `Sum`/`ValueOf`/quantifiers, and preserves the non-commutativity of `pre(forall ...)` vs `forall ... pre(...)`. `EvalError::PreStateUnavailable` when `Pre` is reached with no pre-state in scope; phrased about evaluation context, not AST position, so future both-state contexts share it. The deferred `EvalContext` refactor was rejected here on subtraction grounds and landed one PR later, once a second contextual arg forced it.


### Insurance retrofit: `PolicyHeadroom` conservation via `pre(...)`

**Forced by:** closing the `Expr::Pre` PR's deferred business case with a real conservation rule. Insurance was chosen over the ledger because it already lives around aggregate-cap discipline and the ledger retrofit would need a wider remodel.

**Landed:** admitted `PolicyHeadroom(policy_id, remaining)` with `at_most_one_headroom_per_policy` (uniqueness) and `paid_implies_headroom` (existence, so the conservation guard is not vacuous); `authorise_settlement` reads headroom via `bind_one`, retracts it, and asserts the decremented one. Transition invariant `headroom_consumed_by_payment` uses the sum-of-new-payments delta form - `after = before - sum(amt | SettlementPaid(...) and not pre(SettlementPaid(...)))` - not a per-row equation: the per-row draft passed each row while a multi-payment transition with two same-amount settlements consumed headroom twice (caught in review). Bundled the `EvalContext { state, pre_state, bindings, actor }` evaluator refactor, now honestly forced by `Pre`'s second contextual arg.


### Enriched `morpholog check`: kind/type compatibility

**Forced by:** the input/output boundary made authoring trustworthiness the bottleneck. Decimal-in-subject-slot, `<=` against a date, arithmetic on a subject literal all only surfaced at runtime as `EvalError::TypeMismatch`.

**Landed:** `morpholog_core::kindcheck`, merged with the structural pass into one `Vec<ValidationError>`. Kind inference refines on observation (`Any` stays unconstrained until a concrete use - the "`Any` everywhere" alternative was rejected for losing the most valuable refinement pattern). The walker mirrors the runtime quartet (`Require` against a cloned env, `BindOne`/`Let` against the live one) and shadows `Sum`/`Forall`/`Exists` binders via `KindEnv::with_shadow` so a loop-local cannot collide with an outer variable, without which it would reject correct shadowing or pass programmes the runtime rejects.


### `IntentDecl`: outbox vocabulary as a declared first-class kind

**Forced by:** the KYC example, where a mistyped `emit SARFiled(...)` would silently route to nowhere - no analyst sees it, and the regulatory breach is invisible until a downstream auditor notices the gap. Predicates had `PredicateDecl`; intents were still stringly-typed and kindcheck skipped `Stmt::Emit`.

**Landed:** `IntentDecl` mirroring `PredicateDecl`; `Program.intents`; surface `intent X(args)`; strict from day one (every `emit` targets a declared intent). The foundational move was the `VocabularyKind { Predicate, Intent }` enum: the two vocabularies share every diagnostic shape, so the predicate-specific error variants were generalised (`UndeclaredPredicate` -> `Undeclared { vocabulary, name, context }`) rather than duplicated, and `PredicateArgDecl` became `ArgDecl`. A third declared vocabulary would slot in without further renaming.


### Static-analysis pass: one visitor, binding flow, actor context, and a depth floor

**Forced by:** the kind check had landed as a second walker beside the structural pass, and the next checks (unbound variables, value/predicate shape, actor context) all needed the same traversal and scope notion. A second binding-aware check made the two-walker shape stop paying - separate walks would each re-derive scope and drift from the runtime's binding rules.

**Landed:** `kindcheck` became `check`; `validate_program` is now a duplicate-declaration pass plus one `check_program` traversal carrying a `Scope { kinds, bound }`, cloned at the quartet's boundaries (`require`, `sum`, `for`, `or`-branches) so the non-export rules fall out of structure. Unbound-variable detection follows the evaluator exactly, where the subtlety lived: a disjunction exports only the *intersection* of names its branches bind (the KYC `(clean or adjudicated_clear) and ... expires` invariant relies on it), and `in` is a generator not a use - both caught by running the check over every worked example as the regression gate. Added `ExpectedPredicateExpression`, actor-in-wrong-context (`ActorNotAvailable`), a nesting-depth floor (`NestingTooDeep` - the enforceable form of "validate untrusted IR before proposing it", since `propose` trusts the IR it is handed), and `PgError::DuplicateIntent` to separate a modelling bug from a transient DB error.


### Counting via a constant `sum` target (and dropping `Sum.binding`)

**Forced by:** the chess example wanted to count pieces, and nothing in the kernel could express it. The doctrine had already flagged relaxing `sum`'s variable-only target when an example forced it.

**Landed:** the `sum` target now accepts a decimal literal, so `sum(1 | body)` counts bindings - no evaluator change (the target was already resolved per match), only a parser relaxation and a formatter that renders the target from the term. Chess gained `piece_count_matches_board`, exact-one-king per colour (count `= 1`, which forbids capturing a king), and a pawn bound. The same change retired the vestigial `Expr::Sum.binding` field, which duplicated the target name and was ignored by the evaluator. A dedicated `Expr::Count` was rejected as redundant with `sum(1 | body)`.


### The full comparator set per kind

**Forced by:** legibility, not a worked example - the deliberate exception. The strict forms are derivable (`a > b` is `not(a <= b)`), but the kernel has no `>` to render, so the formatter would print `amount > limit` back as `not (amount <= limit)` everywhere it shows a programme. Derivability buys nothing for legibility, which for the auditor reading formatted output is a core value - the third principle's "the easy case is verbose, so the shape is wrong" smell.

**Landed:** `Expr::Lt`/`Ge`/`Gt` (decimal `<` `>=` `>`) and `DateLt`/`DateGe`/`DateGt` (civil `before` `on_or_after` `after`), each first-class so it round-trips as written. The two duplicated comparator arms became `decimal_comparison`/`date_comparison` helpers parameterised by an `admit` closure - the abstraction the fourth-through-eighth case finally forced. `before`/`after` are matched contextually rather than reserved, because existing examples use them as variable names.


### The explanation engine: rejection as read-side trace interpretation

**Forced by:** the legibility gap. The runtime could say *whether* a transition was admissible, but the buyer's question is *why not, and what is still missing*. The distilled future directions named this the highest-leverage move ("Morpholog as an explanation engine"), and `propose_with_trace` already carried the failure data - so the work was interpretation, not a new evaluator.

**Landed:** `morpholog_core::explain` returns a structured `Explanation` derived from the kernel trace - `Verdict::Admissible`, or a `Rejection` that is a gate (carrying the directly-missing positive claim conjuncts and, per missing predicate, the candidate transformations that assert it), an invariant violation, or a kernel error - rendered to deterministic claim-shaped prose or JSON, with no NLP. The diagnostic failure-walk gained `unsatisfied_positive_claims`, returning the unsatisfied positive claim conjuncts structurally (carried additively on `RequireOutcome::Rejected` / `BindOneOutcome::NoMatch`), and `analysis::transformations_asserting` is the one-hop supplier lookup. No IR primitive and no surface syntax: explanation is read-side analysis over the one executor's trace.

**Considered and rejected:**

- *Natural-language generation.* An explanation an auditor relies on must be reproducible and faithful to the exact failing claim; a probabilistic generator cannot be. The words come from predicate and transformation names plus fixed templates.
- *Reporting "minimal" missing evidence now.* True minimality is a constrained search (abduction/repair), not a trace read. v0 reports the directly-missing positive claims only; the model speaks claims (`directly_missing_claims`), and the renderer reserves "evidence" for prose.

**What stays out:**

- Present blockers (`not X` where `X` holds), comparator failures, existential and disjunctive remedies, and bounded abduction - all later tiers; those render a faithful reason with an empty missing list. The CLI `explain` surface and a static `inspect guarantees` view are deferred.


### Carbon-credit provenance: the flagship that forced no primitive

**Forced by:** nothing in the kernel - and that is the point. The carbon-credit / certificate-of-origin domain was chosen as the explanation engine's first real home because its failure mode is pure legitimacy: a green claim that became official without an admissible, current provenance chain. The test was whether the existing claim model could carry evidence-provenance with no new primitive.

**Landed:** the worked example (`carbon_credit_provenance`) models the whole provenance chain as ordinary claims about claims - `VerifiedMeasurement`, `Attestation`, `Accredited` - gating `issue_credit` on a verified measurement (binding its quantity), an attestation, and a *currently* accredited verifier. Double-counting in both directions (no two credits per measurement, no two measurements per credit), single custody, and terminal retirement are invariants; currentness is the verified-revenue pattern (revoking accreditation retracts standing via `retract`, blocking new issuance while leaving issued credits admitted). No kernel change was needed. The MRV computation stays outside and returns as the admitted `VerifiedMeasurement` quantity, holding the inside/outside boundary. The example exists to point the explanation engine at a real domain: a refused issuance names the missing `VerifiedMeasurement` / `Attestation` / `Accredited` claim and the transformation that would supply it.

**Considered and rejected:**

- *Forcing `Expr::Mul` via generation-times-factor.* That computation is the meter; governing it would cross the inside/outside line. Conservation checks, when needed, use the existing `sum` / comparators on admitted quantities.
- *Conservation by sum across a batch.* Deferred behind a stated "one credit per measurement" simplification, so the legitimacy mechanics stay visible.

**What stays out:**

- Batch issuance (one credit backs one measurement here); obligations over time (retire-by-deadline) and a static `inspect guarantees` view, both of which will point at this model.


### Obligations over time: the outside-coordinator sweep

**Forced by:** the carbon domain's compliance half - a scheme obliges an account to retire enough credits by a deadline. This is the first worked example with a rule about *time*, and the kernel has no clock by design (no I/O, decidable core).

**Landed:** obligations modelled as ordinary claims (`Obligation(obligation, account, quantity, due_on)`, `ObligationSatisfied(obligation)`, `ObligationBreached(obligation)`), added to example 09. `raise_obligation` records one; `discharge_obligation(obligation, current_date)` admits satisfaction when, on or before the deadline, the account's retired total reaches target - discharge is date-aware too, so a late retirement cannot quietly satisfy a "by `due_on`" obligation; `sweep_obligation(obligation, current_date)` is the **outside-coordinator tick** - an external scheduler hands the current date in as an argument, and the kernel decides whether the obligation is breached (past due, not satisfied, under target). "Now" never lives in the kernel; this is the "Morpholog plus an Outside Coordinator" pattern from `outbox-sketch.md` made concrete. The `obligation_not_both_satisfied_and_breached` invariant keeps the two outcomes mutually exclusive. No kernel primitive was needed: the retired total is `sum(q | Retired(c, account) and Issued(c, m, q))` - confirming the evaluator's `Sum` handles a conjunction (join) body in a `require`, not only a single-claim body - and the deadline is the existing `after` date comparator over the date argument.

**Considered and rejected:**

- *A clock or `now()` primitive in the kernel.* It would break the no-I/O decidable core and make admissibility non-reproducible. Time enters as data, through the sweep's argument - never as a clock the kernel reads.
- *Carrying a quantity on `Retired` to avoid the join.* Unnecessary once the join-in-`sum` was confirmed to work; `Retired(credit, account)` stays as it was.
- *Deriving satisfaction as a derived claim.* Transformations read admitted claims, not derived views, so the gates compute the `sum` directly.

**What stays out:**

- Contrary-to-duty obligations (a secondary duty that activates when the first is breached), recurring or rolling deadlines, and a sweep that iterates all due obligations in one call (the coordinator drives iteration, one obligation per call).


### `inspect guarantees`: the model's impossibilities, before it runs

**Forced by:** the legibility brief's other half. `explain` answers "why was this rejected?"; a controller or regulator asks first "what does this model make impossible?". The carbon flagship - rich with `not(...)` invariants (terminal retirement, mutually-exclusive obligation outcomes) - made the answer worth surfacing as its own read.

**Landed:** `morpholog_core::guarantees(program)` returns one `Guarantee` per invariant - the rendered rule, plus a `forbids` clause naming the bad state only for `not(...)` invariants (whose inner expression *is* the forbidden state). `render_guarantees` renders it as deterministic prose; the `morpholog inspect guarantees <program>` CLI emits prose by default, `--json` for the structured form. Pure static read over any registered programme, no kernel or PG. Tested across the whole registry, not just carbon, so the derivation is demonstrably general rather than handcrafted.

**Considered and rejected:**

- *Deriving "bad state" for `implies`/comparator invariants too.* Only the `not(...)` shape has a mechanically obvious forbidden state; inferring one for an `implies` (a functional dependency) or a comparator would be semantic interpretation, not formatting. Those guarantees carry their rendered rule and no `forbids` clause - honest over impressive.
- *Hand-written domain summaries ("a credit cannot be held twice").* That is prose the example author would write, not something derived; it would flatter the demo while hiding that the tool only reads invariant structure. The words come from invariant names and the formatter alone.

**What stays out:**

- Mutually-exclusive predicate *sets* derived from `implies`/`Neq`, transformation pre/post graphs, subject-flow profiles, and `generate controls` - the rest of the legibility set, each its own read when forced.


### `morpholog explain`: the engine reachable from the command line

**Forced by:** the explanation engine shipped as a library (`morpholog_core::explain`) with no way to ask the question from outside Rust. An operational checklist an auditor or controller reads needs a command, not an API the embedder calls.

**Landed:** `morpholog explain <file.morph> <transformation> --args <json> --actor <subject>` - the read-only counterpart of `run`. Same parse/validate front-end and same `Transition` codec; but instead of proposing, it loads the predicate-scoped pre-state, runs the kernel in-memory via `explain`, and renders the `Explanation` as claim-shaped prose (default) or JSON (`--json`). A new public `morpholog_postgres::load_scoped_state` does the read - the sibling of the load inside `propose_against_pg`, same `compute_load_scope`, but a plain pooled read rather than a SERIALIZABLE transaction, because explaining is answering a question, not committing a decision. No kernel or IR change; the surface sits above the parser, like the static-analysis pass.

**Considered and rejected:**

- *Built-in registry as the program source (like `propose` and `inspect`).* The `.morph`-file shape (like `run`) lets an author explain their own model, not only the shipped examples - which is where the operational value is. The built-in path is already covered by the other legibility reads.
- *Exiting non-zero on a rejection verdict (like `run` / `propose`).* That conflates an advisory read with an action. The verdict does not affect the exit code - explain exits zero on both admissible and rejected, a rejection being a successful explanation carried in the output; only operational failures (parse/validation, bad args, unknown transformation, DB failure) exit non-zero. A script that wants the gate uses `run`.

**What stays out:**

- The rest of the legibility set (transformation pre/post graphs, subject-flow profiles, `generate controls`); and everything the engine itself defers (present blockers, comparator failures, abduction) - the CLI renders whatever the v0 engine produces, no more.
