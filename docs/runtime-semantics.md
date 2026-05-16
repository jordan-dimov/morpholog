# Morpholog: IR and Runtime Semantics (v0)

Status: design doctrine. No code, no syntax, no parser commitments.

## Conceptual core

Morpholog has exactly two first-class concepts:

1. **Invariant** — a predicate that defines admissible state.
2. **Transformation** — a named, parameterised proposal to change state.

Everything that follows is **implementation machinery**, present only because something has to execute. It is strictly subordinate. Nothing below is promoted to the public language surface unless we are forced to.

We are not designing a claim language, an intent language, an entity model, or a general-purpose programming language. We are designing the smallest runtime that can answer one question.

## What the runtime answers

> Given a set of invariants `I`, a transformation `T(args)`, and the current admitted state `S`, does `T` commit?

Nothing else.

## Claims

A Morpholog `Claim` is an **admitted assertion** — not objective reality:

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
  operators: == != and or not implies, plus arithmetic on Decimal

Invariant
  name
  version                        -- always present; v0 is always 1
  body                           -- an Expression over candidate state

Transformation
  name
  parameters                     -- named bindings
  body                           -- list of Statements

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

## Atomicity boundary

Steps 1–7 are atomic. Post-commit, outbox intents deliver at-least-once via workers running outside the transaction. External effects are never rolled back — only retried or compensated.

## Explicit non-goals for v0

- No surface syntax, lexer, or parser. The IR is constructed directly as data.
- No "claim language" or "intent language" exposed to users.
- No entities, classes, services, or projection forms.
- No invariant lifecycle. v0 has one canonical epoch; all invariants are `version: 1`, status `enforced`.
- No SQL generation from claim shapes. v0 uses a small hand-written PG schema for claims, audit, outbox.
- No model checker; the decidable-core spec is a later artefact.
- No units. No floating-point arithmetic.

## Success criterion

The settlement netting program, constructed directly as IR data, executes such that:

1. A valid `create_net_settlement` commits, writes one audit record, and enqueues one outbox row.
2. An invariant-violating attempt (a double-netted line, or an amount mismatch, or a settlement with zero lines) rolls back atomically — no claims changed, no audit written, no outbox row.
3. The intent in the outbox row does not fire inside the database transaction.

Status (2026-05-16): items 1 and 2 are proved in memory via `propose()` and the netting test suite. Item 3 is structurally satisfied (intents stage as IR data, never resolved to actual side effects) but is not yet wired to PostgreSQL.
