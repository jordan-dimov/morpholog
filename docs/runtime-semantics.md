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
  Defined(name, args)            -- call a named Definition (see below); claim-shaped,
                                    resolved by name against Program.definitions
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

Definition
  name
  parameters                     -- named bindings, kinds inferred from the body
  body                           -- a Prop over the parameters only (context-free)

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
  assert Claim                   -- IR name; the surface verb is `admit`
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
  committed_at                   -- the writer's transaction START instant, not the
                                    commit instant; (committed_at, transition_id) is
                                    the stable total replay order every audit read
                                    (verify, coverage, as-of, the inspect-audit tail)
                                    orders by
```

## Surface-to-IR mapping

The `.morph` surface verbs map one-to-one onto the IR constructs above. The renames are domain-flavour and layout only: the surface is more business-native than the IR but never more expressive than it (the doctrine, and what it rules out, is in [`scope-and-ambition.md`](scope-and-ambition.md#surface-syntax-and-the-ir)). This table is the exact correspondence, with the reason each surface form is spelled the way it is.

| Surface verb | IR construct | Reason |
|---|---|---|
| `admit X(args)` | `Stmt::Assert` | Matches the runtime doctrine of "admitted claims". `assert` belongs to test frameworks; `admit` belongs to governed state. |
| `bind X(args)` | `Stmt::BindOne` | The `_one` suffix is redundant - there is no `bind_many`. `bind` reads as the binding-statement it is. |
| `unique by (fields)`, `append only`, `current pointer by (fields)`, `superseded via L` (clauses on a `predicate` declaration) | `Discipline::{UniqueBy, AppendOnly, CurrentPointerBy, SupersededVia}` on `PredicateDecl.disciplines` | Claim disciplines (see "Claim disciplines" above). Every clause word is contextual, not reserved - the `before`/`duration` precedent - so all stay usable as variable names. Clauses sit inline after the argument list or on indented continuation lines. |
| `define name(params): body` / `name(args)` at call sites | `Definition` / `Prop::Defined` | A named, parameterised proposition (see "Definitions: named propositions" above). The call is spelled exactly like a claim reference - a condition should read no differently from the evidence it checks - and resolves by name against the declared definitions, which is why definition and predicate names share one namespace and may not collide. Definition names are snake_case by convention (they name rules, like invariants), predicates CamelCase (they name claim shapes); the convention aids the reader, not the resolver. |
| `actor` (no parens) | `Term::Actor` | A special variable bound by transition context, not a function. Parens would suggest function-call semantics it does not have. |
| `<=` `<` `>=` `>` (infix) | `Prop::Compare { op, domain: Decimal }` | Business mathematics reads with infix comparators. The operator is first-class - `amount > limit` renders and round-trips as written, never as `not (amount <= limit)` - while the ordered domain is a field, not a per-operator variant. The decimal domain admits two flavours: bare decimals, and unit-tagged quantities of the SAME unit (a `Decimal[U]` IS a decimal, under a contract label the comparison must respect - `Decimal[USD]` against `Decimal[t]` is refused by name). The domain is carried explicitly, so there is no operator overloading by operand kind. |
| `on_or_before` `before` `on_or_after` `after` (infix) | `Prop::Compare { op, domain: Date }` | Distinct keywords (not overloaded `<=`) for civil-date comparison; `on_or_*` are inclusive, `before`/`after` strict. Reads as business prose and aligns with the `[from, to]` inclusive-window doctrine. `before`/`after` are matched contextually (comparator position only), so they remain usable as variable names. Operands are type-checked as `EvalValue::Date`; using them on decimals surfaces as a runtime `TypeMismatch`. |
| `at_or_before` `strictly_before` `at_or_after` `strictly_after` (infix) | `Prop::Compare { op, domain: Timestamp }` | Instant comparison: "at" is the natural preposition for a point on the timeline, and the `strictly_*` forms keep the boundary explicit where a dispute would turn on it (laytime is settled to the minute). All four are contextual identifiers (comparator position only), extending the `before`/`after` precedent rather than reserving new words. |
| `no_longer_than` `shorter_than` `no_shorter_than` `longer_than` (infix) | `Prop::Compare { op, domain: Duration }` | Span comparison, read as length: `counted no_longer_than allowed` is the laytime sentence verbatim. Contextual identifiers, like the instant forms. |
| `a <= x <= b` (chained ordered comparators, any length) | `Prop::And` of pairwise `Prop::Compare` | A range reads as spoken: `0 <= rate <= 1` is the borrowing-base advance-rate band the way its README already says it. Pure surface - the chain lowers to exactly the And of two-operand comparisons the spelled-out `and` form produces, so it adds no expressive power and every downstream consumer (evaluation, explain, coverage) sees the shape it already knows. Only the ordered comparators chain (all domains; each link carries its own), every link must point the same way (`<=`/`<` or `>=`/`>` - a mixed-direction chain is refused, not guessed at), and `=`/`!=`/`in` do not chain. The formatter's canonical output stays the expanded form; the chain is accepted on the way in, never re-sugared on the way out. |
| `let name = (value)` lines opening an indented `define` or `invariant` body | none - substituted away before the IR exists | An algebraic abbreviation, not a runtime binding: every use of `name` is replaced by the value at parse time, so the kernel, the formatter, and `canonical_hash` all see the desugared rule - naming an intermediate value cannot change what rule the programme is. Variables inside the value are ordinary Morpholog variables and take exactly the meaning they have after substitution at the use site (a value mentioning `kwh` used under `forall ... Reading(m, kwh)` reads the claim-bound `kwh` - deliberate, and pinned by test). The parens around the value are required grammar: parens already mean "layout off", so the value can span lines freely, and a value ending in a bare decimal can never absorb the next line's identifier as a quantity unit. Lets are sequential - a value may use earlier lets only, and a self- or forward-reference is refused by name, never resolved by substitution order - and refused rather than half-supported at every other edge too: duplicate names, parameter collisions, quantifier-binder collisions (shadowing is refused, not implemented; the sum target is not a binder - it is consumed against the sum body's bindings, so a let substitutes into it like any term slot), `actor` as a name, a computed value in a term-only position (claim/call arguments, membership, sum targets, `value` lookups take plain terms), dead bindings (transitively), and expansion past a node budget. Distinct from the statement `let` in transformation bodies, which IS a runtime binding (`Stmt::Let`). |
| `const name = (value)` (top-level declaration) | none - substituted away before the IR exists | The programme-wide sibling of the body `let`: one figure the whole rulebook shares (a rounding quantum, a conversion divisor), named once and written out in place before the rules take effect - so `canonical_hash` is identical to the hand-inlined spelling and naming a shared figure provably cannot change the rules. Where a body `let` is body-local, a `const` REACHES every body sort: invariant and define bodies, derived `over`/`value` clauses, and transformation statements (values freely; claim/call/pattern arguments as term slots, where a computed const is refused by name). Two properties keep it honest. A const initialiser is CLOSED: literals and earlier consts only - a free variable would capture a different local at every use site, `actor` varies per proposal, and `sum`/`value` read state; all refused. And a const name may NOT stand in a pattern position (claim patterns, defined calls, `bind`): substituting there would silently turn a relational binding into a literal filter, shrinking a rule's universe from far away - refused, with the fix in the message (match a variable, compare explicitly). Constructive and resolved slots (`admit`/`emit`/`retract` arguments, `value` lookup keys, sum targets) stay ordinary uses. Consts are sequential among themselves (earlier-only; self- and forward-references refused) and refused rather than half-supported at every other edge: duplicates, `actor` as a name, dead consts (transitively), computed values in constructive term slots, and a const name colliding with any parameter, quantifier binder, statement binding, derived key, or body `let` anywhere in the programme (a programme-wide name must be programme-wide unambiguous; shadowing is refused, not implemented). |
| `=`, `!=` (infix) | `Prop::Eq` (ValueExpr, ValueExpr), `Prop::Neq` (ValueExpr, ValueExpr) | Both operate on full expressions and are symmetric: `a + 1 != b` is as legal as `a + 1 = b`. Not tied to one domain like the ordered comparators, but the two operands must share a kind - `=`/`!=` are kind-strict, with no silent coercion. |
| `+`, `-`, `*`, `/`, `%` (infix) | `ValueExpr::Arith { op, .. }` with `op: ArithOp::Add` / `Sub` / `Mul` / `Div` / `Mod` | Standard arithmetic, exact. `+`/`-` also carry the time matrix (instant + span = instant, instant - instant = span, span +/- span = span; `min`/`max` cap spans) and the unit algebra (same-unit quantities add, subtract, cap, and compare; a bare decimal scales a quantity via `*`/`/`; the ratio of two same-unit quantities - and of two durations - is a bare decimal, exact for terminating ratios: `excess / duration(PT24H)` gives exactly 5.5 for a 132-hour excess, while a repeating ratio carries decimal precision rather than a hidden float). Nothing produces a unit that was not already written down: no compound units, no unit-producing multiplication, and `%` stays decimal-only. The matrix is enforced twice - `NoArithRule` at authoring time, `TypeMismatch` at evaluation - and a pair with no rule (adding two instants) is refused by name, not coerced; `*`/`/`/`%` bind tighter than `+`/`-`. The operator is a field, not a per-operator variant - the value-sort analogue of `Prop::Compare` carrying a `CompareOp`. Admission gates express ratio rules in multiplied form (`a <= c*b`, not `a/b <= c`) to stay exact; `/` is reserved for read-side projections, where a rounded figure is wanted; `%` is remainder, for parity and cyclic rules (`(file + rank) % 2`). A zero divisor on `/` or `%` surfaces `EvalError::DivisionByZero`, and a result outside the exact decimal range surfaces `EvalError::ArithOutOfRange` - exactness or a named refusal, never an approximation, never a panic. Both are KERNEL evaluation errors (the adapter's `PgError::Kernel`, the CLI's error envelope), operational and without audit standing - never business rejections; a domain where extremes or zero divisors are business-possible guards them in its own rules. `sum` totals are accumulated wider than the value type, so only the final total decides representability - the answer depends on the SET of matched claims, never their iteration order. No unary minus until forced. |
| `min(a, b)`, `max(a, b)` | `ValueExpr::Arith { op, .. }` with `op: ArithOp::Min` / `Max` (decimal) | Floor and cap as self-delimiting functions, not infix - no extra precedence tier. Express layered limits, e.g. `min(limit, max(0, x))`. The `ArithOp::is_infix` predicate is what splits the printer (and the surface) between the infix operators above and these function-shaped ones. |
| `round(x, quantum)` | `ValueExpr::Round { value, quantum }` | The multiple of `quantum` nearest to `x`, exact halves AWAY FROM ZERO (`round(0.125, 0.01)` is `0.13`; `round(0 - 0.125, 0.01)` is `-0.13` - the symmetry that keeps credit notes fair). One mode only, measured against a real billing convention; a second policy joins as a parameter when a real bill forces it, never speculatively. Decimal-only in v0: money rounds as a bare decimal under the currency-in-field-names convention, and a quantity mode waits for its example. A non-positive quantum is refused by name - at authoring time when literal, at evaluation otherwise. A dedicated node for the `abs` reasons: the operand evaluates once, the form round-trips as written, and it can never be mistaken for the sign-branched shift-and-remainder spelling it replaces. Function-shaped like `min`/`max`: self-delimiting, no new precedence tier. |
| `not`, `and`, `or`, `xor`, `implies` (keywords) | `Prop::Not`, `Prop::And`, `Prop::Or`, `Prop::Xor`, `Prop::Implies` | Boolean composition reads as keywords in business rules, not symbols. `and` flattens into `Prop::And(Vec<Prop>)` and `or` into `Prop::Or(Vec<Prop>)`; `implies` is right-associative. `xor` is exactly-one: it adds no expressiveness (it is `(a or b) and not (a and b)`, evaluated by lowering to exactly that), but reads far better than that hand-written form where the operands are long claim patterns. Binary, not flattened (n-ary xor is ambiguous); it sits between `and` and `or` in precedence. |
| `forall x in coll: body`, `exists x: body` | `Prop::Forall`, `Prop::Exists` | Bounded quantification is mathematical convention. The `in` clause on `forall` makes unbounded quantification syntactically impossible. `exists` carries no source clause because the IR's `Prop::Exists` doesn't model one - the bound variable is whatever the body matches. |
| `sum(target | body)` | `ValueExpr::Sum` | Set-builder notation. The target is a variable to sum, or a decimal literal - `sum(1 | body)` counts the matches (the chess material census forced this). Type-driven: a sum of decimals is a decimal, a sum of durations is a duration (counted laytime forced this), a sum of same-unit quantities is a quantity of that unit (the cargo book forced this); mixing kinds - or units - is an error. The empty sum is the typed zero of the summed variable's declared kind (`0 t` over a `Decimal[t]` position, `duration(PT0S)` over a duration, decimal zero otherwise), resolved statically by the `lower_sum_seeds` pass - so an empty cargo book compares against a capacity with no zero-valued seed claim needed to open it. A count sum's literal target, or a variable no declaration decides, stays decimal. A general expression target awaits an example that needs it. |
| `max(target \| body)` / `min(target \| body)` | `ValueExpr::Extremum` | The largest or smallest target over the bindings the body admits - the selection a governing-claim rule needs on the commit path ("the version in force on this date" is the greatest `effective_from` not after it). Shaped like `sum` without a seed, because that is the whole difference: an empty sum has a typed zero to give, an empty extremum has no answer at all and raises `EmptyExtremum` rather than inventing one - guard it with a `require` when "none in force" should be a lawful rejection instead of an error. Ordered kinds only - decimals, dates, timestamps, durations and same-unit quantities. Subjects are opaque identifiers, booleans are not a scale, and a collection is not a point on one, so none has a largest member; the check is an allow-list, so a kind added later has no order until someone decides it does. Distinct from the binary `min(a, b)` / `max(a, b)` that caps one value against another; the `\|` tells them apart. **Why both this and the negated-exists idiom exist:** an invariant asking "is this the version in force?" only needs a truth, and `not (exists later: ...)` gives one; a transformation needs the value to look the claim up by, and `require` does not export bindings while `bind` needs a determined key. The aggregate turns that truth test into a key. |
| `value Pred(args)` (with optional `default expr`) | `ValueExpr::ValueOf` | Claim-pattern form. The wildcard `_` in `args` marks the value position to extract. The kernel's `ValueOf { predicate, args, default }` is shaped this way deliberately; a `value(target | body)` shape would imply a general query and be more expressive than the IR. |
| `derived Name(keys):` with an indented `over domain` clause and `value name = expr` clauses | `DerivedClaim { predicate, keys, values, domain }` | A governed read-side view, recomputed from admitted claims on demand - never stored, never admitted, so it cannot drift from the claims it reads. The head lists the KEY fields; enumeration yields one row per distinct key-tuple the `over` domain binds, and each `value` clause computes one output field against that row's bindings. The row is positional: keys, then values, in declaration order. The rules this shape enforces: at least one `value` clause is required (a derived is a computation over a domain, not a stored relation - a value-less "which subjects match" view is a domain query the embedder runs, or an invariant if it must be guaranteed); every non-computed field the row carries must be a key the domain binds (there is no projection list separate from the head); and value expressions see the per-key bindings only, never one another (no `let`-style chaining between clauses). |
| `x in xs` (membership) | `Prop::In(Term, Term)` | Infix at comparator precedence. Distinct from the structural `in` in `forall x in xs: body`; disambiguated positionally (the structural `in` comes immediately after the binder in `forall`). |
| `@2026-05-22` | `Value::Date("2026-05-22")` | `@` sigil avoids the lexer ambiguity between bare ISO-8601 dates and arithmetic (e.g. `2026 - 05 - 22`). |
| `@2026-10-24T14:00:00Z` | `Value::Timestamp("2026-10-24T14:00:00Z")` | The same `@` sigil extended to a full RFC 3339 instant; the `T` time part distinguishes the token. Zone-less UTC by design: civil-time interpretation (port-local days, DST boundaries) is domain knowledge admitted as claims, never a hidden runtime tzdb assumption. Offsets are accepted and normalised to the instant they name. |
| `duration(PT6H)` | `Value::Duration("PT6H")` | An explicit constructor, deliberately boring: no bare-literal DSL, no quotes (the payload is identifier-shaped - ISO durations always start with `P` - and the surface has no string literals). Exact time units only; calendar units (months, years) are rejected by the type itself. `duration` is contextual (constructor only when followed by `(`), so it remains a legal variable name. |
| `qty: Decimal[t]` (declaration) | `PredicateArgKind::Quantity(Unit)` | The unit annotation rides the kind keyword: a `Decimal[U]` is an exact decimal under a contractual label, so the declaration syntax says exactly that. Units are opaque, case-sensitive symbols - no registry, no aliases, no compound symbols (`USD/day` is a field name and a formula, never a unit). Only `Decimal` takes the brackets; a unit on any other kind is a parse error. |
| `25000 USD`, `0 t` | `Value::Quantity { amount, unit }` | Amount-then-unit juxtaposition, the way a charterparty or an invoice writes it. The syntax claim: a numeric literal followed by an identifier in term position is a quantity literal. No signed amounts (the surface has no signed decimal literals anywhere). |
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
   No intents. After the rollback, the refusal is recorded in the
   operational rejection log (see below).
7. If all pass: commit claims + audit record + outbox rows in one
   database transaction.
```

External side effects fire only after commit, delivered by workers reading the outbox.

**The rejection log is operational evidence, not part of the governed
claim state.** A refusal's transaction rolls back, so its record cannot
live inside the transaction that refused: the PG adapter writes one row
to `morpholog.rejections` in a separate autocommit insert AFTER the
rollback. That ordering fixes the record's epistemics - at-most-once (a
crash between rollback and insert loses it), never tamper-grade, never
consulted by any gate or invariant. The audit log remains the only
legitimacy-grade record. Each row carries who proposed what and which
rule refused (kind `invariant`/`require`/`bind` plus the rule name or
rendered gate expression, taken from the structured rejection reason,
never parsed back out of the display string). Recording is default-on
with no flag; a failed insert surfaces as an operational error, not a
rejected envelope. In-memory `propose()` records nothing (the pure
kernel does no I/O), and `morpholog explain` records nothing - a
dry-run diagnosis is not a refused proposal. A refused compensation
proposal records like any other, under the system actor. Consumers:
`inspect rejections` lists the rows; `inspect coverage` counts them
into the `constrained` verdict.

