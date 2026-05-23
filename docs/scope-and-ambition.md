# Morpholog: Scope and Ambition

Status: design doctrine. Companion to [`runtime-semantics.md`](runtime-semantics.md) and to [`design-history.md`](design-history.md), which records (retrospectively) which specific examples forced which design decisions.

This document fixes what Morpholog is *for*, what it should grow into, and - equally important - what it must never become. It is a defence against two opposite mistakes: under-claiming the value, and over-claiming the surface.

## The thesis

Morpholog governs **commit legitimacy**. The only way governed state changes is through a transformation; a transformation commits only if every active invariant holds against the candidate state. The product is not a programming language. It is a runtime where the question "may this state be admitted as legitimate?" is decidable by the language itself, not by code somebody remembered to write.

Everything else - UI, reporting, analytics, optimisation, market data ingestion, OCR, ML, scheduling, integrations - lives outside Morpholog and interacts with governed state only through transformations and reads.

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

This is a **modelling discovery, not a licence to add seven subsystems.** Morpholog already has the core primitive - admitted claims governed by invariants - and the categories above are *ontologically located* in that primitive, not *implementationally solved* by it. Several of them will still require carefully designed language affordances and real runtime work (derived-claim materialisation, as-of evaluation strategies, indexing, invalidation, provenance bookkeeping). The discipline is that any new support must serve the claim/invariant model rather than becoming an independent subsystem alongside it.

The corollary: feature proposals that introduce a new *subsystem* (a workflow engine, an IAM module, a BI layer, a projection framework as a separate abstraction) should be re-examined first as proposals to introduce a new *claim vocabulary* and, where genuinely necessary, a small, claim-serving runtime affordance. Most of them collapse along that axis.

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

**Status:** landed. Every `Program` carries a `Vec<PredicateDecl>` with argument names and kinds (`Subject`, `Decimal`, `Date`, `Bool`, `Collection`, `Any`). `Program::validate()` enforces strict arity: every claim/assert/retract/value_of/derived-claim reference must target a declared predicate. The CLI exposes the declarations via `morpholog inspect predicates <program>`. Kind validation against the values flowing through the binding context is recorded but not yet enforced; it's the highest-leverage layer of the enriched-`morpholog check` work on [`roadmap.md`](roadmap.md).

### 2. Derived claims

A first-class declaration of the form:

```
derived TrialBalance(entity, period, account, balance)
    where balance == sum { x | JournalLine(_, entity, period, account, debit, credit), x = debit - credit }
    governed by [balanced_per_account, entry_signs_consistent]
```

A derived claim is true iff its defining expression holds over admitted claims. It can be **materialised** (written to a table for fast reads, with a provenance link to the inputs that produced it) or computed on demand. Invariants may quantify over derived claims as if they were primary.

This single construct subsumes:
- phase / lifecycle naming (a derived claim that says "all the parts of Confirmed hold"),
- balances and totals (sum-derived claims),
- current pointers when re-expressed as derivable from history,
- report rows,
- most of what other systems call "projections" or "read models."

Derived claims are the answer to "how does Morpholog own the read side without becoming a query engine?" - by making the read side a governed artefact rather than a free query surface.

