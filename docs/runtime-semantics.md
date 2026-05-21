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
  require Expression
  let name = Expression
  let name = new Subject()       -- generates a fresh UUIDv7
  assert Claim
  retract Pred(args...)          -- pattern-based; idempotent on zero matches
  for binding in collection: list of Statements
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

## Statements: gating vs binding

Two statement classes serve different purposes; conflating them is the most common modelling mistake when authoring a transformation.

- **`require Expression` is a yes/no gate.** It evaluates `Expression` against the pre-state snapshot; if the expression admits any match the statement succeeds, otherwise the proposal is rejected. The matches' bindings are **not** propagated back into the active scope: a `require Claim(x, y)` that uses fresh variable names `x` and `y` does not bind them for later statements. The require's only job is admission control.

- **`let name = Expression` is the binding primitive.** It evaluates `Expression` to a single value and binds `name` in the active scope for every subsequent statement (including later `require`s, `assert`s, `retract`s, intent `emit`s, and the body of a `for`). When `Expression` is `ValueOf(predicate, args)`, the let extracts a value position from a uniquely-matching claim.

The idiomatic pattern for "this claim exists and I need a value from it" - lifted verbatim from `examples/05_insurance_claim_settlement/insurance_claim_settlement.morph`:

```
require ClaimReported(claim_id, _, _)                       -- gate: the claim has been reported
let policy_id = value_of ClaimReported(claim_id, _, _)      -- extract: binds policy_id to the
                                                            -- first wildcard's position
... statements that reference policy_id ...
```

`value_of` finds a single claim matching the given pattern and returns the value at the first wildcard position (here, the second argument); zero matches surfaces as `EvalError::ValueOfZeroMatches`, more than one as `EvalError::ValueOfMultipleMatches`. The guard `require` rejects the proposal cleanly when the claim is absent; the subsequent `let` then extracts the value, with the structural guarantee that the lookup is single-valued (a property the programme must enforce via an invariant - e.g. `at_most_one_X_per_id`, the shape `verified_revenue::at_most_one_current_verification_per_asset_period` and `insurance_claim_settlement::at_most_one_policy_per_id` both use).

Inside a `require` body, multiple sub-expressions composed with `And` *do* propagate bindings forward within that single require: the matcher's binding extensions are threaded through the conjuncts. So a require like

```
require And(
    Claim(P, x, limit),                             -- binds limit
    Le(amount, limit)                               -- consumes limit
)
```

is admissible: `limit` is bound by the `Claim` match and consumed by the `Le` comparison within the same require evaluation. What does *not* work is sequencing two separate `require` statements and expecting the second to see bindings from the first.

`Term::Actor` is reachable from inside any transformation-body statement that evaluates a `Term` (`require`, `let`, `assert`, `retract`, `emit`, `for`). It is **not** reachable from inside an invariant body or a derived claim's domain - both raise `EvalError::UnboundActor` at evaluation, because invariants evaluate against admitted state without a proposing transition in scope. This is the require-vs-invariant distinction made enforceable: authority checks belong in `require`, not in invariants.

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