**A stored decimal carries a scale, and the scale is representation, not
value.** `round(x, 0.01)` fixes the VALUE to the penny; it does not
promise the figure is written with two decimal places. A runtime-computed
total may store as `378.3` where a supplied one stores as `378.30`, and
both satisfy the same recompute invariant because they are the same
number. Two consequences worth knowing: comparisons and arithmetic are
unaffected (they are numeric, not textual), but the audit leaf is derived
from bytes, so two numerically equal figures with different scales are
different leaves. Format money at the presentation edge rather than
echoing the wire string.

**A refusal names the offending values.** "invariant `x` violated" tells a
reader which rule stopped them and nothing about why, so an invariant
refusal also carries a *witness*: the variables and values that were live
where the rule failed, sorted by variable so one failure always reads the
same way. It is diagnosed only after the refusal is decided - the
accepting path never pays for it - and it is structured rather than
rendered into the reason, because the reason string is a pinned wire
format and an embedder should read a value, not parse prose. The `Display`
string is unchanged by its presence.

A witness names exactly what the rule binds. `ChargeLine(_, _, rate,
volume, net, _, _)` wildcards the line id, so its refusal reports the
figures but cannot say which row carried them; bind the subject and the
refusal can name it. The variables a rule binds but never uses are not
noise; they are what its refusals can say.

