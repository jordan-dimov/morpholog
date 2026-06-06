# Morpholog: IR and Runtime Semantics (v0)

Status: design doctrine. The IR and runtime are implemented in `crates/morpholog-core`; the `.morph` parser is implemented in `crates/morpholog-surface`. This document is the authoritative source for what the runtime *means*; the code is one realisation of them.

Companion to [`scope-and-ambition.md`](scope-and-ambition.md), which fixes what Morpholog is for, what it should grow into, and what it must never become - and to [`design-history.md`](design-history.md), which records retrospectively which worked example forced each design decision.

This doc uses IR names (`Stmt::BindOne`, `Stmt::Assert`, `Term::Actor`) because it describes the kernel. The `.morph` surface uses domain-flavoured verbs that map one-to-one: `bind`, `admit`, `actor` and so on - the full table is in the [Surface-to-IR mapping](#surface-to-ir-mapping) section below. When this doc says `bind_one`, the surface says `bind`.

## Conceptual core

Morpholog has exactly two first-class concepts:

1. **Invariant** - a predicate that defines admissible state.
2. **Transformation** - a named, parameterised proposal to change state.

Everything that follows is **implementation machinery**, present only because something has to execute. It is strictly subordinate. Nothing below is promoted to the public language surface unless we are forced to.

We are not designing a claim language, an intent language, an entity model, or a general-purpose programming language. We are designing the smallest runtime that can answer one question.

## What the runtime answers

> Given a set of invariants `I`, a transformation `T(args)`, and the current admitted state `S`, does `T` commit?

Nothing else.

## Claims

A Morpholog `Claim` is an **admitted assertion** - not objective reality:

> A statement admitted into governed state under a specific authority, epoch, and transformation.

`Quantity(trade123, 10)` does not mean "the trade has quantity 10." It means: *within this system, under this invariant epoch, by transition T, the claim `Quantity(trade123, 10)` has been admitted and may be relied upon.*

The same underlying event can have many legitimate views: trader-confirmed, risk-included, settlement-deliverable, regulator-reportable, counterparty-disputed. Conventional systems collapse these into a single mutable status (often a lie). Morpholog preserves them as separate admitted claims and lets invariants govern which claims may be used for which decisions. Example from energy revenue verification:

```
OptimiserReportedRevenue(asset, £100k)
BankRecognisedRevenue(asset, £92k)
OwnerExpectedRevenue(asset, £110k)
IndependentlyVerifiedRevenue(asset, £91.7k)
```

These are not contradictions. They are four admitted claims from four authorities. An invariant such as `DebtServiceCoverage(asset, r)` may require `r` to be computed using `IndependentlyVerifiedRevenue` only.

**v0 implications:** claims carry only `(predicate_name, arguments)`. Authority, epoch, validity windows, and exception status are deliberately deferred until a real example demands them. The minimalism rule stands: do not add fields without forcing pressure from a worked example. When a property wants to be metadata, first try to re-express it as a separate claim (a "claim about a claim").

**Programme vocabulary contract.** A [`Program`] declares two vocabularies: admissible claim shapes via `Program::predicates: Vec<PredicateDecl>` and outbox intent shapes via `Program::intents: Vec<IntentDecl>`, each a name plus a positional list of named, kinded arguments (the shared `ArgDecl`). Predicates and intents are separate namespaces. `Program::validate()` enforces the vocabulary strictly; the full contract is the "Authoring-time checks" section below.

## Claim semantics

Claims are **set-valued** in v0.

- Asserting an already-present claim is idempotent (no-op).
- Retracting a specific missing claim fails the transformation.
- Pattern-based retraction (`retract Pred(a, b, _)`) is idempotent on zero matches.
- Reads observe only the pre-transformation snapshot.
- Invariants evaluate against `CandidateState`, not the snapshot.

A claim's identity is its `(predicate_name, arguments)` tuple. The PG schema enforces uniqueness with a constraint, not via runtime de-duplication.

## Implementation machinery (subordinate)

```
Claim
  predicate_name
  arguments

Prop                             -- a proposition: searches a state, yields binding
                                    witnesses (zero, one, or many) - relational, not
                                    boolean. Evaluated by find_matches.
  Claim(predicate, args)         -- match a claim
  And | Or | Not | Implies       -- boolean composition
  Xor(Prop, Prop)                -- exactly-one; lowers to (a or b) and not (a and b)
  Exists | Forall                -- bounded quantification
  Pre(Prop)                      -- evaluate the subtree against pre-state
  Eq | Neq | Compare             -- value (in)equality and ordered comparison;
                                    operands are ValueExprs (== / != are kind-strict,
                                    <= requires Decimal, on_or_before requires Date)
  In(Term, Term)                 -- membership

ValueExpr                        -- a value expression: computes exactly one value.
                                    Evaluated by eval_value.
  Term                           -- a leaf (Var, Wildcard, Literal, Actor)
  Arith { op: ArithOp, left, right }  -- decimal arithmetic; op is one of
                                    Add | Sub | Mul | Div | Mod (infix + - * / %) or
                                    Min | Max (floor / cap, min(a,b) / max(a,b))
  Sum { value, body: Prop }      -- sum a value over a proposition's matches
  ValueOf { predicate, args, default }  -- unique-lookup value extraction
                                    (the two sorts are mutually recursive: a Compare
                                    operand is a ValueExpr, a Sum body is a Prop)

Invariant
  name
  version                        -- always present; v0 is always 1
  body                           -- a Prop over candidate state

Transformation
  name
  parameters                     -- named bindings
  body                           -- list of Statements

Transition                       -- the value object proposed against a Transformation
  transformation_name            -- must match the Transformation's name
  args                           -- per-call positional arguments
  actor                          -- EvalValue::Subject identifying who proposed this
                                    transition; persisted to audit.actor on commit;
                                    reachable from anywhere inside the transformation body
                                    (require/let/assert/retract/for/emit) via Term::Actor;
                                    not reachable from invariant or derived-claim bodies

Term                             -- a node inside a claim's args, a comprehension binding, etc.
  Var(name)                      -- bound by surrounding context
  Wildcard                       -- matches anything
  Literal(Value)                 -- IR literal (Subject, Decimal, or Date)
  Actor                          -- resolves to the proposing transition's actor;
                                    in invariant bodies it raises EvalError::UnboundActor
                                    (authority checks live in `require`, not in invariants)

Statement
  require Prop                   -- yes/no gate; does not export bindings
  bind_one Prop                  -- unique lookup; replaces bindings with match
  let name = ValueExpr           -- value-producing binding
  let name = new Subject()       -- generates a fresh UUIDv7
  assert Claim
  retract Pred(args...)          -- pattern-based; idempotent on zero matches
  for binding in collection: list of Statements  -- iteration; body is scoped
  emit Intent

Intent
  name
  arguments
  idempotency_key                -- hash(transition_id, name, args) by default;
                                    explicit override allowed

CandidateState
  Snapshot − retracted claims + asserted claims

AuditRecord
  transition_id                  -- UUIDv7
  transformation_name
  arguments
  actor                          -- the EvalValue::Subject from the proposed Transition
  invariant_epoch                -- which version-set governed this commit
  invariants_checked             -- list of (name, version)
  asserted_claims
  retracted_claims
  emitted_intents
  committed_at
```

## Surface-to-IR mapping

The `.morph` surface verbs map one-to-one onto the IR constructs above. The renames are domain-flavour and layout only: the surface is more business-native than the IR but never more expressive than it (the doctrine, and what it rules out, is in [`scope-and-ambition.md`](scope-and-ambition.md#surface-syntax-and-the-ir)). This table is the exact correspondence, with the reason each surface form is spelled the way it is.

| Surface verb | IR construct | Reason |
|---|---|---|
| `admit X(args)` | `Stmt::Assert` | Matches the runtime doctrine of "admitted claims". `assert` belongs to test frameworks; `admit` belongs to governed state. |
| `bind X(args)` | `Stmt::BindOne` | The `_one` suffix is redundant - there is no `bind_many`. `bind` reads as the binding-statement it is. |
| `actor` (no parens) | `Term::Actor` | A special variable bound by transition context, not a function. Parens would suggest function-call semantics it does not have. |
| `<=` `<` `>=` `>` (infix) | `Prop::Compare { op, domain: Decimal }` | Business mathematics reads with infix comparators. The operator is first-class - `amount > limit` renders and round-trips as written, never as `not (amount <= limit)` - while the ordered domain is a field, not a per-operator variant. Decimal-only operands; the domain is carried explicitly, so there is no operator overloading by operand kind. |
| `on_or_before` `before` `on_or_after` `after` (infix) | `Prop::Compare { op, domain: Date }` | Distinct keywords (not overloaded `<=`) for civil-date comparison; `on_or_*` are inclusive, `before`/`after` strict. Reads as business prose and aligns with the `[from, to]` inclusive-window doctrine. `before`/`after` are matched contextually (comparator position only), so they remain usable as variable names. Operands are type-checked as `EvalValue::Date`; using them on decimals surfaces as a runtime `TypeMismatch`. |
| `at_or_before` `strictly_before` `at_or_after` `strictly_after` (infix) | `Prop::Compare { op, domain: Timestamp }` | Instant comparison: "at" is the natural preposition for a point on the timeline, and the `strictly_*` forms keep the boundary explicit where a dispute would turn on it (laytime is settled to the minute). All four are contextual identifiers (comparator position only), extending the `before`/`after` precedent rather than reserving new words. |
| `no_longer_than` `shorter_than` `no_shorter_than` `longer_than` (infix) | `Prop::Compare { op, domain: Duration }` | Span comparison, read as length: `counted no_longer_than allowed` is the laytime sentence verbatim. Contextual identifiers, like the instant forms. |
| `=`, `!=` (infix) | `Prop::Eq` (ValueExpr, ValueExpr), `Prop::Neq` (ValueExpr, ValueExpr) | Both operate on full expressions and are symmetric: `a + 1 != b` is as legal as `a + 1 = b`. Not tied to one domain like the ordered comparators, but the two operands must share a kind - `=`/`!=` are kind-strict, with no silent coercion. |
| `+`, `-`, `*`, `/`, `%` (infix) | `ValueExpr::Arith { op, .. }` with `op: ArithOp::Add` / `Sub` / `Mul` / `Div` / `Mod` | Standard arithmetic, exact. `+`/`-` also carry the time matrix (instant + span = instant, instant - instant = span, span +/- span = span; `min`/`max` cap spans); `*`/`/`/`%` stay decimal-only. The matrix is enforced twice - `NoArithRule` at authoring time, `TypeMismatch` at evaluation - and a pair with no rule (adding two instants) is refused by name, not coerced; `*`/`/`/`%` bind tighter than `+`/`-`. The operator is a field, not a per-operator variant - the value-sort analogue of `Prop::Compare` carrying a `CompareOp`. Admission gates express ratio rules in multiplied form (`a <= c*b`, not `a/b <= c`) to stay exact; `/` is reserved for read-side projections, where a rounded figure is wanted; `%` is remainder, for parity and cyclic rules (`(file + rank) % 2`). A zero divisor on `/` or `%` surfaces `EvalError::DivisionByZero`. No unary minus until forced. |
| `min(a, b)`, `max(a, b)` | `ValueExpr::Arith { op, .. }` with `op: ArithOp::Min` / `Max` (decimal) | Floor and cap as self-delimiting functions, not infix - no extra precedence tier. Express layered limits, e.g. `min(limit, max(0, x))`. The `ArithOp::is_infix` predicate is what splits the printer (and the surface) between the infix operators above and these function-shaped ones. |
| `not`, `and`, `or`, `xor`, `implies` (keywords) | `Prop::Not`, `Prop::And`, `Prop::Or`, `Prop::Xor`, `Prop::Implies` | Boolean composition reads as keywords in business rules, not symbols. `and` flattens into `Prop::And(Vec<Prop>)` and `or` into `Prop::Or(Vec<Prop>)`; `implies` is right-associative. `xor` is exactly-one: it adds no expressiveness (it is `(a or b) and not (a and b)`, evaluated by lowering to exactly that), but reads far better than that hand-written form where the operands are long claim patterns. Binary, not flattened (n-ary xor is ambiguous); it sits between `and` and `or` in precedence. |
| `forall x in coll: body`, `exists x: body` | `Prop::Forall`, `Prop::Exists` | Bounded quantification is mathematical convention. The `in` clause on `forall` makes unbounded quantification syntactically impossible. `exists` carries no source clause because the IR's `Prop::Exists` doesn't model one - the bound variable is whatever the body matches. |
| `sum(target | body)` | `ValueExpr::Sum` | Set-builder notation. The target is a variable to sum, or a decimal literal - `sum(1 | body)` counts the matches (the chess material census forced this). Type-driven: a sum of decimals is a decimal, a sum of durations is a duration (counted laytime forced this); mixing is an error. The empty sum is decimal zero - the only choice that keeps every decimal aggregate working - so a duration aggregate seeds its own zero-length element (see the laytime example's commencement). A general expression target awaits an example that needs it. |
| `value Pred(args)` (with optional `default expr`) | `ValueExpr::ValueOf` | Claim-pattern form. The wildcard `_` in `args` marks the value position to extract. The kernel's `ValueOf { predicate, args, default }` is shaped this way deliberately; a `value(target | body)` shape would imply a general query and be more expressive than the IR. |
| `x in xs` (membership) | `Prop::In(Term, Term)` | Infix at comparator precedence. Distinct from the structural `in` in `forall x in xs: body`; disambiguated positionally (the structural `in` comes immediately after the binder in `forall`). |
| `@2026-05-22` | `Value::Date("2026-05-22")` | `@` sigil avoids the lexer ambiguity between bare ISO-8601 dates and arithmetic (e.g. `2026 - 05 - 22`). |
| `@2026-10-24T14:00:00Z` | `Value::Timestamp("2026-10-24T14:00:00Z")` | The same `@` sigil extended to a full RFC 3339 instant; the `T` time part distinguishes the token. Zone-less UTC by design: civil-time interpretation (port-local days, DST boundaries) is domain knowledge admitted as claims, never a hidden runtime tzdb assumption. Offsets are accepted and normalised to the instant they name. |
| `duration(PT6H)` | `Value::Duration("PT6H")` | An explicit constructor, deliberately boring: no bare-literal DSL, no quotes (the payload is identifier-shaped - ISO durations always start with `P` - and the surface has no string literals). Exact time units only; calendar units (months, years) are rejected by the type itself. `duration` is contextual (constructor only when followed by `(`), so it remains a legal variable name. |
| `#NAME` | `Value::Subject("NAME")` | `#` sigil makes subject literals visibly distinct from variables and reflects that subjects are opaque symbolic identifiers, not strings. |

## Execution semantics

```
1. Begin database transaction.
2. Snapshot the current set of claims. All reads in the transformation
   body see this snapshot only. No read-your-writes.
3. Execute T(args) against the snapshot, staging:
     - asserted claims
     - retracted claims
     - emitted intents
4. Construct CandidateState = Snapshot − Retracted + Asserted.
5. Evaluate every active invariant against CandidateState.
6. If any invariant fails: rollback. Nothing commits. No business audit record.
   No intents. (Failed transformations may be written to an operational
   rejection log later, outside the governed claim state.)
7. If all pass: commit claims + audit record + outbox rows in one
   database transaction.
```

External side effects fire only after commit, delivered by workers reading the outbox.

## Statements: the require / bind_one / let / for quartet

Four statement classes serve different binding purposes; conflating them is the most common modelling mistake when authoring a transformation.

- **`require Prop`** is a **yes/no gate**. It evaluates the `Prop` against the pre-state snapshot; if it admits any match the statement succeeds, otherwise the proposal is rejected. The matches' bindings are **not** propagated back into the active scope: a `require Claim(x, y)` that uses fresh variable names `x` and `y` does not bind them for later statements. The require's only job is admission control.

- **`bind_one Prop`** is a **deterministic unique lookup**. It evaluates the `Prop` against the pre-state, current bindings, and transition actor; the surviving binding set is treated as the next binding context.
  - Zero matches: the transformation is rejected (lawful business outcome: the expected governed record is not present).
  - One match: the returned binding set **replaces** the current binding context. Statements after a successful `bind_one` see the newly-bound variables.
  - Multiple matches: kernel error (`EvalError::TypeMismatch`). Multi-match means the programme thought something was unique but the admitted state did not make it unique - typically a missing structural-uniqueness invariant.
  
  `bind_one` is not iteration and does not branch. Use `for` for iteration. Use `require` for gates that should not export bindings. Use `let` for value-producing expressions.

- **`let name = ValueExpr`** is the **value-producing binding** primitive. It evaluates the `ValueExpr` to a single value and binds `name` in the active scope for every subsequent statement. Use `let` for `Sum`, `Arith` (arithmetic), `ValueOf` inside value position, and any other expression that computes rather than looks up.

- **`for binding in collection: body`** is **controlled iteration**. Variables bound inside the body are **scoped to the iteration**: they do not leak across iterations and do not survive the loop. Without this scoping, a residual `bind_one` binding from iteration N would constrain the lookup in iteration N+1; with it, each iteration sees only the outer bindings plus the iteration variable.

The idiomatic pattern for "this claim exists and I need values from it" - lifted from `examples/05_insurance_claim_settlement/insurance_claim_settlement.morph`:

```
bind_one ClaimReported(claim_id, policy_id, _)              -- existence + extraction in one step
bind_one Policy(policy_id, aggregate_limit)                 -- bound policy_id narrows; binds aggregate_limit
... statements that reference policy_id and aggregate_limit ...
```

The structural guarantee that each `bind_one` is single-valued comes from a programme-level invariant - e.g. `at_most_one_X_per_id` (the shape `verified_revenue::at_most_one_current_verification_per_asset_period` and `insurance_claim_settlement::at_most_one_policy_per_id` both use). Without that invariant, a duplicate admission would surface as `bind_one matched 2 candidates` (kernel error) rather than a lawful rejection.

The legacy `require + let + value_of` chain remains expressible (`ValueExpr::ValueOf` is not deleted), and is the right tool when a value-producing position needs a lookup that does not fit a statement-level binding extension - inside arithmetic, inside `Sum`, or inside a derived-claim value expression.

Inside a `require` body, multiple sub-expressions composed with `And` *do* propagate bindings forward within that single require: the matcher's binding extensions are threaded through the conjuncts. So a require like

```
require And(
    Claim(P, x, limit),                             -- binds limit
    Le(amount, limit)                               -- consumes limit
)
```

is admissible: `limit` is bound by the `Claim` match and consumed by the `Le` comparison within the same require evaluation. What does *not* work is sequencing two separate `require` statements and expecting the second to see bindings from the first.

`Term::Actor` is reachable from inside any transformation-body statement that evaluates a `Term` (`require`, `let`, `assert`, `retract`, `emit`, `for`). It is **not** reachable from inside an invariant body or a derived claim's domain - both raise `EvalError::UnboundActor` at evaluation, because invariants evaluate against admitted state without a proposing transition in scope. This is the require-vs-invariant distinction made enforceable: authority checks belong in `require`, not in invariants.

**The actor is asserted, not authenticated.** A `Transition` carries the actor as an `EvalValue::Subject`, and the runtime gates that asserted actor against whatever authority claims a `require` consults. It does **not** verify that the caller *is* that subject - authentication is the deployment's job, performed before `propose` is reached (the boundary that mints a `Transition` is where a session, token, or signature is checked). A `require` is the wrong place to reintroduce that: it has no host functions and no I/O, so cryptographic verification cannot and should not live inside one. Morpholog answers "is this actor permitted to do this, given admitted state?"; it trusts that the actor identity handed to it has already been established.

## Invariants: state vs transition, and `pre(...)`

An invariant is by default a predicate over the candidate (post) state - the world after the proposed transformation has staged its assertions and retractions. That covers structural rules like "balanced posted entry" or "at most one piece per square." `Prop::Pre(inner)` opts a wrapped subtree into pre-transition evaluation, so a single invariant can relate pre and post values:

```
invariant move_count_strictly_increases:
  MoveCount(n) and pre(MoveCount(m)) implies n = m + 1
```

Invariants that contain `Pre` are *transition invariants*; the distinction is descriptive (derivable by walking the body) rather than an IR kind. Both run through the same evaluator.

`pre(...)` is only legal in evaluation contexts that have a pre-state in scope - invariant evaluation during a proposal does, since the kernel passes both pre and candidate states to the invariant check. It surfaces `EvalError::PreStateUnavailable` inside a transformation `require` (pre-state is already the only state), inside a derived-claim body, inside the inner subtree of nested `pre`, and in any `find_matches` call whose `EvalContext` was constructed with `pre_state: None`. The error is phrased about evaluation context rather than AST position so future contexts that carry both states (transformation postconditions, trace assertions) can share the primitive without IR change.

Genesis falls out of `implies` vacuity: when a predicate has never been admitted, `pre(P(...))` matches nothing and any rule predicated on it is vacuously true. Initialisation against an empty database satisfies `move_count_strictly_increases` for free; once `MoveCount(0)` is admitted the rule kicks in. Authors who need a different genesis story write the cases as disjuncts.

Quantifier composition is non-commutative by design. `pre(forall x in S: body)` reads pre-state for both the iteration domain and the body; `forall x in S: pre(body)` iterates the post-state domain and only flips the body. The two coincide when the iteration set is fixed (a chess board always has 64 squares); they diverge for domains that grow or shrink between states (accounts, policies, claims). Choosing the right order is what makes a transition invariant honest about whether it is asking a question of the old world or the new.

## Tracing proposals

`propose_with_trace` is `propose`'s diagnostic twin. It returns a `TracedProposal` that carries a structured `Vec<TraceEntry>` on **both** the success path (`Completed { outcome, trace }`) and the kernel-error path (`Errored { error, trace }`). The error path matters most: a multi-match `bind_one`, a type-mismatch `DateLe`, an unbound `Term::Actor` - each surfaces as an `EvalError`, and `propose`'s `Result<Outcome, EvalError>` shape would discard the run-up that led to the failure. `propose_with_trace` does not.

One trace entry per transformation statement and per invariant check. `For` is nested - its `iterations` carry a sub-trace plus the iteration item per element. `Retract` records the actual retracted claims, not just a count. `BindOne` on a unique match records the full new binding context (sorted by variable name). `Require` records `match_count` on success, `reason` on rejection. Every proposition-bearing entry renders its proposition via `format_prop_inline`, so callers can assert on the failing predicate name instead of pattern-matching on reason strings.

**Scope: statement-level plus failure-walk on rejection paths.** Every statement that runs produces one trace entry. When a `require` or `bind_one` rejects, the trace's `failing_sub_expression` field carries the most specific responsible sub-expression - a failing conjunct of an `And`, an `Implies`'s consequent, a `Forall`'s body - recursing into compound ones; `Not`, `Exists`, and leaves return `None` (no single responsible sub-expression, or already maximally specific). The walk runs **only on rejection paths**, so success-path performance is unchanged, and the field is omitted from JSON when `None`. A fuller structural trace (success-path drill-downs, witness extraction, binding substitution) is deferred until an example forces it.

`propose` and `propose_with_trace` share a single execution path via an internal `TraceSink` enum, so there is no separate traced evaluator that could drift; the non-trace path allocates nothing.

## Authoring-time checks (`Program::validate`)

`Program::validate` runs before any state is touched and surfaces problems that would otherwise appear as `EvalError`s during a `propose`. It collects *every* error rather than failing on the first; a programme migration that adds predicate declarations should see the full work list at once.

The check is one traversal of every invariant, transformation, and derived-claim body. It surfaces several classes of problem in a single pass:

1. **Structural**: every claim reference must name a declared predicate, every `emit` reference must name a declared intent, every reference must match the declared arity, and no two declarations in the same vocabulary share a name. A derived claim's output arity must equal `keys.len() + values.len()`. Predicates and intents live in separate namespaces.

2. **Kind/type compatibility**: every value flowing into a slot must have a compatible kind. Declarations carry per-argument kinds (`Subject`, `Decimal`, `Date`, `Timestamp`, `Duration`, `Bool`, `Collection`, `Any`); comparators have fixed expected kinds per domain (`<=` Decimal, `on_or_before` Date, `at_or_before` Timestamp, `no_longer_than` Duration); `+`/`-` follow the time-arithmetic rule matrix (a pair of known kinds with no rule is `NoArithRule` at authoring time) while `*`/`/`/`%` stay Decimal-only; `sum` produces the kind it sums (Decimal or Duration, never mixed); equality (`==` / `!=`) is strict (`Subject == Decimal` is a kind error, not a silent coercion); variables are inferred-and-refined as they flow through claim slots, intent emits, comparators, and let-bindings.

3. **Binding flow**: a variable consumed where a bound value is required - an `admit`/`retract`/`emit` argument, a comparator or arithmetic operand, a `value` lookup key, a `sum` target - must have been bound first. The static walk follows the runtime exactly: parameters, `bind`, `let`, `for`, and claim matches in predicate position bind names; a `require` match does **not** export to later statements; a disjunction exports only the names bound in *every* branch (whichever branch's witness the runtime carries forward); `in` binds its element when it is otherwise unbound. A use of an unbound name is flagged as `UnboundVariable` - the same the kernel would raise.

4. **Shape**: enforced by the type system, not the checker. The two-sort IR (`Prop` searches state; `ValueExpr` computes a value) makes a value expression at a predicate position - or the reverse - unrepresentable, so neither the parser nor `ir_builder` can construct it. The former static shape check and the `NotPredicate` / `NotValue` kernel errors are gone; the evaluators are total over their sorts.

5. **Actor context**: `Term::Actor` referenced in an invariant or derived-claim body - where no proposing transition is in scope - is flagged (the kernel would raise `UnboundActor`). Authority checks belong in a `require`, not an invariant.

The kind and binding-flow walks share the require/bind_one/let/for quartet's export rules over one scoped environment, so a `require` body's refinements and bindings stay local while `bind_one` and `let` flow forward; `Sum`'s body is walked under a scoped env so iteration-variable refinements stay local, and the value term must resolve to Decimal; `ValueOf`'s wildcard slot determines its result kind and its optional default must agree. `Any` is treated as *unconstrained*, not as "compatible with everything forever once attached to a variable": a variable seen first in an `Any` slot stays open, and a later specific use refines it. `Any` is an escape hatch for declarations, not a kind-eraser for inference.

Diagnostics carry no source spans in v0 - the IR drops parser spans on lowering, and threading them through is a separate decision. Each error names the predicate / operator / variable involved and the context (which invariant, which transformation, which derived claim) so the call site is locatable from grep alone.

`Program::validate` also bounds nesting depth: a body whose expressions or `for`-statements nest past a fixed limit is rejected (`NestingTooDeep`) before any recursive walk runs on it. The evaluator and the check itself descend one stack frame per level, so an unbounded body could exhaust the stack during `propose`. This is the teeth behind the rule that **untrusted IR must be validated before it is proposed**: `propose` trusts the IR it is handed and does no programme-level check of its own, so a deployment that accepts IR from outside must run `Program::validate` first.

`Program::validate` is **not** called automatically by `propose`. The kernel boundary is statement-level, not programme-level; revalidating on every proposal would muddle that distinction and add overhead. The `morpholog check` CLI subcommand runs it explicitly; tests over the worked examples do the same.

## Atomicity boundary

Steps 1-7 are atomic. Post-commit, outbox intents deliver at-least-once via workers running outside the transaction. External effects are never rolled back - only retried or compensated.

## Explicit non-goals for v0

The doctrinal floors - no entities/classes/services, no workflow engine, no arbitrary computation inside transformations, no BI engine, no bypass flags - are in [`scope-and-ambition.md`](scope-and-ambition.md)'s Non-goals and not repeated here. The IR- and runtime-specific ones:

- No invariant lifecycle. v0 has one canonical epoch; all invariants are `version: 1`, status `enforced`.
- No SQL generation from claim shapes. v0 uses a small hand-written PG schema at `crates/morpholog-core/sql/schema.sql` for the runtime tables (claims, audit, outbox).
- No model checker; the decidable-core spec is a later artefact.
- No units. No floating-point arithmetic; decimal only.

## Success criterion

For every worked example, both in memory (via `propose()` and the kernel test suite) and durably (via `propose_against_pg`):

1. Valid transformations commit, writing one audit row and one outbox row per emitted intent in a single SERIALIZABLE transaction.
2. Invariant-violating attempts roll back atomically - no claims changed, no audit row written, no outbox row enqueued.
3. Outbox intents stage at commit but do not fire inside the database transaction; an external worker (the polling `OutboxWorker` in `morpholog-outbox`, with `StdoutDeliverer` as the canonical concrete deliverer) is the only path that delivers them.

These hold for every worked example under [`examples/`](../examples/). In-memory proofs live in [`crates/morpholog-examples/tests/`](../crates/morpholog-examples/tests/); the durable, PostgreSQL-backed proofs live in [`crates/morpholog-postgres/tests/`](../crates/morpholog-postgres/tests/).
