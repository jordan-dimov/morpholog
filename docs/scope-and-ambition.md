# Morpholog: Scope and Ambition

Status: design doctrine. Companion to [`runtime-semantics.md`](runtime-semantics.md) and to [`forced-by-examples.md`](forced-by-examples.md), which records (retrospectively) which specific examples forced which design decisions.

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

### 1. Typed predicate declarations

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

The first cut of derived claims landed with Example 5 (trial balance over the double-entry ledger): `DerivedClaim { predicate, keys, values, domain }`, `enumerate_derived`, no materialisation, no recursion, not visible to invariants or transformations. Later questions - materialisation, invalidation, provenance, recursion through other derived claims, visibility to invariants - remain forced-by-examples territory; see `docs/forced-by-examples.md` for what Example 5 forced and what was explicitly deferred.

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

Authority, delegation, approval limits, and segregation-of-duties are then modelled as claims (`HasRole(actor, role)`, `ApprovalAuthorityFor(actor, amount_cap)`, `DelegatedBy(delegate, delegator, scope)`), and invariants over those claims. No RBAC subsystem. No middleware. Just the affordance to put "who proposed this" into the invariant fragment, and the discipline to express authority itself as governed claims.

The plumbing landed first (`Transition.actor`, `audit.actor`); the consultation primitive `Term::Actor` followed in the next PR, forced by Example 6 (actor authority). Inside a `require` or an `assert`, `$actor` resolves to the actor of the proposing transition. Inside an invariant body, it raises `EvalError::UnboundActor` - the require-vs-invariant doctrine made enforceable by the runtime rather than convention.

### What is not in this list

Not on the list, deliberately: workflow primitives, projection DSL beyond derived claims, query language beyond derived-claim queries, ORM, BI engine, scheduler, solver runtime, message broker. These are *outside the boundary*. If a real example forces one, that is a design event worth a separate doctrine doc, not an incremental feature.

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
- **Example 1 - Settlement netting.** Transactional correctness: arithmetic, exclusion, double-use prevention, atomic rollback.
- **Example 2 - Revenue restatement.** Contested legitimacy: history survives correction; current-standing pointer moves via retraction; supersession lineage persists; durable through `propose_against_pg`.

### Level 2 - Governed standing and governed derived claims

Standing first (the next semantic frontier already named in the README), then derived claims as the disciplined answer to read-side legitimacy.

Proven by:
- **Example 3 - Claim standing.** `AdmissibleFor(claim, purpose)` as the central pattern: which admitted claims may be used for which decisions, and how that standing is acquired and lost without mutating the claims themselves. (Landed; forced `Value::Subject` and the require-vs-invariant distinction.)
- **Example 4 - Double-entry ledger with period close.** Whether Xero-like accounting cores are a credible target. Posted-balance invariants, closed-period rejection, restatement-with-supersession for prior periods. (Landed; reused existing affordances without forcing new ones.)
- **Example 5 - Trial balance as a derived claim.** Whether read-side projections can be governed by the same model as admitted state. (Landed; forced `DerivedClaim`, `DerivedValue`, `Expr::Sub`, and `enumerate_derived`. Materialisation, provenance, and recursion through other derived claims remain deferred.)
- **As-of evaluation.** (Landed in subsequent PRs.) `reconstruct_state_at`, `list_claims_at`, `list_derived_at` reconstruct historical state by replaying the audit log up to a chosen `transition_id`. CLI exposes `--as-of <transition_id>` on `inspect claims` and `inspect derived`.

The remaining ETRM-shaped pressure (position / exposure as derived claims over a real trading book; actor authority and approval-limit invariants for regulated workflows) is the next forced territory: see `docs/forced-by-examples.md` for the running list of what each example forced.

Level 2 also drives the language affordances: typed predicate declarations remain candidate; derived claims are now committed in their non-recursive, non-materialised form. Actor identity is plumbed through the runtime (`Transition.actor`, `audit.actor`) and the consultation primitive (`Term::Actor`) is in the IR; Example 6 (actor authority) earned both. The next forced step in the authority arc - approval limits with decimal comparison, then predicate-pattern matching - awaits the worked example that demands them.

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
