# Morpholog: IR and Runtime Semantics (v0)

Status: design doctrine. The IR and runtime are implemented in `crates/morpholog-core`; surface syntax and parser commitments remain deferred. This document is the authoritative source for the semantics; the code is one realisation of them.

Companion to [`scope-and-ambition.md`](scope-and-ambition.md), which fixes what Morpholog is for, what it should grow into, and what it must never become - and to [`design-history.md`](design-history.md), which records (retrospectively) which specific examples forced which design decisions.

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

**Programme vocabulary contract.** A [`Program`] declares the *vocabulary* of admissible claim shapes via `Program::predicates: Vec<PredicateDecl>`. Each `PredicateDecl` carries a name and a positional argument list with named, kinded positions. The kernel's `Program::validate()` is strict: every `claim`/`assert`/`retract`/`value_of`/derived-claim reference must target a declared predicate, and every reference must match the declared arity. Argument **kinds** (`Subject`, `Decimal`, `Date`, `Bool`, `Collection`, `Any`) are recorded as metadata and surface in `morpholog inspect predicates`; kind validation against the kinds of values flowing through the binding context is deferred until a worked example forces it. Intent declarations are deliberately out of scope - intents are outbox vocabulary, not claim vocabulary, and the asymmetry is captured rather than papered over.

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

Expression                       (inside invariants, requires, lets, comprehensions)
  literal | variable | claim-query | comprehension
  operators: == != <= and not implies, plus subtraction on Decimal
                                    (== takes Exprs; != takes Terms; <= takes Exprs
                                    and requires Decimal operands; Sub returns Decimal;
                                    no other arithmetic or comparison primitives yet)

Invariant
  name
  version                        -- always present; v0 is always 1
  body                           -- an Expression over candidate state

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
  Literal(Value)                 -- IR literal (Subject or Decimal)
  Actor                          -- resolves to the proposing transition's actor;
                                    in invariant bodies it raises EvalError::UnboundActor
                                    (authority checks live in `require`, not in invariants)

Statement
  require Expression             -- yes/no gate; does not export bindings
  bind_one Expression            -- unique lookup; replaces bindings with match
  let name = Expression          -- value-producing binding
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

- **`require Expression`** is a **yes/no gate**. It evaluates `Expression` against the pre-state snapshot; if the expression admits any match the statement succeeds, otherwise the proposal is rejected. The matches' bindings are **not** propagated back into the active scope: a `require Claim(x, y)` that uses fresh variable names `x` and `y` does not bind them for later statements. The require's only job is admission control.

- **`bind_one Expression`** is a **deterministic unique lookup**. It evaluates a predicate-shaped `Expression` against the pre-state, current bindings, and transition actor; the surviving binding set is treated as the next binding context.
  - Zero matches: the transformation is rejected (lawful business outcome: the expected governed record is not present).
  - One match: the returned binding set **replaces** the current binding context. Statements after a successful `bind_one` see the newly-bound variables.
  - Multiple matches: kernel error (`EvalError::TypeMismatch`). Multi-match means the programme thought something was unique but the admitted state did not make it unique - typically a missing structural-uniqueness invariant.
  
  `bind_one` is not iteration and does not branch. Use `for` for iteration. Use `require` for gates that should not export bindings. Use `let` for value-producing expressions.

- **`let name = Expression`** is the **value-producing binding** primitive. It evaluates `Expression` to a single value and binds `name` in the active scope for every subsequent statement. Use `let` for `Sum`, `Add`, `Sub`, `ValueOf` inside value position, and any other expression that computes rather than looks up.

- **`for binding in collection: body`** is **controlled iteration**. Variables bound inside the body are **scoped to the iteration**: they do not leak across iterations and do not survive the loop. Without this scoping, a residual `bind_one` binding from iteration N would constrain the lookup in iteration N+1; with it, each iteration sees only the outer bindings plus the iteration variable.

The idiomatic pattern for "this claim exists and I need values from it" - lifted from `examples/05_insurance_claim_settlement/insurance_claim_settlement.morph`:

```
bind_one ClaimReported(claim_id, policy_id, _)              -- existence + extraction in one step
bind_one Policy(policy_id, aggregate_limit)                 -- bound policy_id narrows; binds aggregate_limit
... statements that reference policy_id and aggregate_limit ...
```

The structural guarantee that each `bind_one` is single-valued comes from a programme-level invariant - e.g. `at_most_one_X_per_id` (the shape `verified_revenue::at_most_one_current_verification_per_asset_period` and `insurance_claim_settlement::at_most_one_policy_per_id` both use). Without that invariant, a duplicate admission would surface as `bind_one matched 2 candidates` (kernel error) rather than a lawful rejection.

