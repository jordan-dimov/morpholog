# Morpholog: Scope and Ambition

Companion to [runtime semantics](runtime-semantics.md), which defines what the kernel means; [the computational model](computational-model.md), which states its guarantees; [design history](design-history.md), which records why decisions were taken; and [the roadmap](roadmap.md), which says what comes next.

This is the project's **product constitution**, not a feature inventory or a release plan. It should outlast implementations, worked examples and commercial experiments. Its job is to keep two ambitions compatible: **make consequential business systems dramatically easier to build and change**, and **keep the authority that admits business state small enough to understand and trust**.

## The ambition

**Build, operate and change substantial business systems with far less application-specific code, duplication and verification work, without surrendering control over consequential decisions.**

Business applications repeat the same obligations in endpoints, services, mutations, batch jobs, reports, tests and migrations. As AI agents make writing those layers cheaper, the cost of *generating* code falls faster than the cost of determining whether the generated system is right. The scarce assets become the business model, the authority behind it, the evidence for decisions and the ability to change it safely.

Morpholog's wager is that much of this complexity can be replaced by **one explicit, executable description of what the business recognises, permits and forbids**. Applications and agents may build around that description. They do not get to implement an alternative truth behind it.

This is an ambition to test, not a claim that an ERP or ETRM has already been replaced. A substantial governed application still requires conventional software. The test is whether Morpholog removes more work and uncertainty than adopting, integrating and operating it adds.

**Bigger than a validation library, smaller than a general-purpose application platform.** Morpholog may be the domain foundation of a whole new system, rather than only a guard attached to an old one. It need not own the UI, external calculations or scheduling to own much of what makes a system *correct as a business system*.

## Proposal is not authority

A person, application, optimiser or AI agent proposes a change. An approved Morpholog programme, deployed behind an enforced boundary, decides whether that change can enter the admitted record. The proposer can be replaced, rewritten or untrusted without changing the rules of admission.

That is why agents matter. They can build interfaces, adapt integrations, discover candidate models, generate tests and propose operations. They cannot legitimately substitute weaker rules, write through a second door, invent business authority or declare their own code safe. These are independently enforced boundaries, not matters of confidence in the agent.

Do not conflate three different changes:

1. **A proposal under approved rules.** A pinned, protected runtime can independently admit or refuse it, conditional on the real deployment and trusted inputs.
2. **A change to surrounding application code.** Independent admission may eliminate duplicated policy checks in that code. It cannot prove the rest of the application correct or eliminate operational and security risks.
3. **A change to the governing programme.** This changes what is legitimate. Review, approval and activation require separate authority. A rulebook does not approve its own promotion.