When several subjects violate the same rule, the witness reports the
first violation in state order, not every one - sorting the bindings
fixes how one assignment reads, not which assignment is chosen. The
PostgreSQL path therefore loads claims in primary-key order, so the same
database explains a refusal the same way twice; a hand-built `State` gets
whatever order it was built in. The key order rather than the causal one
because any total order gives determinism and this one the index already
provides - ordering by admission time forces a sort worth ~1.8x on propose
latency, which every accepted proposal would pay so that refusals
reproduce. Naming every violator, and
attributing an aggregate discrepancy (a sealed total disagreeing with its
lines cannot blame one line from its bindings alone), are separate
problems.

A witness is empty when the failure has no binding assignment to report,
which depends on what was bound where the drill-down stopped rather than
on which operator failed. A comparison failing under a quantifier or an
implication witnesses the variables its antecedent bound - that is the
metered-billing case above. The same comparison as an entire invariant
body witnesses nothing, because nothing was ever bound; a prohibition like
`not Flag(acct_1)` is the same. In that case the field is absent rather
than empty, so those envelopes stay byte-identical to what they were
before witnesses existed.

**A derived claim is a read model, and no rule may name one.** Derived
claims are computed from admitted claims and refreshed out of band; the
kernel evaluates rules against admitted claims, and nothing admits a
derived. So `bind`, `require`, `for`, an invariant body, and another
derived's domain are all refused at check time when they name one - and
so are `admit` and `retract`, which would give a single name two sources,
the view the runtime computes and the rows a transformation wrote - the
alternative is a design that passes `check --strict` and fails only
against a live database, which is exactly how a trial lost an hour.

