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

## Parser arc

Entries from the parser arc onwards are kept at fuller detail because the arc is still active.

### Parser P1: predicate declarations only

**Forced by:** the parser arc starting. Programmes are Rust IR today via the public `dsl` module; the natural reader of a Morpholog programme is a domain expert, not a Rust developer. PR P1 commits the surface foundation - new crate, lexer, ariadne diagnostics pipeline, CLI entry point - by recognising the smallest meaningful production: the `program` header plus zero or more `predicate` declarations. Every subsequent parser PR builds on the decisions this one bakes in; nothing else lands until those are stable.

**The shape:**

- New crate `morpholog-surface`. Name chosen broader than "parser" because the same crate will eventually host any canonical formatter, source-mapping helpers, and LSP-shaped tooling - a crate split per kind of source-aware concern is the proliferation we'd rather avoid.
- `chumsky` 0.10 for lex + parse. `ariadne` 0.5 for diagnostic rendering. Both pulled into the workspace `Cargo.toml` so subsequent parser PRs use the same versions.
- Two-phase: `lex` produces `Vec<(Token, Span)>`; `parse_program` runs the token stream through a chumsky parser and produces `morpholog_core::Program` (with `predicates` populated, every other vector empty).
- `Diagnostic { severity, message, primary: Span, secondary: Vec<(Span, String)> }` is the surface-side error type. Carries byte-offset spans; renders via ariadne when a caller wants line/column/caret output. CLI uses the rendered output; tests use the structured fields.
- Parser-side duplicate detection: each `predicate` declaration carries its span through the intermediate `RawProgram`; a post-pass produces a diagnostic with both source spans when the same name reoccurs. `Program::validate` also detects duplicates but loses span context; this PR's parser surfaces them with both line numbers.
- Error recovery: humble. The parser uses `recover_with(skip_then_retry_until(any, predicate_kw_or_end))` to skip past a malformed predicate declaration and continue. A file with two broken declarations yields two diagnostics in one run.
- CLI: new `morpholog parse <file.morph>` subcommand. Success prints a JSON projection of `Program` (just `name` and `predicates` - the rest of `Program` doesn't derive `Serialize` and would be empty regardless). Failure renders each diagnostic to stderr via ariadne and exits 1.

**Surface decisions this PR commits.** These will be load-bearing for every subsequent parser PR; documenting them here so the rationale survives even if individual sources move:

- File extension: `.morph`. Locked decision per `docs/scope-and-ambition.md`.
- First statement: `program <name>` header is mandatory. A file without one fails parse.
- Identifier syntax: `[a-zA-Z_][a-zA-Z0-9_]*`. Standard.
- Kind keywords: PascalCase exact match (`Subject`, `Decimal`, `Date`, `Bool`, `Collection`, `Any`). Recognised at lexer level so the parser can match against `Token::Kind(_)` directly; unknown kinds (e.g. `Money`) fall through to `Token::Ident` and surface as parse errors where a kind is expected.
- Comments: `//` line only, no `/* */`. Block comments can land when forced.
- Trailing commas in argument lists: allowed.
- Newlines: insignificant within declarations. Declarations are bounded by their own syntax (parens around arg lists, the `predicate` keyword starting each one).
- One file = one programme. No module system; no `use` statements. Composition is by Rust-level registry today; whether a multi-file `.morph` story is needed is a question for a later PR.

**What deliberately did NOT land:**

- Invariants, transformations, derived claims. Everything beyond predicate declarations is out of scope.
- Expression syntax (`And`, `Le`, `Exists`, `Forall`, arithmetic, etc.). Per ChatGPT's PR-P1 review, expressions are big enough to deserve their own PR (P2: expressions parsed in isolation, behind tests, before they're attached to invariant or transformation bodies).
- Statement syntax (`require`, `bind_one`, `let`, `for`, `assert`, etc.).
- A `morpholog run <file.morph>` command. The parser produces IR; running it would require parser coverage of the full surface, which isn't here yet.
- Block comments.
- `format_program` round-trip test as a regression anchor. `format_program` emits invariants and transformations too; round-tripping its output through the P1 parser would either fail (those productions aren't parsed) or force a `format_predicate_decls`-only helper that isn't otherwise needed. Tests use hand-crafted `.morph` text instead.
- Source-position propagation into kernel errors. Kernel errors are runtime-shaped; spans live entirely inside `morpholog-surface`.

**Considered and rejected:**

- *`morpholog-parser` as the crate name.* `morpholog-surface` accommodates the formatter, source-mapping helpers, and LSP support that will eventually arrive without a crate split. The parser is one inhabitant of the surface concept.
- *`morpholog inspect parse` as the CLI shape.* `inspect` is about durable state; parse is source-to-IR compilation, fundamentally different. Top-level `morpholog parse` is the correct verb.
- *Kernel-side duplicate detection only.* `Program::validate` already catches duplicates but loses span context. Parser-side detection produces span-rich diagnostics; the kernel validator remains a structural backstop.
- *Fail-fast error reporting.* Collecting multiple diagnostics in one parse run is materially more useful when an author is migrating Rust IR to `.morph` syntax for the first time. `chumsky`'s `recover_with` makes the recovery cheap.


### Parser P2a: expression syntax

**Forced by:** the parser arc continuing. P1 committed the file-level surface (program header + predicate declarations); P2a commits the **expression syntax** that subsequent parser PRs will embed in invariant bodies, transformation bodies, and derived-claim domains. Done in isolation - no invariants or transformations parse yet - because expressions are the hardest single piece of the surface arc (operator precedence, term-vs-expression asymmetry, span handling) and isolating them keeps those decisions visible.

**The shape:**

- New `parse_expression(source) -> Result<Expr, Vec<Diagnostic>>` public entry point alongside `parse_program`. Shares the same lexer.
- Lexer extended with: decimal literals (`<digits>` or `<digits>.<digits>`, carried as strings to preserve exactness); operator tokens (`+`, `-`, `=`, `!=`, `<=`); boolean keywords (`not`, `and`, `implies`); wildcard token (`_`).
- Parser grammar (informal precedence, highest to lowest): atoms (parens, vars, decimal literals, wildcards, predicate calls); `+` / `-` (left-assoc); `=` / `!=` / `<=` (non-assoc); `not` (prefix); `and` (left-assoc, flattened to `Expr::And(Vec<Expr>)`); `implies` (right-assoc).
- Surface conventions: `actor` is a bare identifier that lowers to `Term::Actor` (no parens, per the surface doctrine); `=` and `<=` accept full expressions on both sides; `!=` accepts only Terms on both sides because `Expr::Neq(Term, Term)` is the IR shape.

**Doctrine-enforced asymmetry.** The IR's `Expr::Neq` and `Expr::In` operate on `Term`s, not `Expr`s; `Eq`, `Le`, `Sub`, `Add` operate on `Expr`s. The parser must honour this directly: `a + 1 != b` is rejected with a clean diagnostic ("`!=` requires both sides to be terms; arithmetic and other expressions are not allowed because the IR's Neq operates on terms only"). Per Position A in `docs/scope-and-ambition.md`, the surface cannot create capabilities the kernel lacks - so the parser rejects what the IR cannot represent rather than silently producing ill-shaped IR. The same constraint rules out `Foo(x + 1, y)` because claim-call arguments are `Vec<Term>`, not `Vec<Expr>`.

**Considered and rejected:**

- *Bool literals (`true` / `false`).* The IR's `Value` enum has variants for `Decimal`, `Subject`, `Date` only. `EvalValue::Bool` exists as a runtime computed value (the result of comparators), but there is no IR literal path for `true`/`false`. Per the doctrine, surface forms must lower to existing IR; bool literals at the surface would have nowhere to lower to. They land when a worked example forces `Value::Bool` into the IR, not before. ChatGPT caught this in PR-P2a's review window.
- *Date literals.* Defer to P2b alongside the other bounded-form work; civil-date literal syntax and `DateLe` deserve attention together.
- *Subject literals.* `dsl::subj("name")` is the only current path. No worked example yet writes a subject literal at the surface; defer.
- *`In` operator (`x in coll`).* Defer to P2b alongside `forall x in coll: body` because `in` is multi-purpose - used both as a comparator (`Expr::In(Term, Term)`) and as a binding keyword (`for x in coll`, `forall x in coll`). Bundling them keeps the keyword's roles clear in one PR.
- *Unary minus.* The IR has `Expr::Sub` only; no unary minus. Surface `-5` would have to fold to a sub-expression or to a literal; both are awkward. Defer until forced. Users today write `0 - 5` or use `Sub(a, b)` IR directly.
- *Error recovery within expressions.* P1's predicate-declaration recovery (sync at next `predicate` keyword) does not extend to expressions in P2a. A malformed expression surfaces as one diagnostic at the failure site; recovery within expressions would mean designing per-operator sync points, which is more design surface than P2a should commit.

**What landed:**

- `Token` extended with `KwNot`, `KwAnd`, `KwImplies`, `Wildcard`, `DecimalLit(String)`, `Eq`, `Neq`, `Le`, `Plus`, `Minus`. PascalCase kind keywords kept distinct from identifier-kind tokens.
- `parse_expression` public API. Same diagnostic shape as `parse_program`.
- 31 integration tests in `tests/parse_expression.rs` covering atoms, arithmetic, comparators, boolean composition, precedence (in both directions for each layer), associativity (left for `and`, right for `implies`, left for arithmetic), the Neq term-only restriction, the claim-call-args term-only restriction, and two realistic fragments from existing worked examples (insurance cap rule, netting per-line conjunct).
- Doctrine committed first on the branch (preparatory commit), so the rest of the parser arc cites a settled rule rather than re-arguing it.

**What this PR does NOT include (P2b and beyond):**

- `exists x: body`, `forall x in coll: body`, `sum(target | body)`, `value(target | body)` - the bounded-form expressions.
- Date literals, `DateLe`, subject literals.
- `in` as either binding keyword or membership comparator.
- The actual embedding of expressions into invariant or transformation bodies. P3 territory.

**Pattern note:** P2a is the first parser PR where the surface diverges visibly from the IR shape. Position A says the surface is more readable than the IR but never more powerful; this PR is the first to exercise both halves of that constraint - making `amount + already_paid <= limit` readable, and refusing `Foo(x + 1)` because the kernel cannot represent it.

### Parser P2b-lite: bounded forms, literals, and the `value` shape correction

**Forced by:** the parser arc continuing. P2a committed the non-bounded expression surface (atoms, arithmetic, comparators, boolean composition). P2b-lite completes the expression layer with the bounded forms (`exists`, `forall`, `sum`, `value`), the membership comparator (`in`), date and subject literals, and the dual-role disambiguation that the `in` keyword forces. After P2b-lite, the `parse_expression` API can recognise every `Expr` variant the kernel supports, including the full operator set the worked examples use.

**The `value` shape correction.** A doctrine-preparatory commit (the first commit on the P2b-lite branch) fixed a wrong row in `scope-and-ambition.md`. The earlier draft mapped `value(target | body) -> Expr::ValueOf`, treating `value` as parallel to `sum`. That was incorrect: `Expr::ValueOf { predicate, args, default }` is claim-pattern-shaped, with a wildcard in `args` marking the value position to extract. A `value(target | body)` form would imply a general query expression, which is *more expressive than the IR can represent* unless the lowering imposes restrictions invisible from the surface. Under Position A, surface forms must have unambiguous IR mappings. The corrected shape is `value Pred(args)` with optional `default expr` suffix - a direct claim-pattern that maps to the IR one-to-one. ChatGPT's review of P2a's doctrine table flagged this; the fix landed before any parser code that would have committed the wrong shape.

**Quantifier body greediness.** `forall x in xs: A and B` parses as `forall x in xs: (A and B)`. Quantifier bodies extend to the end of the enclosing expression (the body is itself a top-level expression). This matches mathematical convention - in `∀x∈S, P(x) ∧ Q(x)`, the conjunction is inside the quantifier's scope. Composition with outer expressions requires parenthesisation: `(forall x in xs: P(x)) and Q(z)`.

**Forall source shapes (the auto-lift).** The kernel's `Expr::Forall { binding, source, body }` requires `source` to be predicate-shaped so `find_matches` can produce binding extensions. Two surface idioms are common:
- `forall line in lines: body` - source is a bare variable. The parser auto-lifts it to `Expr::In(Var("line"), Var("lines"))` so the kernel can iterate.
- `forall claim_id in ClaimReported(claim_id, ...): body` - source is a claim query, already predicate-shaped. Used as-is.

The auto-lift only fires when the parsed source is `Expr::Term(t)`; anything else (Claim, ValueOf, Sum, etc.) passes through. This keeps the surface natural for both idioms without forcing the user to think about which case they're in.

**The `in` dual-role disambiguation.** `in` is structural in `forall <ident> in <source>:` and a membership comparator (`Expr::In(Term, Term)`) everywhere else. Positional disambiguation: the parser consumes the structural `in` inside the `forall` production before the comparator-level grammar can see it. A tree-sitter grammar can express this with the same positional precedence pattern; no context-sensitive parsing is needed.

**Sum target restriction.** `sum(target | body)` requires `target` to be a variable name (lowered to `Term::Var(name)`). Literals, wildcards, and `actor` in target position are rejected with a parse-time diagnostic. This matches every existing worked-example use of `Sum`; the IR's `value: Term` field can hold any Term but no example needs the generality. Relaxable when a worked example forces it.

**Considered and rejected:**

- *Date comparison (`before`, `on_or_before`, `<:=`, `date_le()`) in P2b-lite.* The IR's `DateLe` exists; the surface comparator is what's missing. Three options were on the table: a new keyword like `on_or_before`, a function-call form `date_le(a, b)`, or operator overloading on `<=`. All three need a forcing pressure: a concrete invariant body that uses dates with `<=`-style ordering. P2b-lite parses date literals (usable as `Term::Literal(Value::Date(_))` in claim arg position) but defers the comparator surface to P3 alongside the worked example that drives it. The doctrine table in `scope-and-ambition.md` notes this explicitly.
- *Generic `value(target | body)` shape.* See the doctrine fix above. More expressive than IR; rejected.
- *Generalised `sum` target.* Restricted to variables in v0; the existing worked examples never need the generality. Can relax later.
- *Unicode set-builder syntax (`{ x | P(x) }` instead of `sum(target | body)`).* Considered briefly. Adds Unicode handling to the lexer and conflicts with the curly-brace usage that might want to appear in invariant/transformation bodies later. ASCII `sum(...)` form is already mathematical enough.
- *Bare-token date literals (`2026-05-22` without `@`).* Ambiguous with arithmetic (`2026 - 05 - 22`). `@` sigil resolves this with no lexer complexity. Same reasoning for `#` on subject literals.

**What landed:**

- Lexer tokens: `KwExists`, `KwForall`, `KwSum`, `KwValue`, `KwDefault`, `KwIn`, `Pipe`, `DateLit(String)`, `SubjectLit(String)`.
- Parser productions: `exists x: body`, `forall x in source: body`, `sum(target | body)`, `value Pred(args) [default expr]`, `x in xs`, `@YYYY-MM-DD`, `#NAME`.
- Auto-lift for `forall` source when given as a bare Term.
- Sum-target restriction to `Term::Var` with parse-time diagnostic on violation.
- Doctrine row corrections in `scope-and-ambition.md` for the `value` shape, the `forall`/`exists` pair, the `sum` target restriction, and new rows for `in` membership, date literals, and subject literals.
- 24 new integration tests covering each new form, the `in` dual-role disambiguation, the `value` with-and-without-default cases, sum-target restriction, quantifier body greediness, the parenthesised-composition boundary, the forall-source auto-lift for both bare-variable and claim-call sources, and four realistic fragments from existing worked examples (insurance aggregate cap with `sum`; netting forall; verified-revenue exists; clinical-trial-style date literals in claim args).

**What deliberately stays out (deferred to P3+):**

- Statement syntax (`require`, `bind`, `let`, `admit`, `retract`, `emit`, `for`).
- Programme-level integration (the expression parser is not yet embedded into invariant/transformation bodies).
- Date comparison surface form.
- General `value(target | body)` (rejected; not deferred - it's not coming back).
- General sum target (deferable but no forcing case yet).
- Source maps into kernel error reports.
- Formatter / LSP / tree-sitter grammar.

### Parser P3a: invariant declarations

**Forced by:** the parser arc continuing. P2b-lite completed the expression layer; P3a integrates expressions into the first programme-level construct, `invariant`. The deliberately tiny scope picks the construct whose body is a pure expression (already parseable) and which doesn't introduce statement syntax. This proves the integration mechanic before P3b takes on the harder transformation-body work.

**Surface form:**

```morph
invariant <name>:
    <expression>
```

The IR's `Invariant { name, version, body }` always has `version = 1` in v0 (a locked design decision in `docs/scope-and-ambition.md`). P3a does NOT add version syntax to the surface; the parser defaults `version` to 1, and the test suite pins that `invariant cap(v1): body` is currently a parse error. When versioning grows a second meaningful value, the surface and formatter both grow a version clause together. The `format_program` debug output's `(v1)` suffix is intentionally left as-is for now: the parser does not have to accept every debug-format shape, and full round-trip is deferred to P3c when the surface is complete.

**Programme grammar (free interleaving):**

```text
program        ::= program_header top_level_decl*
top_level_decl ::= predicate_decl | invariant_decl
```

A `.morph` file can mix predicates and invariants in any order. The parser collects declarations in source order and sorts them into `Program.predicates` and `Program.invariants` on a post-pass. This avoids committing the language to an "all predicates first" file convention - users will naturally group related decls together (a predicate plus the invariant that governs it) and the grammar should support that.

**Duplicate-invariant detection (parser-side).**
The kernel's `Program::validate` catches duplicate predicate declarations (added in PR-C) but not duplicate invariant names; no current example forces invariant-by-name lookup ambiguity. The parser nonetheless detects duplicates at the surface level so the diagnostic carries both spans, mirroring P1's duplicate-predicate detection. Kernel-side validation can be added if a future invariant-lifecycle PR forces name-based lookups; until then, the parser is the only gate.

**Considered and rejected:**

- *Required `(v1)` version clause at the surface.* Matches `format_program` output but requires every author to write `(v1)` even though it's the only possible value in v0. Reads as ceremony.
- *Optional `(v1)` clause with default to 1.* Round-trip-compatible with current `format_program` output. Adds parser ambiguity (version-or-not) for negligible value when there's only one possible version.
- *Update `format_program` to elide `(v1)` so round-trip works.* Reviewed and reverted on ChatGPT's suggestion: the parser surface and the debug formatter don't have to match. `format_program` stays as-is; full round-trip becomes a deliberate decision at P3c when the surface is complete and the formatter's role (debug vs canonical) can be decided alongside it.
- *Strict ordering (`predicate_decl* invariant_decl*`).* Forces "all predicates first" file convention. No semantic reason to ban interleaved decls.
- *Adding kernel-side duplicate-invariant validation in this PR.* No current example needs name-based invariant lookup. Parser-side detection is enough for `.morph` files; kernel validation can land when a future invariant lifecycle requires it.

**What landed:**

- New `KwInvariant` token, lexer-reserved.
- New `invariant <Ident>: <expression>` production in the parser.
- `program_parser` restructured: `program_header top_level_decl*` with free interleaving of predicate and invariant decls.
- `parse_program` produces `Program { predicates, invariants, ... }` (other vectors still empty pending P3b/c).
- Parser-side duplicate-invariant detection with span-rich diagnostics.
- CLI `morpholog parse <file>` JSON projection extended to include invariants (body rendered as a string via `format_expr_inline`, since `Expr` doesn't yet derive `Serialize`).
- 12 new integration tests: simple body, forall body, sum+Le body, version defaulted to 1, multiple invariants, free interleaving, predicates-first canonical order, missing colon, missing body, duplicate-with-both-spans, version-syntax rejection, reserved-keyword rejection.

**What deliberately stayed out (P3b and beyond):**

- Transformation declarations and the seven statement keywords (`require`, `bind`, `let`, `admit`, `retract`, `emit`, `for`).
- Derived-claim declarations.
- Date-comparison surface (no `Expr::DateLe` lowering at the surface yet).
- Round-trip property tests (`format_program -> parse_program -> equal IR`). Land at P3c when the full surface is parseable.
- Automatic `Program::validate()` inside `parse_program()`. Kept separate; can become `morpholog parse --validate` later.
- A `morpholog run <file.morph>` command. Needs P3b at minimum for transformations.

**Pattern note:** P3a is the smallest possible step that moves the parser into programme integration territory. Two more PRs (P3b for transformations + statements, P3c for derived claims) finish the v0 surface. After that, the parser arc is operationally complete and the next major investment is whatever the next forcing example demands.

### Parser P3b1: layout pipeline + transformation headers + gate statements

**Forced by:** the parser arc continuing into programme-level integration. P3a integrated invariants (whose bodies are pure expressions, already parseable). P3b is the first time `.morph` becomes operational - the transformation surface introduces statement blocks, indentation, and the first programme-level constructs that change admitted state. P3b1 proves the layout foundation and the simplest statement subset (`require`, `bind`, `let`); P3b2 will add the state-mutating statements (`admit`, `retract`, `emit`, `for`).

The split into P3b1 / P3b2 was deliberate. P3b's natural scope is large enough that the parser code can become hard to review; splitting at the seam between "gate statements over admitted state" (require/bind/let) and "statements that change admitted state or iterate" (admit/retract/emit/for) keeps each PR a clean review unit.

**Layout pipeline: C/A hybrid.** Per the indentation doctrine in `scope-and-ambition.md`, blocks use indentation, not braces. The implementation lives in a new `crates/morpholog-surface/src/layout.rs` module that sits between the character-level lexer and the structural parser. The lexer remains ignorant of line structure; the parser consumes a token stream pre-enriched with virtual `Indent` and `Dedent` tokens at block boundaries.

Three options were on the table for where indentation-awareness lives: lexer-level INDENT/DEDENT emission (Option A), parser-side indentation lookahead (Option B), separate layout pass (Option C). The hybrid that landed is closest to **Option C**, with one structural simplification borrowed from Option A: the layout pass operates on the lexer's `(Token, Span)` output (a typed structure), not on raw character input. The doctrine's softened wording in `scope-and-ambition.md` made this implementation choice explicit: the *doctrine* is indentation; the *implementation* is virtual tokens from a layout pass.

**Layout rules.** Documented in the `layout.rs` module header; pinned by the test suite:

- Spaces only for indentation. Tab characters in indentation emit a diagnostic. The floor is to refuse the tabs-vs-spaces ambiguity rather than guess.
- Parens disable layout. When a token's preceding gap is inside open parentheses, its newlines do not trigger Indent / Dedent. This is what makes long expressions span lines naturally: `require sum(amount | Foo(amount)) + proposed <= limit` can break across lines as long as it stays inside the outer parens.
- Blank lines and comment-only lines do not affect indentation. The layout pass uses the position of the *next real token* after a newline.
- No virtual `Newline` tokens emitted. Each statement and top-level declaration starts with its own keyword (`require`, `bind`, `let`, `predicate`, `invariant`, `transformation`, etc.). The keyword anchors statement boundaries; no separator token is needed. This also lets parenthesised expressions span lines freely with no layout interaction.

**Statement parser.** The new `crates/morpholog-surface/src/parser/stmt.rs` module recognises:

- `require <expression>` -> `Stmt::Require(_)`
- `bind <claim-pattern>` -> `Stmt::BindOne(Expr::Claim { .. })` (parser restricts the surface to a claim pattern; the IR's `Stmt::BindOne` is technically `Expr` but the meaningful authoring form is a single claim, and arbitrary expressions are rejected at parse time per the surface doctrine)
- `let <name> = <expression>` -> `Stmt::Let { name, value }`
- `let <name> = new Subject ( )` -> `Stmt::LetNewSubject { name }`

The `let new Subject()` form is the only place in P3b1 surface where the `new` keyword appears; it's a specific token sequence the parser matches directly (`KwLet Ident Eq KwNew Kind(Subject) LParen RParen`).

**Transformation parser.** Extends `program.rs` with the `transformation` production:

```text
transformation <Ident> ( <param-list> ) : Indent <stmt>+ Dedent
```

Parameters are identifiers only; no kinds. The IR's `Transformation { parameters: Vec<String> }` field has no kind information; surface kinds would be more expressive than IR, violating Position A. The body is `Indent stmt+ Dedent` - at least one statement is required (an empty transformation body has no use in v0).

**Free interleaving across top-level decls.** Same convention as P3a (predicate + invariant): the grammar now accepts `predicate | invariant | transformation` in any order. The parser collects in source order and partitions into the `Program`'s three vectors on a post-pass.

**Parser-side duplicate detection for transformation names.** Mirrors the predicate and invariant duplicate detection. Both spans land in the diagnostic.

**Body greediness in `: <expr>` productions.** Three productions across the parser now consume `: <expr>` with an optional indented body: invariant decl, exists, forall. Each accepts both inline form (`exists x: Foo(x)`) and indented form (`exists x:\n    Foo(x)`), via a common `(Indent expression Dedent | expression)` choice. Without this, the layout pass's `Indent` token would block parsing of any multi-line body. The fix is local to each `: <body>` consumer; there is no global "skip Indent here" hack.

**P3b2 keywords reserved at the lexer.** `admit`, `retract`, `emit`, `for` are reserved as `Token::Kw{Admit,Retract,Emit,For}` even though no production consumes them. The parser rejects them with an unexpected-token diagnostic. This mirrors how `true`/`false` are lexer-reserved in P2a: it prevents the surface from silently treating these as identifiers and lets users get a clean diagnostic at the v0 limit. P3b2 will turn them into productions without any further lexer work.

**Considered and rejected:**

- *Newline tokens as statement separators.* Considered following Python's tokeniser pattern (NEWLINE / INDENT / DEDENT). Rejected because each Morpholog statement starts with its own keyword - the keyword IS the boundary. Newlines would be either redundant or interfere with parenthesised line continuation. The simpler design (Indent / Dedent only) covers the entire block-structure problem.
- *Including `admit` / `retract` / `emit` / `for` in P3b1.* These are the state-mutating + iteration surface. Their grammars are individually small, but the `for x in coll: <indented body>` form introduces *nested* layout (a Indent inside an Indent) which is a separate testing concern from "transformation has one Indent/Dedent". Splitting them off keeps P3b1's layout testing tractable.
- *`let x = new Subject()` deferred to P3b2.* Briefly considered (its grammar shape is distinct from `let x = expr`). Kept in P3b1 because `let` is the value-binding statement and splitting its two RHS shapes across PRs would be artificial.
- *Statement-level recovery on parse failure.* The top-level recovery (sync at next predicate/invariant/transformation keyword) covers programme-level errors; statement-level recovery would mean syncing at the next statement keyword inside a body. Deferred until P3b2 - the simpler "one diagnostic per malformed transformation body" works for now.
- *Parameter kinds on transformation headers.* Surface would be `transformation foo(claim_id: Subject, amount: Decimal):`. The IR's `Transformation` doesn't carry kinds. Adding them surface-side would be more expressive than the IR; rejected.

**What landed:**

- 9 new lexer tokens: `KwTransformation`, `KwRequire`, `KwBind`, `KwLet`, `KwNew`, `KwAdmit`, `KwRetract`, `KwEmit`, `KwFor`, plus the virtual `Indent` and `Dedent`. (Four P3b2 keywords reserved now.)
- New `layout.rs` module + 14 layout-only integration tests.
- New `parser/stmt.rs` module for the four P3b1 statement forms.
- `parser/program.rs` extended: transformation_decl production, free interleaving in the top-level grammar, duplicate-transformation-name detection.
- 11 new transformation-parsing tests in `tests/parse.rs`, including statement-order preservation (the load-bearing test that confirms `Vec<Stmt>` mirrors source order), interleaving with predicates and invariants, `admit` rejection (P3b2 territory), `let new Subject()` round-trip.
- CLI `morpholog parse` JSON output extended with a `transformations` array, each entry projecting body statements through `format_stmt`.
- Quantifier bodies (`exists`, `forall`) and invariant bodies all accept optional `Indent body Dedent` wrapping, enabling multi-line bodies.

**What deliberately stayed out (P3b2 and beyond):**

- `admit`, `retract`, `emit`, `for` statements. Reserved at the lexer but not yet parseable.
- Statement-level error recovery (sync at next statement keyword).
- `for x in coll: <indented body>` nested layout.
- Derived claims (P3c).
- Date-comparison surface form (lands with a worked example body that needs it).
- A `morpholog run <file.morph>` command (needs the full surface and the rest of P3 done).

**Pattern note:** P3b1 is the first parser PR where the surface introduces a *layout* concept the kernel doesn't model. The kernel's IR has flat `Vec<Stmt>` for transformation bodies - no nesting, no blocks. The surface adds indentation to make multi-statement bodies readable; the layout pass + statement parser collapse it back to a flat IR. This is the doctrine in action - the surface adds *organisation* (visual structure) that the kernel doesn't need; the IR stays minimal.


### Parser P3b2: state-mutating statements + iteration

**Forced by:** the parser arc finishing the transformation surface. P3b1 committed the layout pipeline and the gate statements (`require`, `bind`, `let`); P3b2 completes transformation bodies with the four reserved-but-not-yet-parseable keywords from P3b1: `admit`, `retract`, `emit`, `for`. After P3b2, the worked examples either parse end-to-end or stop only at `derived` (P3c territory).

**Surface verb / IR mapping (the shared shape).** All four new verbs operate on a claim-pattern shape: `Verb Name(args)`. The existing `claim_pattern` helper from `parser/stmt.rs` (returning a `(String, Vec<Term>)` tuple after a small refactor) is reused for all four; each verb wraps the tuple in its own IR shape:

| Surface | IR wrapper |
|---|---|
| `admit Foo(args)` | `Stmt::Assert(Claim { predicate, args })` |
| `retract Foo(args)` | `Stmt::Retract { predicate, args }` |
| `emit Foo(args)` | `Stmt::Emit(Intent { name, args })` |
| `bind Foo(args)` | `Stmt::BindOne(Expr::Claim { predicate, args })` (unchanged from P3b1) |

Note `Intent`'s field is `name`, not `predicate`; intents are not claims even though they share the parsed shape. The parser maps the same tuple to the right field name.

**Recursive statement parser.** `for x in coll: body` introduces nested layout: a new Indent inside the transformation's outer Indent. The statement parser is therefore restructured with chumsky's `recursive` combinator so `for_stmt` can reference the full statement parser for its body. The `for` collection is parsed as a full expression (matches the IR's `Stmt::For.collection: Expr`); whatever it evaluates to must be an `EvalValue::Collection` at runtime, but the surface accepts any expression - same flexibility as `forall`'s source.

**The `expression_parser() + Clone` change.** Making the statement parser recursive required `expression_parser()` to return a `Clone`-able parser (so the closure could capture a single instance and clone it for each statement form that uses expressions). Added `+ Clone` to the function's return-type bounds; this is a no-op at runtime (chumsky parsers are clonable when their sub-parts are) but the bound has to be declared so the compiler can verify it.

**Considered and rejected:**

- *Including date-comparison surface (`<=` on dates) in P3b2.* The clinical-trial example uses civil-date `<=` extensively, and P3b2 is the natural place to land it - but it is *not* statement syntax; it's expression-level surface design. Putting it in P3b2 would mix two unrelated decisions (statements vs comparator dispatch). Kept it deferred; the date-`<=` in clinical-trial parses today as `Expr::Le` (decimal) and would TypeMismatch at runtime, which is acceptable for v0 until a separate small PR settles the surface. Two viable shapes for that future PR: a new keyword (`on_or_before`, `before`) or letting `<=` lower to `Expr::DateLe` when operands are date-shaped. The design-history's `DateLe` entry already rejected dispatch-on-operand-type; the cleanest future answer is probably a separate keyword.
- *Statement-level error recovery.* Sync at next statement keyword would let one bad statement not skip the rest of the body. Adds design surface; deferred until a worked example forces it.
- *Restricting `for` collection to a bare variable.* The IR accepts any `Expr`; the surface accepts any expression for symmetry with `forall`'s source. If a real example produces ill-shaped collections, restrict at parse later.

**What landed:**

- `claim_pattern` helper refactored to return `(String, Vec<Term>)` instead of `Expr::Claim`; the four verbs (`bind`, `admit`, `retract`, `emit`) share it.
- Statement parser restructured as `recursive(|statement| ...)`; `for_stmt` references the recursive parser for its body.
- `expression_parser()` return type gains `+ Clone` bound.
- 10 new transformation parsing tests covering each new statement form, statement-order preservation, nested `for`, mixed body with `for` plus before/after statements, empty for body, top-level admit/for rejection.
- `examples_parse_status.rs` rewritten: four examples now assert full parse; two assert "stops at `derived`".
- Long multi-line require body in `examples/06_clinical_trial_enrolment/` wrapped in parens (parens disable layout, the natural escape hatch for multi-line expressions that don't fit the same-column rule).

**What deliberately stayed out (P3c):**

- `derived` claim declarations. The keyword is lexer-reserved; the parser surfaces an unexpected-token diagnostic. The worked examples that declare derived claims (currently the ledger and the insurance settlement) stop here.
- The `format_program → parse_program` round-trip property test. Lands in P3c when the full surface is parseable.
- Date-comparison surface form. Its own small PR after P3c, when an example actually needs it in a `.morph`-runnable transformation.

**Pattern note:** the recursive statement parser is the first place in the parser arc where the grammar is genuinely self-referential. P2a/P2b-lite's `expression_parser` is also recursive (for nested parens, quantifiers inside quantifiers, etc.) but that was within a single closure. P3b2 has cross-statement recursion: `for` body contains statements which might contain another `for`. The layout pass's matched `Indent`/`Dedent` pairs bound the recursion; without that, the parser would have no way to know where a `for` body ends.


### Parser P3-dates: civil-date comparison surface (`on_or_before`)

**Forced by:** the clinical-trial-enrolment worked example. P3b2 left clinical-trial parsing end-to-end via decimal `<=` for date operands - syntactically successful but semantically wrong: those `<=` expressions lower to `Expr::Le`, which type-checks its operands as `EvalValue::Decimal` and would raise `TypeMismatch` at runtime against the `Date` operands the example actually passes in. The surface needed a comparator that lowers to `Expr::DateLe` (the kernel's separate civil-date primitive) before any of the clinical-trial transformations could be runnable from `.morph` source.

**The surface choice: `on_or_before` keyword.** Three candidates were on the table:

- Operator-dispatched `<=` (lowering decided at parse time by operand kind). Rejected by the original `Expr::DateLe` design-history entry; the kernel deliberately separates `Le` and `DateLe` to give each its own type-check, and the parser doesn't have a type environment to dispatch from anyway.
- A new symbolic operator like `<:=`. Compact but code-flavoured; would not match Morpholog's verb-keyword aesthetic where the surface reads as business prose.
- A new keyword. Picked: `on_or_before`. It reads as a regulatory clause (`effective_from on_or_before randomisation_date`), aligns with the `[from, to]` inclusive-window doctrine, and is distinct enough from decimal `<=` that the reader cannot confuse them.

**Implementation.** Lexer adds `Token::KwOnOrBefore`; parser adds it to the comparator-level production alongside `=`/`!=`/`<=`/`in`. Both sides accept full expressions (the IR's `Expr::DateLe(Box<Expr>, Box<Expr>)` matches this shape). The parser does no type checking on the operands; if a user writes `amount on_or_before limit` against decimal-typed variables, the parser accepts and the kernel raises `TypeMismatch` at evaluation - the same pattern P2a uses for arithmetic in `Neq`'s LHS.

**Why no parse-time type check on operands.** The parser has no type environment - predicate declarations carry kinds, but expression operands are bound at runtime and their kinds aren't statically tracked through `bind` / `let`. Adding a parser-side type pass would be a substantial new layer for marginal benefit; the runtime already catches the mismatch with a clear message. Per the doctrine of "smallest possible increment", leave it.

**Considered and rejected:**

- *Restricting `on_or_before` to specific argument shapes (e.g. only `Var` operands).* The clinical-trial use cases include both `var on_or_before var` and `var on_or_before claim_arg`; restricting would either reject legitimate patterns or add complexity. The expression-level shape mirrors `<=` and is the right floor.
- *Adding `before` (strict `<`) at the same time.* The doctrine in `Expr::DateLe`'s entry already said `DateLt`/`DateGt`/`DateGe` each earn their place when an example forces them. Inclusive `on_or_before` is what clinical-trial needs; strict ordering can land separately.
- *Mirroring decimal `<=` with a `<=` overload that the parser dispatches.* Would require operand-kind inference at parse time, which the parser doesn't do. Also relitigated the original design-history decision that explicitly rejected operator overloading.

**What landed:**

- `Token::KwOnOrBefore` in the lexer.
- `CmpOp::DateLe` discriminator in the parser; the comparison production accepts `on_or_before` at the same precedence as `<=`, lowers to `Expr::DateLe`.
- Doctrine table row in `scope-and-ambition.md` mapping `on_or_before` -> `Expr::DateLe`.
- New `consent_obtained_before_randomisation` invariant in `clinical_trial_enrolment.morph`, exercising `on_or_before` inside an invariant body (the rest of the file uses it in `require` bodies). Forces the surface to work at both expression-in-invariant and expression-in-require levels.
- All 11 date-`<=` sites in clinical-trial's randomise_participant transformation updated to `on_or_before`.
- Header comment in clinical-trial.morph updated to describe the new surface.
- New parser tests pinning `on_or_before` lowering to `Expr::DateLe`, decimal `<=` still lowering to `Expr::Le`.

**What stays out:**

- Strict-ordering date comparators (`before`, `after`, `on_or_after`). Land when an example forces them.
- Date arithmetic, intervals, business calendars. Locked decision: deferred until a worked example needs them.
- A type-aware static checker for comparator operands. The runtime catches misuse; static checking is bigger than this PR.


### Parser P3c: derived claims + round-trip property test

**Forced by:** the parser arc finale. P3-dates left two worked examples (ledger and insurance-claim-settlement) stopping at the `derived` keyword. P3c adds the production and ties the formatter and parser together with a round-trip property test.

**The surface, forced by the IR.** Reading the `enumerate_derived` evaluator showed that each `DerivedValue`'s expression is evaluated against `per_key` bindings only - values do not see one another. So the surface has only `value <name> = <expr>` clauses; no `let` for intermediate bindings between values. That settled the design question ChatGPT had raised in the planning phase.

```morph
derived TrialBalanceRow(account):
    over JournalLine(_, account, _, _)
    value balance = sum(d | JournalLine(_, account, d, _)) - sum(c | JournalLine(_, account, _, c))
```

**Grammar:** `derived <Name>(<key-list>): Indent over <expr> value <name> = <expr>+ Dedent`. Free interleaving with the other top-level decls. Parser-side duplicate-derived-name detection mirrors predicates, invariants, and transformations.

**Round-trip property test.** `tests/round_trip.rs` runs every `all_programs()` entry through `format_program -> parse_program -> assert_eq`. Catches both formatter drift and parser regressions in one move. Adding a new worked example automatically extends coverage.

**The formatter rewrite forced by round-trip.** `format_expr_inline` was emitting debug-style output (`and(a, b)`, `not(x)`, `date_le(a, b)`, `$actor`, `"subject"`) - readable but not parseable. The round-trip test made the divergence visible. Fixed:

- Verbs: `assert` -> `admit`, `bind_one` -> `bind`, `new_subject()` -> `new Subject()`.
- Boolean: `and(...)` -> infix `a and b and c`; `not(x)` -> prefix `not x`; `implies(a, b)` -> infix `a implies b`.
- Comparators: `==` -> `=`; `date_le(a, b)` -> `a on_or_before b`.
- Quantifiers: `exists(x, body)` -> `exists x: body`; `forall(x in src, body)` -> `forall x in src: body`. The forall source is detected for the auto-lifted `In(Var(binding), coll)` shape and unwrapped.
- Terms: `Term::Actor` -> `actor` (no sigil); `Value::Subject(s)` -> `#s`; `Value::Date(s)` -> `@s`.
- Derived claims: `derived Name(keys):` then `over <expr>` then repeated `value <n> = <expr>`. The older two-section form (`values: ...` block, then `over ...` block) is gone.
- Operator precedence is preserved by wrapping every composite operand in parens. Verbose but unambiguous; the parser collapses redundant parens.
- The multi-line `format_expr(e, depth)` printer is gone; `format_expr_inline` is the only expression printer. Invariant and derived-claim bodies all use the inline form.

**The actor-shadowing IR fix.** The round-trip surfaced that `approval_controls` and `insurance_claim_settlement` used `var("actor")` as transformation parameters distinct from `Term::Actor` (the proposing actor). The parser auto-maps bare `actor` to `Term::Actor`, so the round-trip lost the distinction. Renamed the parameter to `principal` in both Rust IR files; the semantic is clearer (the parameter is the *subject* of an authority claim, not the proposer) and the round-trip works.

**Considered and rejected:**

- *`let` clauses for intermediate bindings inside `derived` bodies.* Would require an IR extension (each `DerivedValue` would need to evaluate against earlier values, not just keys). The IR's current shape forces each value to be independent; the surface mirrors that. If a worked example surfaces a real need for intermediate bindings, the IR change comes first.
- *Parser-side operand-kind checking for `on_or_before`, `+`, `<=`, etc.* The parser has no type environment. Runtime catches misuse; static checking is a separate layer.
- *Smarter precedence-aware paren elision in the formatter.* Possible but more code; the verbose-always approach is correct and the round-trip test catches any drift. Optimisation deferred.

**What landed:**

- `KwOver` lexer token; `derived_decl` production in the parser.
- Parser-side duplicate-derived-name detection.
- Round-trip property test `tests/round_trip.rs` across every worked example.
- Formatter rewrite (`format_expr_inline`, `format_term`, `format_value`, `format_derived_claim`, `format_stmt`) to emit canonical surface text.
- `actor` parameter renamed to `principal` in `approval_controls.rs` and `insurance_claim_settlement.rs`.
- All six worked examples parse end-to-end. The `examples_parse_status.rs` integration test asserts full parse for all six (no longer needs "stops at X" assertions).
- Roadmap updated; the parser arc is complete for v0.

**What stays out:**

- A `morpholog run <file.morph>` command. Now reachable because the surface is complete; a separate small PR.
- `derived` claim materialisation, recursion, as-of, and visibility to invariants - each deferred deliberately. The first of those an example forces is the next move.
- Date-arithmetic, strict-ordering date comparators, and time-of-day - listed in [`runtime-semantics.md`](runtime-semantics.md) as awaiting examples.


### `Expr::Or` predicate-shaped disjunction

**Forced ahead of an example.** The honest second ahead-of-example deviation in the codebase, after [`Transition.actor`](#transition-value-object-and-auditactor). The next worked example - per-account delta conservation on the double-entry ledger - will use `Or` to express the creation-or-update split that pre-state lookups force ("either this account already had a balance and the delta equals the posting sum, or this is the first balance and the postings net to the opening value"). Landing `Or` first keeps that example's PR focused on the load-bearing primitive (`Expr::Pre`); bundling both would conflate two design moves that warrant separate scrutiny.

Standing rationale for the kernel addition independent of the example: every other predicate-shaped composer (`And`, `Not`, `Implies`, `Exists`, `Forall`) is first-class. Desugaring disjunction via De Morgan (`not (not A and not B)`) is technically equivalent but aesthetically wrong - it punishes the natural surface form to preserve a minimalism that was never the point. Minimalism is the absence of accidental ceremony, not the absence of primitives.

**Landed:**

- `Expr::Or(Vec<Expr>)` mirroring `Expr::And`'s flattened shape. A parser-level `a or b or c` lowers to a single `Or` node, not nested binary `Or`s.
- `find_disjunction` evaluator: concatenation of each branch's binding extensions against the same base context. No deduplication - multiplicity is preserved, matching `find_conjunction`'s convention. Downstream uses that care only about non-emptiness (`require`, invariants) are unaffected; `Forall` over an `Or` source iterates duplicates, which is the documented behaviour.
- `find_failing_subexpr` returns `None` for `Or`. When a disjunction fails, every branch failed - picking one to blame would mislead. Same rationale as `Not` and `Exists`.
- Surface keyword `or`, precedence layer between `and` and `implies`. Standard logical precedence: `a and b or c` parses as `(a and b) or c`; `a or b implies c` parses as `(a or b) implies c`; `not a or b` parses as `(not a) or b`.
- `or_()` DSL constructor for Rust-authored programmes, alongside `and`, `not`, `implies`.
- Walkers extended: `predicates_referenced_by_expr`, `validate_expr`, `format_expr_inline` all gain `Or` arms parallel to `And`.

**Considered and rejected:**

- *Deduplicating binding extensions across branches.* Adds cost on every `Or` evaluation; the existing `And` does not deduplicate either; downstream consumers that need it can apply dedup themselves. Documented in the `Or` IR variant's doc comment.
- *A two-branch `Or { left, right }` shape.* Symmetry with `Implies` was considered; the flattened `Vec<Expr>` won because `or` chains are common and the flattening already exists for `And`.

**What stays out:**

- Short-circuit evaluation. The current `find_disjunction` walks every branch and accumulates. A worked example with a measurable hot path would force a short-circuit optimisation; until then the simple shape is correct and clear.