The first is a conditional runtime capability. The second is an important, unproven engineering and commercial hypothesis. The third is a governance obligation, with end-to-end protected promotion still to be demonstrated; see [#627](https://github.com/jordan-dimov/morpholog/issues/627).

## The constitution

Within a governed domain:

1. The admitted state is a set of **claims**: assertions this system has accepted, not objective facts about the world.
2. **Transformations** are the authorised way to add or retract governed claims.
3. **Invariants** constrain the proposed state. Admission also depends on the transformation's gates and well-formed values. A commit cannot introduce or leave a violation in a case it touches; an inherited violation in an untouched case may remain.
4. An admitted change and its audit record commit atomically. A refusal makes no governed state change. The operational rejection log is not a complete census of proposals.
5. The decision is finite, deterministic and pure relative to the programme, admitted state, proposal and explicit decision context. Time, identities and external observations enter through specified boundaries, never hidden reads from the kernel.

The precise meaning of cases, snapshots, errors, refusals and replay belongs in [runtime semantics](runtime-semantics.md), not here. In particular, “every invariant holds everywhere at all times” is not a guarantee Morpholog makes.

The algebra is deliberately small:

> **Programme + admitted state + proposal + explicit context → decision.**
>
> **Committed decision → new admitted state + attributable record.**

Claims are the governed state; invariants and transformations are the constitutional constructs. Predicate and intent declarations, definitions, derived claims, source syntax and execution machinery serve them. They do not create independent centres of authority.

Compilation, indexes, bounded reads, snapshots, transports, sessions, generated views and clients can change the *cost* of deciding and using that model. They must not change its *meaning*.

## A governed application, not merely a gate

If the Morpholog record is the domain's authoritative state, many familiar layers of business software can become generated, derived or unnecessary:

- **Writes.** Named transformations replace independently maintained mutation paths and their scattered business-policy checks. Callers supply intended choices and external observations, not facts already determined by admitted state.
- **Reads.** Declared predicates, definitions and derived claims provide a common source for typed read contracts, SQL views and histories. A derived result is not a second writable truth.
- **Integration.** Generated clients, schemas and service interfaces let ordinary software use the same programme. An outbox records post-commit intents without invoking arbitrary services during admission.
- **Change.** Checks, explanation, evaluation, rehearsal and audit evidence reduce the work of discovering how rules and recorded decisions relate.

None of this implies that an ORM, API generator, editor, agent assistant or deployment console belongs in the kernel or surface language. These can be worthwhile **product surfaces** if they remove repeated work and derive from one authority instead of becoming authorities themselves.

The goal is not to maximise the percentage of a codebase written in Morpholog. It is to minimise **independently maintained implementations of business meaning**. A short rulebook with ordinary integration may be better than a giant rulebook, and simpler than a smaller but fragmented application. Measure responsibility, not syntax.

The highest-leverage improvement often removes a question entirely: **derive, do not ask; check, do not trust**. Derive what authoritative state already determines, ask callers only for genuine choices, observations and evidence, and represent permitted transitions rather than relying solely on final-state totals. Eliminate work before making the remaining work faster.

## One programme, one decision boundary

A domain whose families share governed state and invariants needs one authoritative programme. Arbitrarily deploying separate programmes over the same writable claims is not benign modularity: each programme can govern only its own acts, so another writer may escape an invariant.

The unit of trust is the **fully assembled programme and its semantic identity**, not a filename, folder or package. Authoring can be divided into explicitly listed parts for parallel human and agent work without introducing imports, independently activated modules or competing rule authorities. Packaging must not change the programme hash or decision. A different programme is appropriate where state ownership and authority genuinely separate, not merely because the source grows long. See [#785](https://github.com/jordan-dimov/morpholog/issues/785).

An auditor must be able to answer **which exact rules governed this decision?** The model identity is recorded. **Who approved those rules and which deployed artefact enforced them?** is a separate question requiring source, approval and deployment evidence; a hash alone does not answer it.

## The boundary of a real system

| Responsibility | Where it belongs |
|---|---|
| Admitted claims, transformations, invariants and decisions | Morpholog programme and runtime |
| Atomic governed state, audit, durable intents and evidence | Morpholog on its transactional substrate |
| Typed views, generated contracts and derived reads | Supporting tools, without independent write authority |
| Pricing, forecasting, optimisation, OCR, models and external observations | External compute, with results proposed back under explicit provenance |
| UI, HTTP routing, task scheduling, notifications and process orchestration | Conventional application and integration software |
| Payments, reports, messages and outside actions | Custodied executors after commit, with acknowledgement/reconciliation where needed |
| Rulebook approval and production activation | A separately protected governance and operational process |

The real line is **governed authority versus everything that computes, presents or acts around it**. Workflow orchestration is outside; whether a consequential workflow step is legitimate is inside.

An operation spanning those boundaries needs **one honest atomicity story**. If an existing application and Morpholog commit separate halves of one logical stock or accounting operation, the result is not equivalent to a single transaction. Either move the whole governed operation under one owner, demonstrate a sound shared-transaction design, or keep that operation outside the integration. A two-commit gap is not an acceptable shortcut. This is an especially important feasibility gate for brownfield adoption.

The round trip is straightforward: external computation proposes a transformation; Morpholog decides and, on commit, records governed state and outgoing intents together; an executor performs outside effects, with retries and possible duplicate delivery; results or acknowledgements return as new governed evidence when the business relies on them.

There are no arbitrary synchronous host calls during admission. External data is not fetched invisibly into the kernel, where a later change to that data could alter the meaning of a past decision.

## Trust has more than one meaning

A correct decision under written rules is **not** proof that the rules are right for the organisation. Keep separate:

- **Logical admission.** Does a proposed state meet the rules? Morpholog answers within its precise semantics; static checks and historical evaluations answer narrower related questions.
- **Business intention.** Are those the *right* rules, for the intended purpose, under the right authority? A passing check and a model hash cannot establish this.
- **Input standing.** Is a reported observation authentic, current enough and acceptable for this purpose? Morpholog can govern standing claims; it cannot authenticate the external world merely by receiving them.
- **Enforcement.** Is the approved programme the one actually running, with no bypass, protected credentials and controlled external actions? This depends on deployment as well as software.
- **History.** What does a record or proof establish, and what does it leave unknown? Replay and tamper-evident evidence have stated boundaries.

Never sell one layer of assurance as a substitute for another.

## From rules to reasons

Rules are not sacred because they are executable. A rule may come from obsolete policy, a historical workaround, a regulator's requirement, a particular customer's contract or an unexamined assumption. It may contradict a later business intention while remaining internally consistent.

**An explainable decision is not yet an explainable rulebook.** “Rule X refused this shipment” answers what prevented it. “Rule X exists because of authority Y, to protect purpose Z, under assumptions A, and was approved by B” answers why the rule itself has standing.

The useful direction is a traceable chain:

> **Purpose or obligation → authoritative source and approval → rule → governed cases → decisions and evidence.**

Purposes can form hierarchies and can conflict. Profitability, safety, legal compliance, contractual commitments and operational speed are not one universal objective that a kernel can maximise. The relevant human authorities must define priorities and resolve trade-offs. Agents may detect inconsistency, trace provenance, compare consequences and suggest revisions. They must not invent the organisation's priorities from the code.

Start pragmatically: a rule's rationale, source, owner, effective context, assumptions, exceptions and historical impact may be held in versioned documentation or a separate governed approval record. New language semantics for intentions, policy hierarchies or conflict resolution require specific examples and explicit proof obligations, not a speculative universal ontology.

This is **a strategic direction, not a shipped claim**. Current explanation and rehearsal tell us valuable things about decisions and recorded history, not whether a business ought to have wanted each rule.

## AI-assisted development: the larger opportunity

A high-ambition workflow has agents help turn a domain description, policies and example records into a *candidate* programme, construct the surrounding adapters, APIs, views and UI, test realistic operations, and refine the implementation against an approved programme. Reviewers then concentrate on authoritative rules, integration boundaries and unresolved assumptions rather than several copies of each business rule.

But there are separate gates:

1. **Candidate model.** Check it, run adversarial examples, compare behaviour and rehearse history. None of this proves requirements complete or rules socially or commercially correct.
2. **Candidate application.** Independently test its actual behaviour, including non-governed code paths, deployment and failure handling. A pinned programme cannot fix a broken transport or a false external fact.
3. **Candidate policy change.** Obtain distinct, protected approval for the exact model and its activation. Agents can propose a rulebook; they cannot self-certify it.
4. **Production proposals and effects.** Admit through the active model, keep external capabilities outside the proposer, and reconcile consequences where needed.

In that world, much of the application *code* can become cheap to replace. Business judgement, source authenticity, deployment security and accountability cannot.

## Where to prove the thesis

Morpholog is most promising when the expensive error is not simply a wrong calculation but a state treated as **officially legitimate when it should not have been**: a trade accepted, stock allocated, an order released, a payment posted, a certificate issued, a limit exceeded or a regulatory assertion relied upon.

Three complementary experiments matter:

- **Greenfield domain core.** Build a coherent capability around one governed model, with generated or simple host code. Test whether a real backend becomes smaller and easier to evolve.
- **Brownfield bounded replacement.** Replace one atomic operation family in an established system only where state ownership, transactionality and no-bypass conditions can be proved. Count the cost of moving the boundary.
- **AI-assisted change.** Make the same realistic business-rule changes in a strong conventional implementation and in the governed version. Use independent withheld acceptance tests and record reviewer corrections, defects, elapsed effort and operational overhead.

An ambitious rewrite of an ERP or exchange is something to de-risk family by family, not proof of product-market fit. A stopped integration is useful evidence if it exposes obligations the product cannot yet remove.

## How success is measured

For every capability, name the recurring obligation it makes unnecessary for a caller, author, reviewer or operator. Then count every new obligation it introduces.

Evidence worth collecting:

- Fewer independently maintained policy implementations and redundant caller-supplied facts.
- More consequential operations governed without bypass, split-transaction semantics or duplicated authoritative state.
- Less time and independent reviewer effort to a **correct, accepted business change**, compared with strong conventional software.
- Measured authoring, migration, integration, deployment and operating costs, including contention and retries.
- Better ability to answer what was decided, under which rules and evidence, how a correction changed the record, and what a candidate rulebook would have done to recorded proposals.

Do not equate fewer lines, faster model output, a green test suite or an attractive agent demonstration with reduced lifetime cost. **Rehearsal re-decides historical proposals over recorded prefixes; it does not prove future correctness or reproduce every original concurrent snapshot.** Audit evidence establishes only the properties the evidence format and trust assumptions support.

The economic question is simple:

> Does the upfront cost of the governed model and enforced boundary pay back through fewer independent obligations, safer changes and simpler operation?

If not, govern a smaller domain or leave it in ordinary software. Morpholog is not owed a place in every application.

## Deliberate limits

**The kernel stays small, finite, deterministic and pure.** It is an admission engine, not a concealed general-purpose application runtime. A new IR or kernel primitive needs a worked example existing constructs cannot express cleanly. Supporting tooling has a different bar: it must remove real repeated work without inventing another authority.

- **No arbitrary host computation in transformations.** Heavy, impure or nondeterministic work runs externally and returns through governed proposals.
- **No entity, class or ORM model in the surface language.** Subjects are identifiers; predicates express admitted claims. Generated clients and adapters are welcome as subordinate tools.
- **No workflow scheduler, solver, BI suite or message broker inside the kernel.** Legitimate transitions and reproducible reads are governed; process infrastructure is external.
- **No unrestricted dynamic imports or independent rule fragments.** Programme assembly is explicit, inspectable and semantically one programme.
- **No bypass switch.** A real exception is an explicit, authorised, auditable business fact, not a request to skip admission checks.
- **No invented physical or institutional truth.** Observations, authenticated identities, legal sources and purposes need responsible external authorities.
- **No bespoke storage kernel.** PostgreSQL supplies atomic commit, isolation, recovery and concurrency control. Reimplementing it would add enormous correctness risk for no product gain.
- **No universal verification promise.** Finite deterministic decisions and audit evidence have precise meanings; completeness of requirements and fidelity to intentions are separate questions.

These are not bans on optional *products around* Morpholog. Source-aware tooling, a generated API, an agent authoring assistant or a deployment console may be valuable so long as they derive from the same authority. Conversely, “it can live outside the kernel” is not sufficient reason to build it.

## Rules for the project itself

1. **Remove obligations before optimising operations.** Ask what callers no longer need to supply, developers no longer need to write, reviewers no longer need to verify and operators no longer need to reconcile.
2. **Keep one authority for governed meaning.** Every generated surface or fast path must preserve the admission semantics.
3. **Derive instead of duplicating, check instead of trusting.** Refuse unsupported assertions and illegitimate transitions rather than silently repairing them.
4. **Protect and measure the guarantees.** Compiled paths, indexes and replay optimisations need semantic parity, not their own definitions of correctness.
5. **State the remaining trust assumptions.** An accepted proposal does not prove right intentions, true inputs, authorised promotion or successful external effects.
6. **Make new primitives earn their existence.** Use a concrete forcing example; prefer the smallest affordance and remove one-off workarounds.
7. **Let real embedders falsify the product thesis.** Test end-to-end operations, concurrent writers, changes and deployment against competent alternatives. Treat negative results as results.
8. **Claim no more than can be witnessed.** Distinguish shipped guarantees, deployment-dependent properties, observed experiments and future ambition.

## Where this leaves us

Morpholog should make **business meaning the durable, independently enforced part of a system** and make much of the surrounding software cheaper to construct, regenerate and replace. It should answer not only “what happened?” but “why was this allowed, under which rules and evidence?”, while making it increasingly practical to connect the rules back to the purposes and authorities that justified them.

The ambition is not to write every line of an application in a new language. It is to make fewer lines, and fewer independent human and machine obligations, necessary in the first place.

**The rules stay small enough to trust. The systems built around them need not stay small.**