The refusal is a modelling rule, not a claim that the reference could
never match. State outlives a source file, so rows admitted under that
name by an older shape of the programme may well exist - and that is the
problem rather than the exception: the name would have two sources, the
view the runtime computes and the rows left behind. Read the claims the
view is computed from, or make the figure a claim of its own.

A derived output cannot carry a claim discipline either. Disciplines
promise how governed state behaves - what may be retracted, which claims
agree, which pointer is current - and a read model replaced wholesale on
refresh honours none of them. The refusal lands on the declaration, where
the clause was written: `unique by` lowers to a generated invariant, so
refusing it there would name a rule nobody typed, and `append only`
lowers to nothing at all and would pass unnoticed.

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

The `require + let + value_of` chain remains expressible (`ValueExpr::ValueOf` is not deleted), and is the right tool when a value-producing position needs a lookup that does not fit a statement-level binding extension - inside arithmetic, inside `Sum`, or inside a derived-claim value expression.

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

## Definitions: named propositions

A `Definition` is a named, parameterised `Prop`, declared with `define
name(params): body` and called from invariant bodies, `require`/`bind`
gates, derived-claim domains, `Sum` bodies, and other definitions. It is
body grammar, not a third first-class construct: a definition never
changes state and carries no standing of its own - it gives a recurring
condition the name the business uses for it, so a validity window, a
lifecycle phase, or a statutory requirement is written once and read
everywhere by name. The call is spelled exactly like a claim reference;
the parser resolves it against the declared definitions once the whole
programme is collected, which is why a definition name may never
collide with a predicate name.