The first cut of derived claims landed with Example 5 (trial balance over the double-entry ledger): `DerivedClaim { predicate, keys, values, domain }`, `enumerate_derived`, no materialisation, no recursion, not visible to invariants or transformations. Later questions - materialisation, invalidation, provenance, recursion through other derived claims, visibility to invariants - remain design-history territory; see `docs/design-history.md` for what Example 5 forced and what was explicitly deferred.

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
| `<=` (infix) | `Expr::Le` (decimal) | Business mathematics reads with infix comparators. Decimal-only; the kernel keeps `Expr::Le` and `Expr::DateLe` as separate IR variants (no operator overloading - see `design-history.md`). |
| `on_or_before` (infix) | `Expr::DateLe` (civil date) | Distinct keyword (not overloaded `<=`) for civil-date `<=`. Reads as business prose, aligns with the `[from, to]` inclusive-window doctrine. Runtime type-checks the operands as `EvalValue::Date`; using it on decimals surfaces as a runtime `TypeMismatch`. |
| `=`, `!=` (infix) | `Expr::Eq` (Expr, Expr), `Expr::Neq` (Term, Term) | `Eq` operates on full expressions; `Neq` operates on terms only (the IR shape). The parser rejects arithmetic on either side of `!=`. |
| `+`, `-` (infix) | `Expr::Add`, `Expr::Sub` (decimal) | Standard arithmetic notation, decimal-only. No unary minus until forced. |
| `not`, `and`, `implies` (keywords) | `Expr::Not`, `Expr::And`, `Expr::Implies` | Boolean composition reads as keywords in business rules, not symbols. `and` flattens into `Expr::And(Vec<Expr>)`; `implies` is right-associative. |
| `forall x in coll: body`, `exists x: body` | `Expr::Forall`, `Expr::Exists` | Bounded quantification is mathematical convention. The `in` clause on `forall` makes unbounded quantification syntactically impossible. `exists` carries no source clause because the IR's `Expr::Exists` doesn't model one - the bound variable is whatever the body matches. |
| `sum(target | body)` | `Expr::Sum` | Set-builder notation. Target restricted to a variable in v0; relax when a worked example forces it. |
| `value Pred(args)` (with optional `default expr`) | `Expr::ValueOf` | Claim-pattern form. The wildcard `_` in `args` marks the value position to extract. The kernel's `ValueOf { predicate, args, default }` is shaped this way deliberately; a `value(target | body)` shape would imply a general query and be more expressive than the IR. |
| `x in xs` (membership) | `Expr::In(Term, Term)` | Infix at comparator precedence. Distinct from the structural `in` in `forall x in xs: body`; disambiguated positionally (the structural `in` comes immediately after the binder in `forall`). |
| `@2026-05-22` | `Value::Date("2026-05-22")` | `@` sigil avoids the lexer ambiguity between bare ISO-8601 dates and arithmetic (e.g. `2026 - 05 - 22`). |
| `#NAME` | `Value::Subject("NAME")` | `#` sigil makes subject literals visibly distinct from variables and reflects that subjects are opaque symbolic identifiers, not strings. |

**What this rules out:**

- A surface form that maps to no IR construct (would be a fictitious operator).
- A surface form that adds an interpretation the kernel does not have (e.g., `<` for strict decimal comparison - the kernel only has `Le`, so `<` is not in the surface until the kernel grows `Lt`).
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

Morpholog's claim is that this whole class of failure becomes non-representable when the language itself is the legitimacy boundary. Measured this way, Morpholog can plausibly own **the majority of the legitimacy surface** of a serious business system while still being a minority of the lines of code. That is the ambition. "X% of the codebase" is the wrong frame.

## Roadmap

Three levels. Each one is proven by a worked example before any language affordance is locked in.

### Level 1 - Governed writes (today)

Transformations and invariants over admitted claims. One PostgreSQL `SERIALIZABLE` transaction per proposal. Audit and outbox written atomically with the claim mutations.

Proven by:
- **[Settlement netting](../examples/01_settlement_netting/).** Transactional correctness: arithmetic, exclusion, double-use prevention, atomic rollback. The foundational example: invariants check the *candidate state*, not just the pre-state.

### Level 2 - Governed standing, restatement, read-side projection, and authority

The richer worked examples. Each one combines several of the patterns the language needs:

- **[Verified revenue](../examples/02_verified_revenue/).** The flagship. Contested legitimacy in two patterns woven together: *currentness with restatement* (the verifier corrects a figure; the original stays in admitted state; a singleton pointer moves; lineage records the change) and *admissibility-for-purpose* (different authorities grant standing for the same figure for their own decisions; standing can be revoked without touching the underlying claim). When the verifier corrects, standings on the prior verification are retracted by pattern, but historical decisions survive. Forced `Value::Subject` and crystallised the require-vs-invariant distinction.

- **[Double-entry ledger](../examples/03_double_entry_ledger/).** Whether Xero-like accounting cores are a credible target. Posted-balance invariants, closed-period rejection, restatement-with-supersession for prior periods, and `TrialBalanceRow` as the read-side projection. Hosts the trial-balance derived claim that forced `DerivedClaim`, `DerivedValue`, `Expr::Sub`, and `enumerate_derived` into the IR.

- **[Approval controls](../examples/04_approval_controls/).** Actor identity threading through transitions and into the audit log, with both unconditional authority (`MayApprove`) and quantitative authority via `Expr::Le` (`ApprovalLimit`). Forced `Term::Actor` and `Expr::Le`.

- **[Insurance claim settlement](../examples/05_insurance_claim_settlement/).** Settlements consumed against a policy aggregate limit. Two coexisting rules, answering different questions: the admission gate `Le(Add(Sum(paid), proposed), aggregate_limit)` ("is there enough capacity?") and the conservation invariant `pre(PolicyHeadroom(p, before)) and PolicyHeadroom(p, after) implies after = before - sum(new payments)` ("did the payment actually consume the capacity?"). Forced `Expr::Add` and provides the canonical business use of `Expr::Pre`.

