# Morpholog: Scope and Ambition

Status: design doctrine. Companion to [`runtime-semantics.md`](runtime-semantics.md) and to [`design-history.md`](design-history.md), which records (retrospectively) which specific examples forced which design decisions.

This document fixes what Morpholog is *for*, what it should grow into, and - equally important - what it must never become. It is a defence against two opposite mistakes: under-claiming the value, and over-claiming the surface.

## The thesis

Morpholog governs **commit legitimacy**. The only way governed state changes is through a transformation; a transformation commits only if every active invariant holds against the candidate state. The product is not a programming language. It is a runtime where the question "may this state be admitted as legitimate?" is decidable by the language itself, not by code somebody remembered to write.

Everything else - UI, reporting, analytics, optimisation, market data ingestion, OCR, ML, scheduling, integrations - lives outside Morpholog and interacts with governed state only through transformations and reads.

## The constitution

The whole product:

1. The world is a set of admitted **claims**.
2. The only way claims change is through a **transformation**.
3. Every transformation must preserve every active **invariant**.
4. Therefore bad state cannot be committed.

A corollary worth stating outright, because much of the value now lives there: since admissibility is decided by the invariants and not by whatever produced the proposal, the proposer does not have to be trusted. A person, a solver, a heuristic, a model, a genetic search - each only suggests a candidate next state; the runtime admits or refuses it on the same terms. This is what lets a business put untrusted intelligence to work: admissibility is enforced outside the intelligence, not entrusted to it.

Those are the *constitutional* concepts: claims are the admitted state; invariants and transformations are the only first-class constructs over it - the rules and the actions. The other declarations a programme carries (`predicate`, `intent`, `derived`) are vocabulary and read-side, in the supporting tier below; none is a modelling primitive. Everything the implementation has grown is **supporting machinery**, and the discipline is to keep it subordinate so it never becomes the identity of the project:

- **Vocabulary** - `predicate` and `intent` declarations: the shapes claims and outbox effects may take.
- **Body grammar** - the expression syntax of invariant and transformation *bodies*. Supporting syntax, not a user-facing concept; it exists only so that a rule, or a computation inside one, has an honest shape.
- **Read side** - derived claims are named reads over admitted claims. They never mutate governed state; for a derived result to carry legitimacy, a transformation must admit it as an ordinary claim.
- **Execution account** - the trace and `explain`: a rendering of why a transformation was or was not admitted, never a separate reasoning engine.
- **Record and consequence** - the audit log (what was admitted, under which rules) and the outbox (post-commit effects a transformation emitted): a consequence log, never a workflow engine.