**Evaluation is relational substitution with projection.** A call
builds a frame carrying only the parameters: a ground argument (a
literal, `actor`, or a bound variable) pre-binds its parameter; an
unbound variable or wildcard leaves it free, so the body acts as a
generator for that position. The body evaluates under that frame alone
- it cannot see the caller's other bindings, and the caller never sees
the body's internal names. Each body match projects its parameter
values back onto the call's arguments, extending the caller's bindings
exactly as a claim match would. Projection deduplicates: a call yields
each distinct argument-binding witness once, so internal multiplicity
(two different internal witnesses for the same projected arguments) is
not observable and cannot double-count inside a `sum`. A call binds
its argument variables; binding flow at the call site follows the
claim-match rules.

**Parameters divide by what the body does with them.** A parameter the
body binds (it appears in a claim-match position) is generator-capable:
a call may pass an unbound variable there and receive the value. A
parameter the body only *uses* (a window date consumed by a comparator)
must arrive bound at every call; the static check enforces it with the
same unbound-name error the runtime would raise. A parameter the body
never references is refused outright - it could never be given a value
by the body, so it is either dead weight or a guaranteed runtime error.
Parameter names are duplicate-free: each is one binding slot in the
call frame.

**Definitions are proposition-valued only.** A call answers "does this
condition hold?" and hands values out solely by binding its arguments -
the trade lifecycle's `terms_in_force_on(trade, d, qty)` returns the
governing quantity *through* `qty`. `value Pred(args)` reads claims,
never definitions, and a definition can never be an `admit` / `retract`
/ `emit` target; naming one in any of those positions is a dedicated
category error, not an undeclared-predicate report.