- **[Clinical trial enrolment](../examples/06_clinical_trial_enrolment/).** Participant randomisation is admissible only if the protocol version, consent form, investigator delegation, and eligibility assessment are all valid on the randomisation date. Forced `Value::Date`, `EvalValue::Date`, and `Expr::DateLe` - civil-date ordering, inclusive `[from, to]` window semantics. The first non-finance worked example; pins the doctrine that *closing a window must not retroactively invalidate decisions admitted under it*. Time-of-day, time zones, durations, business calendars, gas day and settlement-period semantics are deliberately deferred to future forcing examples.

- **[Chess transition invariants](../examples/07_chess_transition_invariants/).** A non-business teaching example that exercises transition invariants - rules over how state *changes*, not just over what state is admissible. Textbook chess invariants (the move counter advances by one, turn alternates, at most one capture per move) all need `pre(...)` to compare pre- and post-states; none can be expressed as a predicate over a single state. Forced `Expr::Or` (the capture rule admits a 0-or-1 disjunction) and the kernel mechanism that powers the insurance example's `headroom_consumed_by_payment` invariant.

- **As-of evaluation.** `reconstruct_state_at`, `list_claims_at`, `list_derived_at` reconstruct historical state by replaying the audit log up to a chosen `transition_id`. CLI exposes `--as-of <transition_id>` on `inspect claims` and `inspect derived`. The trial-balance example demonstrates this end-to-end.

**The authoring-surface refactor arc.** After the worked examples crystallised the kernel's primitive set, a refactor arc made the IR pleasant to author against: a public `dsl` module with builder helpers; `Stmt::BindOne` as a deterministic unique-lookup binding statement (collapsing the older `require + let + value_of` workaround); `Program::predicates` with strict arity validation; structured per-statement diagnostic trace via `propose_with_trace` (with `failing_sub_expression` identifying which sub-expression of a rejected `require` or `bind_one` is responsible); predicate-scoped loading on both read and write paths of the PG adapter; the kernel split into focused submodules. After this arc a Morpholog programme is **a declared vocabulary of admissible claim shapes plus transformations and invariants over that vocabulary**, with execution that's structurally inspectable. The kernel discipline stayed the same; the authoring layer above it became materially more usable. See [`docs/design-history.md`](design-history.md) for the per-PR retrospective.

The candidate language affordances driven by Level 2: predicate declarations are now landed. The next forced moves - higher-order authority (one authority claim governing a family of transformations), effective time as a separate axis, validity windows, materialised derived claims - all await the worked examples that demand them. The current operational plan for them lives in [`docs/roadmap.md`](roadmap.md).

### Level 3 - Governed external and integration provenance

The same primitive (claims), now reaching to the system's edges. External-computation results admitted as provenance claims; outbox intents acquire delivery/acknowledgement claims; actor authority extends to delegated and external actors.

No example specified yet. This level should not be planned in detail until Level 2 has stabilised; pre-deciding it would violate the "smallest possible increment" discipline.

## Non-goals

These are floors. They do not get relaxed by accumulation of pressure; they get relaxed only by explicit revisit with reasons recorded.

- **No entities, classes, services, or ORM** in the surface language. Subjects are opaque identifiers; predicates attach to subjects; that is the entire object model.
- **No general workflow engine.** Lifecycle is conjunctions of admitted claims (and, eventually, derived claims). Morpholog is not Camunda and must not grow toward it.
- **No arbitrary computation inside transformations.** Pure expressions over admitted claims, plus assertions, retractions, intents. External computation lives outside; its results may be admitted back as claims with provenance.
- **No BI / analytics / reporting engine.** Derived claims govern reproducible read-side outputs; everything else is a separate concern with a separate tool.
- **No optimisation / solver runtime.** ETRM scheduling, AP payment runs, dispatch - outside. Morpholog governs the inputs and admits the outputs.
- **No ad-hoc query DSL** beyond derived-claim queries and the as-of operator.
- **No bypass flags** (`skip_validation`, `force_commit`, etc.). Exceptions, when needed, are first-class typed claims with full audit standing.

## Where this leaves us

Morpholog does not aspire to be the language you write the whole business system in. That would be a trap and a category error. It aspires to be the **governed-truth layer that makes the rest of the business system safe to build**: a runtime where the legitimacy boundary is the language, the failure modes that haunt serious business software become non-representable, and the perennial question - *what did we believe, when, under which rules, and how do we know the data obeyed them?* - has an answer by construction.

Measured in lines of code, that is a minority of any real system. Measured in legitimacy-surface coverage, it can be most of what matters.