No supporting concept may grow into an independent subsystem - a workflow engine, a projection or query engine, a general query language, host functions, a solver runtime, an analytics layer. Those are the [non-goals](#non-goals); this hierarchy is *why* they are non-goals. The test for any proposed construct is one line: **does it serve claims, invariants, and transformations - or compete with them?**

## The boundary

The line is not "core vs application." It is **governed truth vs everything else**.

| Inside Morpholog (governed truth) | Outside Morpholog (computation, presentation, communication) |
|---|---|
| Claims (admitted assertions over subjects) | Market data feeds, sensor streams, OCR, ML categorisation |
| Invariants (rules that decide admissibility) | Pricing, curve building, MTM compute, simulation, solver output |
| Transformations (the only path state may change) | UI, dashboards, reports, BI, free-text search |
| Audit (which transformations admitted what, under which rule set) | Workflow engines, message brokers, schedulers |
| Outbox (intents to be delivered post-commit) | Integration code that actually sends invoices, REMIT reports, settlement instructions, emails |

The right question for every proposed feature is: *does this need to be governed, or merely computed?* If it needs to be governed, it becomes a claim and an invariant. If it is merely computed or merely presented, it lives outside and the result - if it carries any legitimacy weight - is admitted back in as a claim with provenance.

A useful sharpening of this line, especially around process-shaped concerns: **workflow orchestration is outside; the legitimacy of each workflow step is inside.** Morpholog does not schedule approvals, route tasks, or own user notifications - that is what message brokers, queues, and workflow engines exist for. But whether a given approval was admitted under the authority and conditions the rules require, whether a step may legitimately follow from the prior admitted state, and what the audit trail says afterwards - those are inside, and they are claims. This distinction protects the project from two opposite mistakes: building Camunda inside Morpholog, and letting workflow middleware bypass Morpholog's legitimacy check.

## Where compute lives

The boundary above says what belongs in Morpholog; the next question is how an external system integrates - where heavy computation runs, and how it returns to the governed model. The answer is a three-zone split.

| Zone | What runs there | Examples |
|---|---|---|
| **Compute** | Anything heavy, non-deterministic, or arbitrary. Outside the database transaction. | Monte Carlo simulations, OU price-process integration, ML inference, merit-order dispatch, optimisation solvers, web scraping, image processing. |
| **Commit** | Small, bounded, deterministic transformations that propose state changes. Inside the transaction; must satisfy every active invariant to become durable. | Admitting a settlement claim, recording a verified figure, posting a journal entry, registering a payment intent. |
| **Outbox** | Post-commit intents staged inside the same transaction, delivered at-least-once after commit. | Sending an invoice, posting a webhook, calling a downstream service, triggering a recompute. |

The doctrinal statement, in one sentence:

> **Morpholog does not call out during commit. It emits durable intents after commit. External compute returns through another governed transformation.**

That sentence determines the shape of every integration. There is no synchronous call-out from inside a transformation - it would break the atomicity guarantee and couple the kernel to whatever the called system was doing. There is no "Morpholog drives the simulation" - the simulation lives in the compute zone, in whatever language and framework suits it, and submits its result back as a candidate claim. There is no "external system polls and mutates" - state changes always come through `propose`.

**Input (compute -> commit).** External compute proposes a transformation by name, with an actor and typed arguments (a `Vec<EvalValue>`, the same codec across the CLI's `run` and the Rust `propose_against_pg`). The input is deliberately narrow: Morpholog owns whether to admit; the caller owns what to propose.

**Output (commit -> compute).** Intents a committed transformation emitted land in the outbox, delivered at-least-once post-commit - either by an in-process Rust `Deliverer` (the polling worker) or out-of-process through the `morpholog outbox` CLI's claim/complete lease protocol, whichever suits the deployment.

The pattern across the two - request transformation, outbox intent, external compute, result transformation - is the *round-trip compute pattern*, walked through concretely in [`outbox-sketch.md`](outbox-sketch.md).

## The expansion principle

> **Whatever you want to make legitimate, name it as a predicate and admit it as a claim. Whatever rules must hold, write as an invariant. Everything else lives outside.**

This single rule subsumes a large family of concepts that other systems implement as separate subsystems. In Morpholog, each is just more claims:

| Concept that looks like a feature elsewhere | What it is in Morpholog |
|---|---|
| Claim standing / admissibility-for-purpose | `AdmissibleFor(claim_subject, purpose)` - a claim |
| Read-side / projections | Derived claims computed from admitted claims |
| Lifecycle phase ("Confirmed," "Posted," "Settled") | A conjunction of admitted claims, optionally named as a derived claim |
| External-computation provenance | `MTMComputedFrom(mtm_id, curve_snapshot, model_v)` - claims |
| Integration acknowledgement / delivery | `SentTo(intent, counterparty, message_id)`, `AcknowledgedBy(intent, counterparty, ack_id)` - claims |
| Permissions / approvals / authority | `HasRole(actor, role)`, `ApprovalAuthorityFor(actor, threshold)` - claims |
| Temporal qualification (event / effective / known time) | `OccurredOn(subject, date)`, `EffectiveFor(subject, period)`, `KnownAsOf(subject, time)` - claims |

Each category is *located* in the core primitive - admitted claims governed by invariants - not solved by it: several still need carefully designed affordances and real runtime work (derived-claim materialisation, as-of strategies, indexing, invalidation, provenance). The discipline is that any new support serves the claim/invariant model rather than becoming a subsystem beside it. So a proposal that looks like a new subsystem - a workflow engine, an IAM module, a BI layer, a projection framework - is first re-examined as a proposal for a new *claim vocabulary* plus, where genuinely needed, a small claim-serving affordance. Most collapse along that axis.

## What the language actually needs

Four affordances unlock the categories above. Together they are far less than the surface area of the seven subsystems they replace.

### 1. Typed predicate declarations *(landed)*

```
predicate BankRecognisedRevenue(
    asset: Subject,
    period: Period,
    amount: Decimal,
    recognition: Subject
)
```

Pure introspection. Not types-over-subjects. Not classes. Just *shapes-of-predicates*: how many arguments, of what kinds, in what positions, with what names. The kernel continues to treat claims as opaque tuples; everything else (parser errors, indexing, derived-claim type-checking, read-side schemas, documentation generation) gets dramatically better.

This is the smallest possible step toward making claim vocabulary at scale manageable, without compromising the "no types over subjects" floor.

**Status:** landed. Every `Program` carries typed `PredicateDecl`s; `Program::validate()` enforces strict arity and kind/type compatibility, surfaced through `morpholog check`. Intent declarations (`IntentDecl`) followed, giving outbox effects the same typed, strictly-validated vocabulary.

### 2. Derived claims *(first cut landed)*

A named, read-only computation over admitted claims - a trial balance, a running total, a report row - declared alongside the rules and computed by the same kernel, so a read can never show what an invariant would refuse. This single construct subsumes most of what other systems call "projections" or "read models": phase/lifecycle naming, balances and totals, current pointers re-expressed as derivable, report rows. It is how Morpholog owns the read side without becoming a query engine - the read side is a governed artefact, not a free query surface.

**Status:** the first cut landed with the double-entry ledger's trial balance (`DerivedClaim { predicate, keys, values, domain }`, `enumerate_derived`, computed on demand). Materialisation, invalidation, recursion, and visibility to invariants are deferred; see [`design-history.md`](design-history.md).

### 3. As-of, as a single operator

The audit log already records every transition with a UUIDv7 transition id and timestamp. One operator - `state at T` (or `as of t`) - lets any invariant or derived claim be evaluated against the state that existed at any prior transition.

This single primitive collapses the four temporal notions that ETRM and accounting systems normally carry as separate fields:

- **Event time** - modelled as a claim `OccurredOn(subject, date)`.
- **Admission time** - already in the audit row (`committed_at`, `transition_id`).
- **Effective time** - modelled as a claim `EffectiveFor(subject, period)`.
- **Knowledge time** - the `as of T` operator, evaluated against the audit log.

No bitemporal schema is assumed at the modelling level. The v0 audit log already contains enough information to define as-of semantics by replay; performance may later require snapshots or materialised histories, but those are *implementation strategies* rather than *semantic primitives*. The semantics is "evaluate against the state that existed at T"; how to do that efficiently is a separate, contained question.

### 4. Actor context on transitions

The actor under whose authority a transition is being proposed is **transition context**, not a transformation parameter. Every `Transition` carries an `actor`; the audit log persists it; an invariant or a `require` can consult it through a reserved `Term::Actor` term without each transformation having to declare it. The shape becomes:

```
transformation approve_journal(journal):
    require HasRole($actor, finance_controller)
    require not Posted(journal)
    assert JournalApproved(journal, $actor)
```

Authority, delegation, approval limits, and segregation-of-duties are then modelled as claims (`HasRole(actor, role)`, `ApprovalAuthorityFor(actor, amount_cap)`, `DelegatedBy(delegate, delegator, scope)`), and invariants over those claims. No RBAC subsystem. No middleware. Two pieces working together: the affordance to consult "who proposed this" at *admission time* (`require` checks inside a transformation body, against the actor of the proposed transition), and the discipline to express authority itself as governed claims that invariants can constrain (consistency of the authority record, not its application to any specific proposal).

The plumbing landed first (`Transition.actor`, `audit.actor`); the consultation primitive `Term::Actor` followed, forced by the actor-authority worked example (now part of [`approval_controls`](../examples/04_approval_controls/)). Inside a `require` or an `assert`, `$actor` resolves to the actor of the proposing transition. Inside an invariant body, it raises `EvalError::UnboundActor` - the require-vs-invariant doctrine made enforceable by the runtime rather than convention.

### What is not in this list

Not on the list, deliberately: workflow primitives, projection DSL beyond derived claims, query language beyond derived-claim queries, ORM, BI engine, scheduler, solver runtime, message broker. These are *outside the boundary*. If a real example forces one, that is a design event worth a separate doctrine doc, not an incremental feature.

## Surface syntax and the IR

The `.morph` parser commits surface syntax to a deliberately narrower contract than the IR.

**The doctrine:** the parser can rename, rearrange, and add sugar; it cannot add semantic capability the kernel does not have. Surface syntax is more domain-native than the IR but never more expressive than it. Every legal `.morph` file must map to a legal `morpholog_core::Program`, and the surface offers no operator, no construct, and no escape hatch that the kernel cannot evaluate.

The cost of this discipline is that the parser does real translation work (infix to prefix, keyword to enum variant, bounded form to comprehension). The payoff is that the language stays *uncircumventable*. Surface affordances cannot accumulate into a parallel evaluator. The IR remains the single source of semantic truth.

**Verb-flavor renames (surface to IR):**

| Surface verb | IR construct | Reason |
|---|---|---|
| `admit X(args)` | `Stmt::Assert` | Matches the runtime doctrine of "admitted claims". `assert` belongs to test frameworks; `admit` belongs to governed state. |
| `bind X(args)` | `Stmt::BindOne` | The `_one` suffix is redundant - there is no `bind_many`. `bind` reads as the binding-statement it is. |
| `actor` (no parens) | `Term::Actor` | A special variable bound by transition context, not a function. Parens would suggest function-call semantics it does not have. |
| `<=` `<` `>=` `>` (infix) | `Prop::Compare { op, domain: Decimal }` | Business mathematics reads with infix comparators. The operator is first-class - `amount > limit` renders and round-trips as written, never as `not (amount <= limit)` - while the ordered domain is a field, not a per-operator variant. Decimal-only operands; the domain is carried explicitly, so there is no operator overloading by operand kind. |
| `on_or_before` `before` `on_or_after` `after` (infix) | `Prop::Compare { op, domain: Date }` | Distinct keywords (not overloaded `<=`) for civil-date comparison; `on_or_*` are inclusive, `before`/`after` strict. Reads as business prose and aligns with the `[from, to]` inclusive-window doctrine. `before`/`after` are matched contextually (comparator position only), so they remain usable as variable names. Operands are type-checked as `EvalValue::Date`; using them on decimals surfaces as a runtime `TypeMismatch`. |
| `=`, `!=` (infix) | `Prop::Eq` (ValueExpr, ValueExpr), `Prop::Neq` (ValueExpr, ValueExpr) | Both operate on full expressions and are symmetric: `a + 1 != b` is as legal as `a + 1 = b`. Not tied to one domain like the ordered comparators, but the two operands must share a kind - `=`/`!=` are kind-strict, with no silent coercion. |
| `+`, `-`, `*`, `/`, `%` (infix) | `ValueExpr::Arith { op, .. }` with `op: ArithOp::Add` / `Sub` / `Mul` / `Div` / `Mod` (decimal) | Standard arithmetic, decimal-only and exact; `*`/`/`/`%` bind tighter than `+`/`-`. The operator is a field, not a per-operator variant - the value-sort analogue of `Prop::Compare` carrying a `CompareOp`. Admission gates express ratio rules in multiplied form (`a <= c*b`, not `a/b <= c`) to stay exact; `/` is reserved for read-side projections, where a rounded figure is wanted; `%` is remainder, for parity and cyclic rules (`(file + rank) % 2`). A zero divisor on `/` or `%` surfaces `EvalError::DivisionByZero`. No unary minus until forced. |
| `min(a, b)`, `max(a, b)` | `ValueExpr::Arith { op, .. }` with `op: ArithOp::Min` / `Max` (decimal) | Floor and cap as self-delimiting functions, not infix - no extra precedence tier. Express layered limits, e.g. `min(limit, max(0, x))`. The `ArithOp::is_infix` predicate is what splits the printer (and the surface) between the infix operators above and these function-shaped ones. |
| `not`, `and`, `or`, `xor`, `implies` (keywords) | `Prop::Not`, `Prop::And`, `Prop::Or`, `Prop::Xor`, `Prop::Implies` | Boolean composition reads as keywords in business rules, not symbols. `and` flattens into `Prop::And(Vec<Prop>)` and `or` into `Prop::Or(Vec<Prop>)`; `implies` is right-associative. `xor` is exactly-one: it adds no expressiveness (it is `(a or b) and not (a and b)`, evaluated by lowering to exactly that), but reads far better than that hand-written form where the operands are long claim patterns. Binary, not flattened (n-ary xor is ambiguous); it sits between `and` and `or` in precedence. |
| `forall x in coll: body`, `exists x: body` | `Prop::Forall`, `Prop::Exists` | Bounded quantification is mathematical convention. The `in` clause on `forall` makes unbounded quantification syntactically impossible. `exists` carries no source clause because the IR's `Prop::Exists` doesn't model one - the bound variable is whatever the body matches. |
| `sum(target | body)` | `ValueExpr::Sum` | Set-builder notation. The target is a variable to sum, or a decimal literal - `sum(1 | body)` counts the matches (the chess material census forced this). A general expression target awaits an example that needs it. |
| `value Pred(args)` (with optional `default expr`) | `ValueExpr::ValueOf` | Claim-pattern form. The wildcard `_` in `args` marks the value position to extract. The kernel's `ValueOf { predicate, args, default }` is shaped this way deliberately; a `value(target | body)` shape would imply a general query and be more expressive than the IR. |
| `x in xs` (membership) | `Prop::In(Term, Term)` | Infix at comparator precedence. Distinct from the structural `in` in `forall x in xs: body`; disambiguated positionally (the structural `in` comes immediately after the binder in `forall`). |
| `@2026-05-22` | `Value::Date("2026-05-22")` | `@` sigil avoids the lexer ambiguity between bare ISO-8601 dates and arithmetic (e.g. `2026 - 05 - 22`). |
| `#NAME` | `Value::Subject("NAME")` | `#` sigil makes subject literals visibly distinct from variables and reflects that subjects are opaque symbolic identifiers, not strings. |

**What this rules out:**

- A surface form that maps to no IR construct (would be a fictitious operator).
- A surface form that adds an interpretation the kernel does not have (e.g., date arithmetic like `@2026-05-22 + 30d` - the kernel compares dates but has no interval arithmetic, so the form stays out of the surface until a worked example forces the kernel to grow it).
- A surface escape hatch like `unsafe_block { ... }` or `evaluate_in_rust(...)`.

**What this leaves room for:**

- Different layouts (block-style `invariant X:\n  body` vs inline `invariant X: body`) that map to the same IR.
- Macros / convenience forms that expand to existing IR (e.g., `between(x, lo, hi)` could desugar to `lo <= x and x <= hi`).
- Renaming, reordering, and disambiguation rules in the surface that have no IR cost.

The doctrine governs every surface addition. A reviewer should reject any surface addition that lacks an IR mapping or that smuggles in an interpretation the kernel cannot evaluate.

**Block syntax: indentation, not braces.** Multi-statement blocks (transformation bodies, `for` loops, future statement-bearing constructs) use indentation, not braces or `end` keywords. This matches the colon-terminated forms at the expression level (`forall x in xs: body`, `exists x: body`, `invariant Name: body`) and keeps `.morph` source reading as rules, not config JSON or imperative scaffolding. The layout mechanism is an implementation choice; the doctrine is *indentation*.

## The right way to measure ambition

The wrong question: *what percentage of the code is Morpholog?* The right question: *what percentage of the legitimacy-bearing failure modes does Morpholog make non-representable?*

Real ETRM and accounting systems suffer the same small family of failures, regardless of vendor or scale:

- **Reconciliation drift** - truth and reports diverge.
- **Mutable history** - yesterday's beliefs cannot be reproduced.
- **Stale standing** - a draft is used as if posted; a superseded value is treated as current.
- **Authority leaks** - an operation is performed without the approval the rules require.
- **Lost compute provenance** - which curve produced which MTM, under which model version?
- **Non-reproducible reports** - a regulatory return cannot be re-derived from its inputs.
- **External effects under stale state** - an invoice was sent, a settlement instructed, a report filed, against state that has since changed.

These are not seven different failures. They are one failure with seven faces: *state was treated as legitimate that the system cannot, under explicit rules, justify treating as legitimate.*

Morpholog's claim is that this whole class of failure becomes non-representable when the language itself is the legitimacy boundary. Measured this way, Morpholog owns **the majority of the legitimacy surface** of a serious business system while still being a minority of the lines of code. That is the ambition, and it is the right frame; "X% of the codebase" is the wrong one.

## Where Morpholog fits: fragile legitimacy, not hard calculation

That failure family points at the kind of domain where Morpholog earns its place - a wider category than "regulated finance". The recurring shape is **official standing**: the moment something becomes *allowed, recognised, paid, certified, released, listed, funded, or treated as compliant*. Morpholog is strongest wherever the expensive failure is not a bad calculation but a bad standing - a state treated as legitimate that no one can, afterwards, justify. Put another way, the category is **systems where being wrong is not merely incorrect, but illegitimate**.

A domain fits to the degree it has five properties, not by how complex its arithmetic is:

1. A bad official state is expensive - legally, financially, or in safety terms.
2. The rules are cross-cutting and scattered today (email, PDFs, spreadsheets, separate systems).
3. Evidence matters as much as calculation.
4. Audit and explanation carry monetary or legal weight.
5. There are external effects after commit (a payment, a certificate, an instruction).

Finance, KYC, insurance, clinical trials, and energy settlement score high - but so do trade finance, supply-chain custody, environmental-attribute claims (carbon credits, certificates of origin: "no green claim without admissible provenance"), healthcare eligibility, data-access governance, and the admission of AI-agent actions ("the agent may propose; only legitimate actions become official"). The diagnostic questions are always the same - *why was this allowed, who had the authority, what evidence existed at the time, what obligation did it create, what downstream effect did it emit, and what would have blocked it?* - which is the same observation as: **the legitimacy engine and the explanation engine are one bet, and it has begun paying out.** `morpholog explain` already turns a rejected proposal into a missing-evidence checklist, naming each absent claim and the transformation that would supply it; `morpholog inspect guarantees` names what a model makes impossible - both read straight off the IR that decides admissibility, not a second system bolted alongside. A domain becomes compelling once Morpholog can answer those questions about it, and on the carbon-credit flagship it does. The [roadmap](roadmap.md) carries the rest of the legibility set.

The boundary discipline holds across every one of these: Morpholog governs the *admission of the official-standing claim* - the permit is issued, the action approved, the credit retired - never the physical or computational act behind it (the switching hardware, the AI agent's reasoning and drift, the meter). Those stay outside and return as admitted evidence. It is the discipline that keeps "official state is everywhere" from eroding the inside/outside line.

## Roadmap

Three levels. Each one is proven by a worked example before any language affordance is locked in.

### Level 1 - Governed writes (today)

Transformations and invariants over admitted claims. One PostgreSQL `SERIALIZABLE` transaction per proposal. Audit and outbox written atomically with the claim mutations.

Proven by:
- **[Settlement netting](../examples/01_settlement_netting/).** Transactional correctness: arithmetic, exclusion, double-use prevention, atomic rollback. The foundational example: invariants check the *candidate state*, not just the pre-state.

### Level 2 - Governed standing, restatement, read-side projection, and authority

The richer worked examples, each combining several of the patterns the language needs. The flagship is **[verified revenue](../examples/02_verified_revenue/)**: contested legitimacy in two woven patterns - *currentness with restatement* (the verifier corrects a figure; the original stays admitted; a singleton pointer moves; lineage records the change) and *admissibility-for-purpose* (different authorities grant standing for the same figure and can revoke it without touching the underlying claim; historical decisions survive a correction). The others span double-entry accounting, approval authority, cumulative settlement, date-window validity, transition invariants, KYC screening, and carbon-credit provenance - the flagship the explanation engine points at. Each lives under [`examples/`](../examples/); [`design-history.md`](design-history.md) records which kernel primitive each forced and why.

After this level a Morpholog programme is **a declared vocabulary of admissible claim shapes plus transformations and invariants over that vocabulary**, with structurally inspectable execution and as-of replay over the audit log. The candidate affordances it drove - predicate and intent declarations, kind/type checking, as-of - are landed; the next forced moves (higher-order authority, effective time as a separate axis, materialised derived claims) await the examples that demand them, per [`roadmap.md`](roadmap.md).

### Level 3 - Governed external and integration provenance

The same primitive (claims), now reaching to the system's edges. External-computation results admitted as provenance claims; outbox intents acquire delivery/acknowledgement claims; actor authority extends to delegated and external actors.

No example specified yet, and none should be: planning Level 3 in detail before Level 2 stabilises would violate the smallest-increment discipline.

## Non-goals

These are floors. They do not get relaxed by accumulation of pressure; they get relaxed only by explicit revisit with reasons recorded.

- **No entities, classes, services, or ORM** in the surface language. Subjects are opaque identifiers; predicates attach to subjects; that is the entire object model.
- **No general workflow engine.** Lifecycle is conjunctions of admitted claims (and, eventually, derived claims). Morpholog is not Camunda and must not grow toward it.
- **No arbitrary computation inside transformations.** Pure expressions over admitted claims, plus assertions, retractions, intents. External computation lives outside; its results may be admitted back as claims with provenance.
- **No BI / analytics / reporting engine.** Derived claims govern reproducible read-side outputs; everything else is a separate concern with a separate tool.
- **No optimisation / solver runtime.** ETRM scheduling, AP payment runs, dispatch - outside. Morpholog governs the inputs and admits the outputs.
- **No ad-hoc query DSL** beyond derived-claim queries and the as-of operator.
- **No bypass flags** (`skip_validation`, `force_commit`, etc.). Exceptions, when needed, are first-class typed claims with full audit standing.
- **No bespoke storage kernel.** Morpholog runs on PostgreSQL 17+ and leans deliberately on `SERIALIZABLE` isolation, JSONB, and atomic multi-table commit. The recurring "if the language is truly minimal, compile it down to a standalone microkernel" inverts the thesis: atomic commit under serializable concurrency, crash recovery, and MVCC are the hardest correctness code in computing, and reimplementing them would move millions of lines of un-battle-tested machinery *inside* the trust boundary for an elegance the product does not need. The minimalism that matters is **surface** (the `.morph` language and IR stay small), not **substrate**. The database is the correctness substrate, chosen on purpose.

## Where this leaves us

Morpholog does not aspire to be the language you write the whole business system in. That would be a trap and a category error. It aspires to be the **governed-truth layer that makes the rest of the business system safe to build**: a runtime where the legitimacy boundary is the language, the failure modes that haunt serious business software become non-representable, and the perennial question - *what did we believe, when, under which rules, and how do we know the data obeyed them?* - has an answer by construction.

Measured in lines of code, that is a minority of any real system. Measured in legitimacy-surface coverage, it can be most of what matters.