**Bodies are context-free.** `actor` and `pre(...)` inside a definition
body are validation errors, so a definition means the same thing in a
gate as in an invariant. The actor is passed as an ordinary call
argument where a gate needs it (`investigator_delegated_on(actor, ...)`),
resolving against the proposing transition at the call site; a call
*wrapped* in `pre(...)` works, because the context swap applies to the
body's evaluation. Definitions may call definitions; cycles are a
validation error, and the nesting-depth guard charges each call its
callee's expanded depth, so a chain of shallow definitions cannot
smuggle an over-deep body past the budget.

**Diagnostics keep both levels.** A failing call renders as the named
condition first and the responsible body conjunct second (`inside
two_distinct_prior_verifications(...): MatchVerified(...)`), and the
explanation engine's missing-claims walk descends through the body
under the call's frame - a gate factored into named conditions reports
the same directly-missing claims its inline form would.

**Audit honesty.** Editing a definition changes the meaning of every
invariant that calls it without changing the invariant's own text, so
the audit row's `invariants_checked` list under-describes by itself.
The programme hash (`morpholog hash`) is what names the full rules in
force, definitions included; a definition edit is a rules change.

## Claim disciplines: declared properties of claim shapes

A discipline is a modelling commitment a predicate declaration carries
on its face, written as clauses after the argument list. Disciplines
are deliberately boring, deterministic, generated, visible, and few -
properties of claim shapes, never a back door for arbitrary rule
templates. The discipline clauses:

- **`unique by (fields)`** - the named fields determine the whole
  claim: any two claims agreeing on the keys agree on every field
  (full agreement, the SQL-UNIQUE reading; partial agreement is
  deliberately not offered). Several clauses may coexist on one
  predicate. Lowered to a generated invariant per clause.

  **It constrains the tuple you name, not the identities that tuple
  references.** `ReversalLine unique by (invoice_id, line_id)` reads like
  "one reversal per line" and means "one reversal per reversal": it says
  nothing about how many reversals point AT a given line. An embedder
  shipped that reading and credited a customer twice, permanently, under
  append-only. What bounds a claim's references to another claim is an
  invariant over both, and where it bites is a modelling decision: at
  admission, or - if corrections must stay retryable while a document is
  still a draft - only once the document is sealed.
- **`append only`** - no transformation may `retract` this predicate.
  Enforced statically: retraction only happens through a `retract`
  statement, so the authoring-time ban is complete and costs nothing
  at runtime. Ordinary programmes correct append-only claims by
  supersession or exception claims, never retraction; there is no
  escape hatch.
- **`current pointer by (fields)`** - this predicate is a retractable
  current-pointer (the doctrine's middle class, beside append-only
  content and append-only lineage). Lowers exactly a `unique by`
  invariant - the pointer singleton - and records the class as
  metadata. A pointer cannot also be `append only` (it must retract to
  move); the contradiction is refused.
- **`superseded via L`** - names the lineage predicate carrying this
  pointer's supersession history; only meaningful with, and required
  to accompany, `current pointer by`. `L` must have exactly two
  arguments in the `(successor, prior)` convention. The lowering
  generates **no-fork only** - `unique by` the prior (second) field,
  so one prior has at most one direct successor - and marks `L`
  append-only. It does NOT claim well-formed lineage: joins (two
  priors sharing a successor) and cycles are not prevented; a model
  needing those guarantees writes its own invariants.