The legacy `require + let + value_of` chain remains expressible (`Expr::ValueOf` is not deleted), and is the right tool when a value-producing position needs a lookup that does not fit a statement-level binding extension - inside arithmetic, inside `Sum`, or inside a derived-claim value expression.

Inside a `require` body, multiple sub-expressions composed with `And` *do* propagate bindings forward within that single require: the matcher's binding extensions are threaded through the conjuncts. So a require like

```
require And(
    Claim(P, x, limit),                             -- binds limit
    Le(amount, limit)                               -- consumes limit
)
```

is admissible: `limit` is bound by the `Claim` match and consumed by the `Le` comparison within the same require evaluation. What does *not* work is sequencing two separate `require` statements and expecting the second to see bindings from the first.

`Term::Actor` is reachable from inside any transformation-body statement that evaluates a `Term` (`require`, `let`, `assert`, `retract`, `emit`, `for`). It is **not** reachable from inside an invariant body or a derived claim's domain - both raise `EvalError::UnboundActor` at evaluation, because invariants evaluate against admitted state without a proposing transition in scope. This is the require-vs-invariant distinction made enforceable: authority checks belong in `require`, not in invariants.

## Tracing proposals

`propose_with_trace` is `propose`'s diagnostic twin. It returns a `TracedProposal` that carries a structured `Vec<TraceEntry>` on **both** the success path (`Completed { outcome, trace }`) and the kernel-error path (`Errored { error, trace }`). The error path matters most: a multi-match `bind_one`, a type-mismatch `DateLe`, an unbound `Term::Actor` - each surfaces as an `EvalError`, and `propose`'s `Result<Outcome, EvalError>` shape would discard the run-up that led to the failure. `propose_with_trace` does not.

One trace entry per transformation statement and per invariant check. `For` is nested - its `iterations` carry a sub-trace plus the iteration item per element. `Retract` records the actual retracted claims, not just a count. `BindOne` on a unique match records the full new binding context (sorted by variable name). `Require` records `match_count` on success, `reason` on rejection. Every expression-bearing entry renders its expression via `format_expr_inline`, so callers can assert on the failing predicate name instead of pattern-matching on reason strings.

**Scope: statement-level only.** The trace shows which statement failed; it does not drill into expression internals. A failing `require And(...)` shows the outer `require` as rejected with the rendered `And(...)` expression; it does not identify which conjunct of the `And` was false. Conjunct-level diagnostics would require a separate evaluator refactor (a `find_matches_with_trace`-style pass) and are deliberately deferred.

`propose` and `propose_with_trace` share a single execution path via an internal `TraceSink` enum. The non-trace path allocates no trace storage and the `On`-vs-`Off` check at each statement is a single-variant enum match the optimiser collapses to nothing meaningful; the trace path opts in by passing `TraceSink::On(&mut Vec<TraceEntry>)`. There is no separate "traced evaluator" that could drift from `propose`.

## Atomicity boundary

Steps 1-7 are atomic. Post-commit, outbox intents deliver at-least-once via workers running outside the transaction. External effects are never rolled back - only retried or compensated.

## Explicit non-goals for v0

- No surface syntax, lexer, or parser. The IR is constructed directly as data.
- No "claim language" or "intent language" exposed to users.
- No entities, classes, services, or projection forms.
- No invariant lifecycle. v0 has one canonical epoch; all invariants are `version: 1`, status `enforced`.
- No SQL generation from claim shapes. v0 uses a small hand-written PG schema at `crates/morpholog-core/sql/schema.sql` for the runtime tables (claims, audit, outbox).
- No model checker; the decidable-core spec is a later artefact.
- No units. No floating-point arithmetic.

## Success criterion

For every worked example, both in memory (via `propose()` and the kernel test suite) and durably (via `propose_against_pg`):

1. Valid transformations commit, writing one audit row and one outbox row per emitted intent in a single SERIALIZABLE transaction.
2. Invariant-violating attempts roll back atomically - no claims changed, no audit row written, no outbox row enqueued.
3. Outbox intents stage at commit but do not fire inside the database transaction; an external worker (the polling `OutboxWorker` in `morpholog-outbox`, with `StdoutDeliverer` as the canonical concrete deliverer) is the only path that delivers them.

These hold for every worked example under [`examples/`](../examples/). In-memory proofs live in [`crates/morpholog-examples/tests/`](../crates/morpholog-examples/tests/); the durable, PostgreSQL-backed proofs live in [`crates/morpholog-postgres/tests/`](../crates/morpholog-postgres/tests/).