**Lowering is materialised and runs at parse** (beside definition-call
resolution; hand-built IR runs `lower_disciplines` explicitly, and
validation fails loudly if a declared discipline's generated invariant
is absent). Generated invariants are ordinary [`Invariant`]s with
`origin: Discipline`, placed **before** the authored invariants - a
discipline is a precondition of sense for the other rules, so a
proposal violating both is refused with the root cause named. They are
enforced, scoped, audited, and rendered exactly like authored rules;
`guarantees` carries a `from` provenance ("predicate
CurrentOfficialPrice, current pointer by (trade)") so a generated name
in a rejection or audit row traces to its declaration in one hop. The
formatter renders the clauses and omits the generated invariants
(reparsing regenerates them deterministically, so round-trip holds);
the programme hash therefore covers the declarations and the
invariants they imply. Generated names are stable and boring -
`{snake(Predicate)}_unique_by_{fields}` - because they appear in
rejection reasons and `invariants_checked`.

A keyless global singleton ("exactly one MoveCount claim") is not
expressible as a discipline - `unique by` with every field as a key is
refused as vacuous (claims are a set; two identical claims are already
one claim), and no worked example has forced the keyless form.

## Invariants: state vs transition, and `pre(...)`

An invariant is by default a predicate over the candidate (post) state - the world after the proposed transformation has staged its assertions and retractions. That covers structural rules like "balanced posted entry" or "at most one piece per square." `Prop::Pre(inner)` opts a wrapped subtree into pre-transition evaluation, so a single invariant can relate pre and post values:

```
invariant move_count_strictly_increases:
  MoveCount(n) and pre(MoveCount(m)) implies n = m + 1
```

Invariants that contain `Pre` are *transition invariants*; the distinction is descriptive (derivable by walking the body) rather than an IR kind. Both run through the same evaluator.

`pre(...)` is only legal in evaluation contexts that have a pre-state in scope - invariant evaluation during a proposal does, since the kernel passes both pre and candidate states to the invariant check. It surfaces `EvalError::PreStateUnavailable` inside a transformation `require` (pre-state is already the only state), inside a derived-claim body, inside the inner subtree of nested `pre`, and in any `find_matches` call whose `EvalContext` was constructed with `pre_state: None`. The error is phrased about evaluation context rather than AST position, so any context that carries both states can share the primitive without IR change.

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

2. **Kind/type compatibility**: every value flowing into a slot must have a compatible kind. Declarations carry per-argument kinds (`Subject`, `Decimal`, `Date`, `Timestamp`, `Duration`, `Bool`, `Collection`, `Any`); comparators have fixed expected kinds per domain (`<=` Decimal, `on_or_before` Date, `at_or_before` Timestamp, `no_longer_than` Duration); `+`/`-` follow the time-arithmetic rule matrix (a pair of known kinds with no rule is `NoArithRule` at authoring time) while `*`/`/`/`%` stay Decimal-only; `sum` produces the kind it sums (Decimal or Duration, never mixed); equality (`=` / `!=`) is strict (`Subject == Decimal` is a kind error, not a silent coercion); variables are inferred-and-refined as they flow through claim slots, intent emits, comparators, and let-bindings.

3. **Binding flow**: a variable consumed where a bound value is required - an `admit`/`retract`/`emit` argument, a comparator or arithmetic operand, a `value` lookup key, a `sum` target - must have been bound first. The static walk follows the runtime exactly: parameters, `bind`, `let`, `for`, and claim matches in predicate position bind names; a `require` match does **not** export to later statements; a disjunction exports only the names bound in *every* branch (whichever branch's witness the runtime carries forward); `in` binds its element when it is otherwise unbound. A use of an unbound name is flagged as `UnboundVariable` - the same the kernel would raise.

4. **Shape**: enforced by the type system, not the checker. The two-sort IR (`Prop` searches state; `ValueExpr` computes a value) makes a value expression at a predicate position - or the reverse - unrepresentable, so neither the parser nor `ir_builder` can construct it. The former static shape check and the `NotPredicate` / `NotValue` kernel errors are gone; the evaluators are total over their sorts.

5. **Actor context**: `Term::Actor` referenced in an invariant or derived-claim body - where no proposing transition is in scope - is flagged (the kernel would raise `UnboundActor`). Authority checks belong in a `require`, not an invariant.

6. **Disciplines**: clause fields must exist on the predicate; a key set covering every field (or none) is vacuous; duplicate clauses are refused; `append only` + `current pointer by` is a contradiction; a `superseded via` lineage must be a declared two-argument non-pointer predicate and must accompany `current pointer by`; no transformation may `retract` an append-only predicate (declared, or lineage of a `superseded via`) - checked through nested `for` bodies; and every discipline that lowers must find its generated invariant present (hand-built IR that skipped `lower_disciplines` fails loudly).

7. **Definitions**: the reference graph must be acyclic (checked before anything walks through calls); a definition may not share a name with a predicate; `actor` and `pre(...)` inside a body are refused (bodies are context-free); every parameter must be referenced by the body; call arity must match; a call argument for a use-only parameter must arrive bound; and the depth guard charges each call its callee's expanded depth, computed callees-first. Parameter kinds are inferred from the body (callees-first) and call arguments check against them, so a kind mistake at a call site reports like any claim-argument mismatch.

The kind and binding-flow walks share the require/bind_one/let/for quartet's export rules over one scoped environment, so a `require` body's refinements and bindings stay local while `bind_one` and `let` flow forward; `Sum`'s body is walked under a scoped env so iteration-variable refinements stay local, and the value term must resolve to Decimal; `ValueOf`'s wildcard slot determines its result kind and its optional default must agree. `Any` is treated as *unconstrained*, not as "compatible with everything forever once attached to a variable": a variable seen first in an `Any` slot stays open, and a later specific use refines it. `Any` is an escape hatch for declarations, not a kind-eraser for inference.

The kernel IR carries no source spans - a `Program` can be hand-built or deserialised, and the kernel stays source-agnostic. Spans live on the surface side: the parser keeps every declaration's span (and every top-level transformation-body statement's span) in a `SourceMap` returned by `parse_program_with_sources`, and the map resolves a `ValidationError` or `Lint` back to source - context-carrying errors through their context, declaration-naming errors by name, anything unresolvable (a generated discipline invariant) to no span and a plain-text rendering. The one position the kernel does carry is an index, not a span: `ValidationContext::Transformation` records which top-level body statement a finding was made in (a finding inside a `for` keeps the `for`'s index), and the rendered message appends `, statement N`. Granularity is declaration + top-level statement; sub-expression spans are a later tier. Each error still names the predicate / operator / variable involved and its context, so a finding over hand-built IR remains locatable from grep alone.

`Program::validate` also bounds nesting depth: a body whose expressions or `for`-statements nest past a fixed limit is rejected (`NestingTooDeep`) before any recursive walk runs on it. The evaluator and the check itself descend one stack frame per level, so an unbounded body could exhaust the stack during `propose`. This is the teeth behind the rule that **untrusted IR must be validated before it is proposed**: `propose` trusts the IR it is handed and does no programme-level check of its own, so a deployment that accepts IR from outside must run `Program::validate` first.

`Program::validate` is **not** called automatically by `propose`. The kernel boundary is statement-level, not programme-level; revalidating on every proposal would muddle that distinction and add overhead. The `morpholog check` CLI subcommand runs it explicitly; tests over the worked examples do the same.

## Atomicity boundary

Steps 1-7 are atomic. Post-commit, outbox intents deliver at-least-once via workers running outside the transaction. External effects are never rolled back - only retried or compensated.

## Explicit non-goals for v0

The doctrinal floors - no entities/classes/services, no workflow engine, no arbitrary computation inside transformations, no BI engine, no bypass flags - are in [`scope-and-ambition.md`](scope-and-ambition.md)'s Non-goals and not repeated here. The IR- and runtime-specific ones:

- No invariant lifecycle. v0 has one canonical epoch; all invariants are `version: 1`, status `enforced`.
- No generated *storage* schema from claim shapes. v0 uses a small hand-written PG schema at `crates/morpholog-core/sql/schema.sql` for the runtime tables (claims, audit, outbox). Read-side typed *views* over predicates are generated (`generate views`); the storage DDL is not.
- No model checker; the decidable-core spec is a later artefact.
- No unit conversions, registries, aliases, or compound units. Units themselves are contractual labels on exact decimals (`Decimal[USD]`); everything relating one unit to another is domain knowledge that enters as admitted claims when an example forces it. No floating-point arithmetic; decimal only.

## Success criterion

For every worked example, both in memory (via `propose()` and the kernel test suite) and durably (via `propose_against_pg`):

1. Valid transformations commit, writing one audit row and one outbox row per emitted intent in a single SERIALIZABLE transaction.
2. Invariant-violating attempts roll back atomically - no claims changed, no audit row written, no outbox row enqueued.
3. Outbox intents stage at commit but do not fire inside the database transaction; an external worker (the polling `OutboxWorker` in `morpholog-outbox`, with `StdoutDeliverer` as the canonical concrete deliverer) is the only path that delivers them.

These hold for every worked example under [`examples/`](../examples/). In-memory proofs live in [`crates/morpholog-examples/tests/`](../crates/morpholog-examples/tests/); the durable, PostgreSQL-backed proofs live in [`crates/morpholog-postgres/tests/`](../crates/morpholog-postgres/tests/).
