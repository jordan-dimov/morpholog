# What the worked examples forced into the language

Status: design archaeology. Companion to [`scope-and-ambition.md`](scope-and-ambition.md) and [`runtime-semantics.md`](runtime-semantics.md). This doc records the design moves the runtime made because a worked example forced them, in the order in which each move happened.

**Reorganisation note.** The worked examples were originally numbered in the chronological order in which each IR primitive was forced. They were later consolidated by *business programme*, producing the current four-example layout:

- `revenue_restatement` (old Example 2) + `claim_standing` (old Example 3) -> `verified_revenue` (new Example 2)
- `double_entry_ledger` (old Example 4) + `trial_balance` (old Example 5, hosted on the ledger programme) -> `double_entry_ledger` (new Example 3)
- `actor_authority` (old Example 6) + `approval_limits` (old Example 7) -> `approval_controls` (new Example 4)

The retrospective entries below still refer to the *original* example names, because they record what was forced at the moment it was forced. The current example taxonomy is in the [README](../README.md); this doc is the design journal that produced the current taxonomy, not a map to it.

## Why this doc exists

The project's pacing discipline - *smallest possible increment that produces a working artefact* - means design decisions are deferred until concrete pressure forces them. The corresponding risk: when pressure does come, the response gets rationalized after-the-fact as "what we always meant." This doc tries to keep the actual record honest. For each significant decision, it answers three questions:

- **What forced this?** Which example, and what was the concrete pressure.
- **What did we consider and reject?** The credible alternatives, and why we did not pick them.
- **What does this mean for future examples?** The pattern the decision sets and where it is likely to apply or break next.

The doc covers each worked example in chronological order. It is extended each time a new example crystallizes a decision the forward-looking doctrine docs hadn't yet pinned.

## Decisions forced by the examples

### `Value::Subject` IR variant

**Forced by:** Example 3 - claim standing.

**The pressure:** purposes (`bank_debt_service`, `investor_reporting`) needed to appear as constant references in invariant bodies and transformation `require` clauses. The pre-Example-3 IR had `Value::Decimal(String)` only; constant subjects had no representation.

**The alternative we considered and rejected:** pass purpose as a transformation parameter the caller already knows. Functional, but ceremonial *and bug-prone* - the caller could pass the wrong purpose to a purpose-specific transformation, e.g. `subj("investor_reporting")` to `admit_debt_service_revenue`, and the transformation would silently admit a `DebtServiceRevenue` claim against the wrong standing.

**The change:** added `Value::Subject(String)` to the `Value` enum; matched in `unify_args` (against `EvalValue::Subject`) and `resolve_term` (returning `EvalValue::Subject`). Two unit tests pin the contract. ~30 lines of kernel change; one new IR variant. Smallest possible change that closes the gap.

**Pattern for future examples:** if a transformation needs to embed a specific constant subject in its body (a status name, a fixed authority, an enumerated category), it can now be expressed as `Term::Literal(Value::Subject(...))` rather than as a caller-supplied parameter. If a constant of a *different* kind shows up (e.g. a boolean literal, a fixed datetime), the same pattern applies - add the variant when forced, not before.

### The `require`-vs-`invariant` semantic distinction

**Forced by:** Example 3 - claim standing.

**The pressure:** the natural-sounding rule *"a `DebtServiceRevenue` claim implies an active `AdmissibleFor`"* turned out to be an invariant trap. If written as an invariant, revoking standing later would either:
- reject the revoke (breaking the rule that standing can be lost), or
- force cascade-retraction of every historical decision that relied on it (breaking the rule that history is preserved).

Neither matched the real-world semantics we wanted: *a calculation made under valid standing at time T stays valid even if standing is revoked at T + 1.*

**The resolution:** `require` is the *admission gate* - checked at the moment of admission, and only then. `invariant` is the *eternal rule* - must always hold against admitted state. The two are not interchangeable; they answer different questions. Example 3 uses `require AdmissibleFor(...)` on decision transformations and reserves invariants for the standing claims' own internal consistency (`admissibility_has_provenance`, `admissibility_excludes_revocation`).

**Pattern for future examples:** any rule that looks like *"every X claim must satisfy Y"* must be classified before being written:

| Question | Which to use |
|---|---|
| *Must this hold at the moment X is admitted, then never re-checked?* | `require` |
| *Must this hold against state, always, including state X is part of?* | `invariant` |

If a proposed invariant would force cascade-retraction of historical claims when supporting state changes, it is probably a require in disguise. The two-line difference in IR makes the semantic difference much bigger.

### Currentness and standing as distinct semantic constructs

**Forced by:** Examples 2 and 3 together.

**Example 2 asks:** *which claim is in force now?* (currentness)
**Example 3 asks:** *which claim may be relied on for this purpose?* (standing)

**The pressure to keep them separate:** Example 2's `CurrentBankRecognition(asset, period, recognition_id)` is parameterized only by (asset, period); at any moment there is at most one current recognition per asset-period. Example 3's `AdmissibleFor(claim_id, purpose)` is parameterized by purpose; multiple parallel admissibilities can coexist on the same claim. Collapsing them into one primitive would lose this distinction: you cannot model "the same verification is in force for the bank but not for investor reporting" with a single current-pointer.

**The pattern:** both use the same lower-level mechanic (a separate claim that confers a property on another claim, retractable independently of that other claim). The verifier's correction in Example 2 retracts a `CurrentBankRecognition`; the authority's revocation in Example 3 retracts an `AdmissibleFor`. In both, the underlying append-only claim is never touched.

**Pattern for future examples:** when modelling *"which X is canonical for purpose Y at time Z?"* the first question is whether currentness alone is enough, or whether purpose-specific standing is needed. Examples 4 and 5 both confirmed this - period-closed accounting state has currentness *and* admissibility-for-purpose questions (current journal entry vs admissible-for-statutory-report).

### History-as-append-only

**Forced by:** Examples 2 and 3.

**The pattern:**

| Pattern | Predicates that fit |
|---|---|
| Append-only history | `BankRecognisedRevenue`, `IndependentlyVerifiedRevenue`, `StandingGrantedBy`, `DebtServiceRevenue`, `InvestorReportedRevenue` |
| Retractable pointers / admissibility | `CurrentBankRecognition`, `AdmissibleFor` |
| Append-only superseding / revocation | `Supersedes(new, old)`, `StandingRevoked` |

The discipline: *content* claims (what was admitted) are append-only; *pointer* and *admissibility* claims are retractable; *lineage* and *revocation* claims are append-only again.

**Pattern for future examples:** in any new example, classify each predicate into one of these three buckets before writing transformations. Only the middle bucket may be retracted. This is a useful design check - if a transformation wants to retract something in bucket one or three, the model probably has the wrong shape.

### Generic standing on any subject (Example 3's deferred typed-predicate question)

**Forced by:** Example 3 - claim standing.

**The behaviour:** `grant_standing` will admit `AdmissibleFor(any_subject, any_purpose)` even if the named subject has no underlying claim of any predicate. The decision transformation later catches "no underlying IV" via its own `require IndependentlyVerifiedRevenue(...)`.

**Why this is intentional in v0:** the runtime has no predicate type system that would let `grant_standing` enforce *"the supplied subject names a verification claim, not something else."* `AdmissibleFor` is a generic relation by design - the same shape applies to verifications, journal entries, curve snapshots, audit artefacts, valuation reports, and other claim kinds we may add later.

**Future trigger:** a later example that finds this looseness too loose will force the introduction of typed predicate declarations - declaring that a predicate's *n*th argument is a subject identifying a specific claim kind. Until then, generic standing is the honest position. `scope-and-ambition.md` already lists *typed predicate declarations* as a candidate language affordance for this reason.

## What deliberately stayed minimal

Each entry below was actively considered during one of the early examples and deferred. They are listed here so the reasoning survives the conversation.

| Considered | Why deferred |
|---|---|
| `ReliedOnBy(decision_id, claim_id)` decision-snapshot pattern | Example 3's simpler `require`-based design proved sufficient; the snapshot pattern adds a third "claims-about-claims-about-claims" layer that would only earn its keep when the simple model breaks. |
| `RevocationLifted` claim or grant-supersession to allow re-grant | Example 3 makes revocation terminal in v0. Re-granting after revocation would need a clear lifecycle model; deferred until a real example forces it. |
| Temporal qualification claims (`OccurredOn`, `EffectiveFor`, `KnownAsOf`) | Mentioned in `scope-and-ambition.md` as candidates for the as-of operator. No example yet needs them. |
| Strict comparison (`<`, `>`, `>=`) | `Le` landed with Example 7. `Lt`/`Gt`/`Ge` await an example that needs strictness (e.g. "must be strictly under cap"). |
| Predicate-pattern matching (`ApprovalAuthorityFor(actor, predicate_pattern, limit)`) | Mentioned in `scope-and-ambition.md`. Would let one authority claim govern a family of transformations. Forced by an example that genuinely needs the higher-order shape; not by Example 6 or 7. |
| Cumulative or time-bounded limits ("up to N total per day") | Would force `Sum` over a time-bounded subset of admitted state, plus a way to look up the audit log from inside a require. Out of scope for Example 7. |
| Cascading retraction of historical decisions on standing revocation | Considered as option B for Example 3's design; rejected because it contradicts the "history is preserved" rule. |
| Sharing IR fixture helpers across example modules | The `morpholog_examples::*` modules share an internal `helpers` constructor module for IR brevity, but each example module declares its own predicates, transformations, and invariants - they happen to reuse predicate names (e.g. `Supersedes` in Examples 2 and 3) without sharing constructor code. Keeps examples independent. |
| Per-example PostgreSQL schemas | All examples share `crates/morpholog-core/sql/schema.sql`. The schema is canonical runtime infrastructure (`claims`, `audit`, `outbox`); examples differ in which predicates they admit into those tables, not in their storage shape. |

## Example 4: double-entry ledger reused everything, forced nothing

Example 4 (posted-balance invariants, closed-period admission gating, restatement-with-supersession) landed without forcing any new IR primitive. The accumulated affordances from Examples 1-3 were sufficient. Specifically:

- **Balance is `Eq(Sum, Sum)`.** The accounting equation `sum { d | JournalLine(entry, _, d, _) } == sum { c | JournalLine(entry, _, _, c) }` composes directly with existing IR variants. Pinned by `unbalanced_entry_rejected_by_invariant`.
- **Period close is admission-gating via `require not PeriodClosed(period)`.** No invariant ties `JournalEntry` to `PeriodClosed`, so closing a period does not invalidate historical entries - the same require-vs-invariant lesson as Example 2.
- **Restatement reuses `Supersedes`** from Example 2 with no shape changes.
- **The append-only / retractable / append-only three-bucket classification holds**: every predicate is content, terminal state, or lineage. No retractable pointers were needed.

The clean reuse outcome is itself informative: the accumulated affordances from Examples 1-3 are sufficient to express a textbook accounting workflow. The next semantic frontier - derived claims for read-side projections - was what Example 5 pushed on. See the next section.

### `DerivedClaim`, `DerivedValue`, and `Expr::Sub`

**Forced by:** Example 5 - derived claims (trial balance over the double-entry ledger).

**The pressure:** the Example 4 prediction above named derived claims as the next frontier. Concretely, "what is the balance of every account?" had no expression in the v0 runtime. A caller could iterate `state.claims` in plain Rust and compute the trial balance by hand, but that logic lived outside the governed model. There was no way to say "trial balance is part of the ledger program and these are the rules that define it" alongside the invariants and transformations.

**The design conversation:** PR #19 added a design sketch and a spike test that pinned the target API before any kernel code. The sketch raised eight open design questions; review (recorded in PR #19's polish commit) narrowed the four most consequential ones:

- The naive `DerivedClaim { predicate, parameters, body }` shape was rejected. It conflated *enumerated* keys (`account`) with *computed* values (`balance`), leaving the evaluator without a principled way to tell them apart.
- The revised shape splits the two explicitly: `DerivedClaim { predicate, keys, values, domain }` with `DerivedValue { name, expr }` for each computed value, and a separate `domain` expression that enumerates distinct key bindings.
- For subtraction, lean was `Expr::Sub` rather than extending `Sum`'s value position into an expression sublanguage. More general (arithmetic outside aggregation), smaller semantic step.
- v0 derived claims should *not* be visible to invariants or transformations, not added to `State.claims`, not persisted, not exposed via CLI, not recursive, and not as-of. Each was a deferred design question listed explicitly.

**What landed in implementation:**

- `Expr::Sub(Box<Expr>, Box<Expr>)` for decimal subtraction. Both operands evaluate as decimals; the result is `EvalValue::Decimal(a - b)`. Non-decimal operands surface as `EvalError::TypeMismatch`. At the time, deliberately the only arithmetic primitive: no addition, multiplication, or division until a real example forces them. (Addition arrived later when the insurance-claim-settlement example forced `Expr::Add`; see the corresponding entry below.)
- `DerivedClaim { predicate, keys, values, domain }` and `DerivedValue { name, expr }` exactly as the revised sketch proposed. `Program` gains a `derived_claims: Vec<DerivedClaim>` field; existing example programs declare it as `vec![]`.
- `enumerate_derived(derived, state) -> Result<Vec<ClaimInstance>, EvalError>` reuses the existing `find_matches` to enumerate raw bindings from the domain, then deduplicates by key tuple via a `BTreeSet<EvalValueOrd>` (a private newtype that gives `EvalValue` a JSON-based ordering for stable iteration). For each unique key tuple, each `DerivedValue.expr` is evaluated via `eval_value` and the results are appended to the key tuple as the output `ClaimInstance.args`.
- `double_entry_ledger::trial_balance_row()` is the worked example. Keys: `["account"]`. Values: one `balance = Sub(Sum(debits), Sum(credits))`. Domain: `JournalLine(_, account, _, _)`.

**What was confirmed about the design conversation:** every prediction from the sketch held. No new helper beyond `find_matches` was needed for binding enumeration. The keys/values/domain shape was structurally sound and the evaluator's algorithm was the small mechanical one the sketch described. The v0 boundaries (not in State, not visible to invariants/transformations) are pinned by a dedicated test (`derived_claims_do_not_pollute_admitted_state`).

**Implication for future examples:** derived claims are now a tool in the kit. When a future example asks "what is the X for each Y?" the answer is a `DerivedClaim` with `Y` as the key and `X` as the computed value. Recursion (one derived claim's body referencing another), interchangeability with admitted state (so invariants can quantify over derived claims), materialisation (caching results in a table), and provenance (tracking which admitted claims contributed to each row) all remain explicitly deferred. The first of those that some example genuinely forces is the right next move; speculatively adding any of them now would expand the surface beyond what the existing customer needs.

**Pattern note:** Example 5 is the first time the sketch-then-implement two-PR pattern was used (rather than the design-pin pattern of the postgres-persistence-v0 doc). The pattern worked: PR #19's open questions reduced design ambiguity enough that PR #20's implementation was almost mechanical, with no late-stage surprises and no rework. Worth reaching for again when the next genuinely-IR-expanding example arrives.

**Read-side surfacing (follow-on PR):** The PR after PR #20 added the minimum operational surface for derived claims: `list_derived(pool, &DerivedClaim) -> Vec<ClaimInstance>` on the PostgreSQL adapter and `morpholog inspect derived <program> <name>` on the CLI. Both are thin wrappers that load current claims via the existing read path, hand off to the synchronous `enumerate_derived` kernel primitive, and return JSON. No materialised storage, no PG-side projection, no recursion through derived claims. Worth noting because the work was *boring*: the keys/values/domain shape and the sync-kernel `enumerate_derived` boundary that the sketch chose held up without modification under the async I/O wrapper. A signal that the design conversation paid for itself in implementation simplicity twice over.

### As-of evaluation (audit-log replay)

**Forced by:** the realisation that derived claims answer "what can we compute from current admitted state?" but a regulated user needs the analogous question against historical state. An auditor at quarter-end does not care what the trial balance is now; they care what it was at the close of Q1, *before* the late restatement. Today's `list_derived` cannot answer that. The only escape was to write bespoke SQL outside Morpholog, which directly undermines the read-side legitimacy promise that derived claims started to deliver.

**The design conversation:** PR #26 added a design sketch and a spike test that hand-rolled audit-log replay in user code. The sketch raised eight open design questions; review (Copilot + ChatGPT independently) flagged that the original wording of question #6 - "an unknown id larger than every committed one returns current state" - was a wrong-answer cliff. A mistyped id that coincidentally ordered past the latest transition would silently return current state instead of failing loudly. The polish commit on PR #26 settled the contract: every unknown id is an error.

The other sketch leans that held into implementation:

- **No kernel change.** `enumerate_derived(&State)` already takes the right shape. As-of is a question of which `State` you hand to the kernel; the adapter does temporal reconstruction and the kernel stays time-agnostic.
- **Ordering by `(committed_at, transition_id)`**, not by UUIDv7 byte comparison. The implementation first looks up the target audit row by `transition_id`, then replays rows where `(committed_at, transition_id) <= (target.committed_at, target.transition_id)`. Avoids leaning on UUIDv7 byte-comparison for the semantic cut-off; stays correct under concurrent commits where wall-clock and UUID-generation order can disagree.
- **Two functions, not one.** Public `reconstruct_state_at` returns the *full* historical state. Internal `reconstruct_state_at_for_predicates` returns a *partial* state filtered by a predicate set, used by `list_derived_at` to mirror PR #25's predicate-scoped loading. A single function with an optional `predicates` parameter would make the contract slippery; two functions, two clear contracts.

**What landed in implementation:**

- `PgError::TransitionNotFound(Uuid)` - new variant for the "as-of coordinate does not exist" failure mode.
- `reconstruct_state_at(pool, transition_id) -> Result<State, PgError>` on `morpholog-postgres`. Public, full historical state.
- `reconstruct_state_at_for_predicates(pool, transition_id, predicates) -> Result<State, PgError>` - `pub(crate)`, partial state filtered during the replay loop (not after).
- `list_claims_at(pool, transition_id)` and `list_derived_at(pool, derived, transition_id)` - thin wrappers, the as-of analogues of `list_claims` and `list_derived`.
- Eight integration tests covering: spike scenario via production helper, latest-tid equivalence with current, unknown-id error, scoped reconstruction correctness under noise, and the partial-state contract.

**What was confirmed about the design conversation:** the keys lean held - no kernel change required, two-function shape was the right call, the `(committed_at, transition_id)` two-column comparison was straightforward to implement (PostgreSQL supports row comparison natively). The spike test was retired; its headline scenario became test #1 of the production integration suite. The single ambiguity from the sketch (question #6) was resolved by review *before* implementation, which is the value the sketch-then-implement pattern is supposed to capture.

**Implication for future examples:** as-of is now reachable. The next natural pressure points (most have since landed; see the entries below for the actual retrospectives):

- A bench scenario that surfaces the replay cost at long-audit-log scale. **Landed in PR #28**, which also surfaced an O(N^2) pathology in `reconstruct_inner` that the next PR fixed. See the `ReplaySet` entry below.
- A CLI flag `--as-of <transition_id>` on the inspect subcommands. **Landed in PR #28**.
- A worked example that combines as-of with effective-time claims (`EffectiveFor(subject, period)`), which is how the four temporal axes - event time, admission time, effective time, knowledge time - become accessible without polluting any schema with bitemporal flags. **Still open** as of the latest entry below.
- Write-path as-of (as a primitive inside invariants or transformations) remains explicitly deferred. The failure mode is much worse - a missed predicate in a write-path footprint analysis could let invalid commits through - so it gets its own forcing example and its own scrutiny when forced.

### `ReplaySet` (audit-log replay working set)

**Forced by:** the `morpholog-bench as-of` scenario added in PR #28 - the first benchmark to exercise audit replay at scale, and the first to surface a quadratic in `reconstruct_inner`.

**The pressure:** at N = 10 000 audit transitions, `reconstruct_state_at` took ~4.6 seconds; at N = 100 000 the projected ~8-minute cost was untenable. Same family of pathology as the original write-path quadratic from PR #22: a linear-scan dedupe loop (`claims.iter().any(|c| c == a)`) over a growing `Vec<ClaimInstance>` for every asserted claim, summing to O(N^2) over the full replay. The corresponding retraction loop (`claims.retain(|c| c != r)`) is independently O(|claims|) per retraction, although the bench's asserts-only fixture did not exercise that branch.

**The fix:** a dedicated `ReplaySet` struct inside `morpholog-postgres`, used only by `reconstruct_inner`. Internals:

- `claims: Vec<ClaimInstance>` - every claim ever asserted during replay, in the order it was first asserted.
- `index: HashMap<ClaimInstance, usize>` - maps each claim to its position in the vector.
- `live: Vec<bool>` - `live[i]` is `true` iff `claims[i]` is currently asserted.

`assert` becomes a single `HashMap::entry()` lookup that either inserts (first-time observation) or flips a live bit (re-assertion after retraction). `retract` is a single `HashMap::get()` followed by setting `live[i] = false`. The final `into_state()` walks `claims` once and keeps only the live entries, preserving first-asserted order. `ClaimInstance` derives `Hash` to support this (children - String, Vec<EvalValue>, with EvalValue's Hash from PR #23 - already compose).

**What was confirmed:** every existing test in the as-of suite passed unchanged - the semantic contract was preserved. Bench numbers: N=10 000 dropped from ~4 600 ms to ~140 ms (~33x). N=100 000 became feasible for the first time at ~1 500 ms; scaling is now linear (~13x for 10x N at the small sizes, ~11x at the larger ones - both consistent with linear plus HashMap rehashing overhead).

**Pattern note:** this is the third instance of the same shape: a hot path used a `Vec` with linear scans for membership; replacing the membership check with a HashMap-backed structure made it amortised O(1). PR #23's predicate index on `State`, PR #25's predicate-scoped loading working set, and now PR #28-followup's `ReplaySet`. The pattern is the same; the venue moves with the workload. Worth recognising rather than re-discovering each time.

**Implication for future examples:** the audit-replay path now scales for any workload that fits in memory (~300K claims at the current per-claim size is ~50 MB working state). Further scale concerns are about *fetching* the audit rows (Copilot #5: streaming sqlx queries instead of `fetch_all`), about *re-replaying* large logs repeatedly (snapshots / materialisation), or about retraction-heavy workloads (the ReplaySet already handles them O(1); the bench should add a retraction-heavy fixture to confirm empirically). None of these is forced today.

### `Transition` value object and `audit.actor` (forced ahead of an example)

The first deviation from "forced by a worked example": the shape was paid for in PR #35 before an example required it. The reasoning is in *Why pay this cost up front* below.

**The pressure:** Example 3 left question 2 from the README - *who admitted this entry, and under what authority?* - only half answered. Audit rows recorded transformation, arguments, asserted/retracted claims, and the invariants that governed admission, but no actor identity. Closing the remaining half cleanly requires every committed transition to carry an actor; that in turn requires either (a) every transformation grows an `actor` parameter, polluting every domain payload, or (b) actor lives as transition context separate from the transformation's argument list.

**The alternative we considered and rejected:** (a) - actor as a transformation parameter. Considered (and recommended by ChatGPT's initial review) because it is the smallest local change. Rejected because every transformation in the codebase, plus every future transformation, would have to declare and propagate an `actor` parameter that has nothing to do with its domain logic. That is the kind of cross-cutting plumbing that types-over-everything systems pay for elsewhere; option (b) keeps domain payloads clean.

**What landed (PR #35):**

- `Transition { transformation_name, args, actor }` in `morpholog-core`. The value object proposed against a `Transformation`. `propose()` takes `(&Transformation, &Transition, &State, &[Invariant])` and verifies that `transformation.name == transition.transformation_name`.
- `actor: EvalValue` on `Transition`, validated at the kernel boundary to be `EvalValue::Subject(_)` (anything else surfaces as `EvalError::TypeMismatch`). Future authority work can assume actor identity is subject-shaped without a downstream defensive check.
- `audit.actor jsonb NOT NULL` on the PostgreSQL schema; migration `004_audit_actor.sql` backfills existing rows to a sentinel `{"type":"subject","value":"unknown"}`.
- `PgProposalOutcome::Committed.actor` so the commit receipt is self-describing; the `morpholog propose` CLI surfaces it on stdout.
- A new required `--actor <subject>` flag on `morpholog propose`.
- `system_actor()` helper for runtime-initiated transitions (today: the outbox compensation path). Returns `EvalValue::Subject("morpholog-system")`. A placeholder until first-class actor authority forces a lineage model that distinguishes runtime-initiated from user-initiated transitions.

**What deliberately did NOT land:** `Term::Actor`. No IR construct yet consults the actor; the audit log records it but no invariant or `require` can reach for it. That lands when the first authority invariant earns it.

**Why pay this cost up front:** the alternative was paying it later under more pressure, when the first authority worked example would have forced both the plumbing *and* the consultation primitive in one PR. Separating them keeps each PR honest. The two principles we hold the project to (subtract until it breaks; example forces shape) are in mild tension here, and this entry records that tension rather than papering over it.

**Pattern note:** this is the first time a doctrinally-loaded refactor was paid for ahead of a worked example. The bar for doing so again is high; the test is whether the resulting code felt obviously right (it did, including in review). If a future ahead-of-example refactor leaves the codebase feeling speculatively shaped rather than obviously shaped, that is the signal that the discipline slipped.

### `Term::Actor` consultation primitive (forced by Example 6)

**Forced by:** Example 6 - actor authority.

**The pressure:** PR #35 plumbed actor identity through `Transition` and `audit.actor`, but nothing in the IR could *consult* the actor. Authority remained a half-promise: the runtime knew who proposed a transition, but no `require` clause could read that knowledge. The smallest forcing function was a transformation whose admission gate had to mention the proposing actor - exactly what `approve_document` in Example 6 does:

```
transformation approve_document(doc_id, doc_type):
    require MayApprove($actor, doc_type)
    assert Approval(doc_id, doc_type, $actor)
```

Without `Term::Actor`, the alternatives were all worse: thread the actor as a transformation parameter (the option PR #35 explicitly rejected), or stuff it into a magic binding name (couples user-controlled names to a runtime-controlled slot).

**The design choice:** make `Term::Actor` a distinct IR variant rather than a reserved variable name.

**Considered and rejected:** stashing the actor under a reserved binding key (e.g. `"$actor"`) in `Bindings`. Smaller signature footprint - no function would need a new parameter - but it would have mixed semantically distinct things (user-named bindings vs. runtime-supplied context) into one map. Worse: a user constructing IR directly could create a parameter with the same reserved name and silently shadow the runtime's value. The variant approach makes the shape explicit at the type level and lets the evaluator enforce "no actor available" as a typed error rather than a missing-binding fallthrough.

**What landed:**

- `Term::Actor` variant on the existing `Term` enum, no payload. Resolves to the actor of the proposed transition; raises `EvalError::UnboundActor` if no transition is in scope.
- `actor: Option<&EvalValue>` threaded through `resolve_term`, `find_matches`, `find_claim_matches`, `unify_args`, `find_in_matches`, `find_conjunction`, `eval_value`, `execute_stmt`, `resolve_claim`, `resolve_intent`. The public `eval_invariant` and `enumerate_derived` signatures are unchanged; internally they pass `None`.
- The ground-extraction inside `find_claim_matches` propagates `EvalError::UnboundActor` rather than silently returning an empty match set when `Term::Actor` appears with no actor in scope. This is what makes invariant authoring catch the mistake at evaluation time.

**The doctrine the IR now enforces:** authority checks belong in `require`, not in invariants. An invariant body that reaches for `Term::Actor` errors at evaluation; it cannot silently misbehave. This is the require-vs-invariant lesson (from Example 3) made enforceable by the runtime rather than recorded only in prose.

**What deliberately did NOT land:** decimal comparison (`<=`, `>=`) and predicate-pattern matching. Both are mentioned in `scope-and-ambition.md` as candidates and would extend the authority story (approval limits; one authority claim governing a family of transformations). Neither is forced by Example 6, which models *unconditional* authority. The forcing example for comparison is whichever future example needs "may approve up to N"; the forcing example for predicate-pattern matching is whichever future example needs "may admit any claim of this kind". Both land then, not before.

**Pattern note:** Example 6 is also the smallest worked example to date - two predicates, no invariants, three transformations. The subtraction discipline held: the temptation to add an `at_most_one_approval_per_doc` invariant, or an administrative-authority gate on `grant_approval_authority`, or a `Forbidden` claim, was deflected to the "deliberately not covered" section of the README. The model is exactly what the consultation primitive needs to exist, and no more.

### `Expr::Le` decimal comparison primitive (forced by Example 7)

**Forced by:** Example 7 - approval limits.

**The pressure:** Example 6 demonstrated unconditional authority ("this actor may approve documents of this kind"). The next natural shape any business uses is *quantitative*: "this actor may approve documents of this kind up to N". Without a comparison primitive in the IR, `amount <= limit` had no expression. The two least-honest alternatives - smuggle the comparison through `Eq` games (e.g. equality against a derived "max amount" sum) or extend `Sum`'s value position with a comparison sublanguage - would have left the language saying something it does not mean. The primitive needs to be the primitive.

**The design choice:** add a single decimal-comparison variant (`Expr::Le`), predicate-shaped, with the same dispatch contract as `Eq`.

**Considered and rejected:** adding `Lt`, `Le`, `Gt`, `Ge` all at once. Tempting (the API is "complete"); rejected on subtraction grounds. The example uses `<=`; nothing in the codebase or any worked example yet needs `<`, `>`, or `>=`. Each one earns its place when an example demands it. "Complete sets" of operators are how IRs accumulate.

Also considered: making `Le` take `Term` operands rather than `Expr`, like `Neq` does. Rejected because the natural use case composes with `Sum`/`Sub` (e.g. "cumulative this period <= cap"), and matching `Eq`'s `Box<Expr>` shape keeps composition uniform across the comparison family the IR will eventually grow.

**What landed:**

- `Expr::Le(Box<Expr>, Box<Expr>)`. Both operands evaluate via `eval_value`. Both must yield `EvalValue::Decimal`; anything else surfaces as `EvalError::TypeMismatch("Le expects decimal operands")`. Predicate-shaped: returns the unchanged binding set when `a <= b`, the empty set otherwise.
- `predicates_referenced_by_expr` extended (`Eq | Le | Sub` recurse into both children) so predicate-scoped loading still works against any program that uses comparisons.
- Example 7 (later merged into [`examples/04_approval_controls/`](../examples/04_approval_controls/)): three transformations (grant, revoke, approve), two predicates (`ApprovalLimit` retractable, `LimitedApproval` append-only), no invariants. The `approve_within_limit` transformation's require is `And(ApprovalLimit($actor, doc_type, limit), Le(amount, limit))` - the And binds `limit` from the authority claim, then `Le` compares the proposed amount against it.
- Tests pin: rejection without grant; admission under limit; **boundary equality** (`amount == limit` admits, by `Le`'s inclusive semantics); rejection above limit; per-actor and per-doc-type scoping; stacked grants (the require finds *some* satisfying limit when multiple grants exist); revocation preserves history. Durable test through `propose_against_pg` rounds the same chain to the audit log.

**What deliberately did NOT land:** `Lt`, `Gt`, `Ge`. Each is one variant away when an example forces it. The bar is the same as for every other arithmetic primitive: a concrete worked example whose semantics genuinely need the strict comparison or the right-leaning form.

**Honesty about the typing edge.** `Expr::Le` requires both operands to evaluate to `EvalValue::Decimal`. A non-decimal value in the `limit` position of an admitted `ApprovalLimit` claim makes the require's `Le` raise `EvalError::TypeMismatch` rather than fall through to "no satisfying limit". This is correct behaviour for a structurally-malformed claim - the runtime surfaces the corruption rather than papering it over - but it means `approve_within_limit` is not robust against ill-typed authority records. The complete fix (rejecting non-decimal limits at admission time on `grant_approval_limit`) is the work of typed predicate declarations, which remain deferred. Until an example forces typed predicates, this example's callers are trusted to admit decimal limits. The pinning test `non_decimal_limit_in_authority_claim_surfaces_as_type_mismatch` makes the current behaviour catchable rather than convention.

**Pattern note:** Example 7 is the second worked example whose IR addition was a single variant, paired with a transformation that uses it once. Example 5's `Expr::Sub` was the first; this is the second; Example 6's `Term::Actor` was the third. Each forced exactly one IR variant. That is the shape to keep aiming for - one example, one primitive, one require or assert that needs it.

### `Expr::Add` decimal addition primitive (forced by `insurance_claim_settlement`)

**Forced by:** the `insurance_claim_settlement` example (the first post-reorganisation example, hosted at [`examples/05_insurance_claim_settlement/`](../examples/05_insurance_claim_settlement/)) - cumulative settlements consumed against a per-policy aggregate limit.

Note on numbering: this is the first IR addition forced by a worked example added *after* the example reorganisation noted at the top of this document. Older retrospectives refer to original-numbering examples (1-7); this entry refers to the forcing example by directory name to avoid colliding with the original "Example 5" (derived claims and `Expr::Sub`).

**The pressure:** every previous example expressed comparisons against *fixed* bounds. `Expr::Le(amount, limit)` from Example 7 is a binary test of "this proposed value against this authority ceiling." Insurance aggregate limits require a *cumulative* test: the sum of everything already paid on this policy, plus the proposed settlement, must not exceed the cap. The natural shape is

```text
Le(Add(Sum(paid_on_policy), proposed), aggregate_limit)
```

The least-honest alternative is the inversion `Le(proposed, Sub(aggregate, Sum(paid_on_policy)))`, which expresses the same arithmetic but reads as "the proposed amount must not exceed the headroom" rather than "cumulative consumption must not exceed the cap." The business statement is the first; encoding the IR to match it preserves the modelling intent. When a real example needs cumulative consumption written cleanly, the primitive belongs in the IR.

**The design choice:** add a single decimal-addition variant (`Expr::Add`), shaped exactly like `Expr::Sub`. Same dispatch contract, same TypeMismatch behaviour, same recursion in `predicates_referenced_by_expr`.

**Considered and rejected:** waiting until a forcing example also demanded multiplication or division. Tempting (a single "arithmetic tower" PR would feel weighty); rejected on the same subtraction grounds as `Lt`/`Gt`/`Ge`. Nothing in any worked example or in the codebase yet needs `*` or `/`. Each is one variant away when an example forces it. The bar is the same as for every other arithmetic primitive: a concrete worked example whose semantics genuinely need it.

Also considered: encoding cumulative consumption via the inversion `Le(proposed, Sub(aggregate, Sum(paid)))` and skipping `Add` entirely. Mechanically equivalent; rejected because it makes the example's load-bearing require read backwards relative to the business rule. The IR should make natural business statements expressible directly.

**What landed:**

- `Expr::Add(Box<Expr>, Box<Expr>)`. Both operands evaluate via `eval_value`. Both must yield `EvalValue::Decimal`; anything else surfaces as `EvalError::TypeMismatch("Add expects decimal operands")`. Returns `EvalValue::Decimal(a + b)`.
- `predicates_referenced_by_expr` extended: `Eq | Le | Sub | Add` all recurse into both children. Predicate-scoped loading continues to work against any program that uses arithmetic.
- Kernel tests pin: decimal + decimal works, non-decimal operand surfaces as `TypeMismatch`, and the cumulative-cap composition `Le(Add(running, proposed), cap)` admits under the cap and rejects over it. The composition test is the load-bearing one - it pins the shape the example then exercises end-to-end.
- [`examples/05_insurance_claim_settlement/`](../examples/05_insurance_claim_settlement/) defines the `insurance_claim_settlement` programme: transformations for policy issuance, claim reporting, authority grant, and the load-bearing `authorise_settlement`, plus a `PolicyLimitUsage` derived claim. The load-bearing transformation gates admission on `Le(Add(Sum(SettlementPaid(policy, _, _, paid)), amount), aggregate_limit)` - the require that exercises `Add` exactly once, in exactly the shape that forced it.
- Invariants: `paid_implies_authorised` (every payment must have a matching authorisation record); plus three structural-uniqueness invariants that document what the `ValueOf` pattern depends on - `at_most_one_policy_per_id`, `at_most_one_claim_report_per_id`, and `settlement_id_uniquely_identifies_payment`. These mirror `verified_revenue::at_most_one_current_verification_per_asset_period`: an invariant is the right place for an eternal structural rule that a `ValueOf` lookup relies on. The uniqueness invariants landed after review (Copilot and ChatGPT both flagged the original draft, which lacked them, would have made duplicate-policy or duplicate-claim admissions surface as kernel `ValueOfMultipleMatches` errors rather than lawful rejections).
- Tests pin: under-cap admission, exact-fill boundary equality (cumulative = aggregate admits), over-cap rejection that surfaces from the require, per-policy scoping (a fully-consumed policy does not prevent settlements on a different policy), `PolicyLimitUsage` enumeration matching the sum of admitted payments, and rejection of each uniqueness violation via the corresponding invariant. Durable test through `propose_against_pg` rounds the same chain to the audit log and outbox.

**What deliberately did NOT land:** `*` (Mul), `/` (Div), and any of the strict comparisons (`<`, `>`, `>=`). The same argument as `Expr::Le`'s rejected "complete set": each variant earns its place when a concrete worked example demands it. Multiplication is the most likely next forcing function (interest calculations, fee schedules, unit pricing), but no example today needs it.

**An incidental discovery, recorded for the next IR work.** Building this example surfaced that `Stmt::Require` is a yes/no predicate gate that does NOT propagate its matching bindings back into the active scope. The example originally tried to use sequential requires to bind `policy_id` (from `ClaimReported`) and `aggregate_limit` (from `Policy`); the bindings were silently dropped, and later statements that referenced them failed with `UnboundVariable`. The settlement-netting precedent for the right pattern is `Let` + `ValueOf`: gate existence with a require, then extract the value through an explicit binding. The require-vs-let distinction is real and load-bearing; it just wasn't documented before this example exercised it. No IR change was needed - the existing primitives compose - but the doctrine deserves to surface, and `runtime-semantics.md` is the natural place for it next time the doctrine docs are touched.

**Pattern note:** `insurance_claim_settlement` continues the "one example, one primitive, one require that needs it" pattern that the original `Expr::Sub`, `Term::Actor`, and `Expr::Le` entries each followed. The discipline holds: the example itself stayed deliberately tight - no coverage correction, no reserve restatement, no multi-purpose standing, no effective-time axis - because every one of those patterns is already pinned by an earlier example. Re-illustrating proven patterns in a new domain does not earn its place; only forcing new IR does.

### `Value::Date`, `EvalValue::Date`, and `Expr::DateLe` (forced by `clinical_trial_enrolment`)

**Forced by:** the `clinical_trial_enrolment` example ([`examples/06_clinical_trial_enrolment/`](../examples/06_clinical_trial_enrolment/)) - participant randomisation admissible only if every relevant document (protocol version, consent form version, investigator delegation, eligibility assessment) is in force on the randomisation date.

**The pressure:** this is the first non-finance worked example and the first one that needed to reason about *when* something is valid. The natural shape of the load-bearing rule is

```text
effective_from <= action_date and action_date <= effective_to
```

repeated across protocol, consent form, delegation, and assessment. Every prior example dealt only with subjects and decimals; dates simply didn't appear. Encoding the date-window rule without a civil-date primitive forces one of two losses:

- *Subjects with lexicographic comparison.* `Le("2026-03-31", "2026-04-01")` happens to be true for ISO-8601 specifically, but `Expr::Le` is documented as decimal-only and would have to be widened, or a new `SubjectLe` introduced that pretends strings carry temporal meaning. Either confuses the type story for every future reader.
- *External date predicate.* A host-language function evaluated outside the kernel could compare dates, but it would not participate in audit, replay, or static analysis. Authority to admit a record would live half inside the runtime and half outside.

**The design choice:** treat civil dates as a first-class value type, parallel to decimals.

- `Value::Date(String)` stores the source ISO-8601 string in the IR, mirroring `Value::Decimal(String)`. Parsing is the evaluator's concern, not the IR's.
- `EvalValue::Date(jiff::civil::Date)` is the runtime value. The evaluator parses literals on use via `resolve_term`, with `EvalError::TypeMismatch` covering both wrong-kind operands and malformed ISO source strings.
- `Expr::DateLe(Box<Expr>, Box<Expr>)` is the only date-comparison primitive. Predicate-shaped; admits when `lhs <= rhs` (inclusive). Both operands must evaluate to `EvalValue::Date`; mixing decimals and dates surfaces as `TypeMismatch`, not silent rejection.

**Considered and rejected:**

- *Overloading `Expr::Le` to dispatch on operand kind.* Mechanically possible; would let the helpers stay smaller. Rejected because `Le` is documented as decimal-only in the kernel rustdoc and consumers (the approval-controls example, kernel tests, future readers) read it that way. A generic ordering primitive earns its place when a *third* ordered domain forces one; until then, two named primitives are clearer than one polymorphic primitive whose behaviour is invisible at the call site.
- *Adding `DateLt`, `DateGt`, `DateGe` together.* The "complete set" temptation that the `Expr::Le` entry already rejected. Inclusive validity windows compose as `DateLe(from, date) and DateLe(date, to)`; no current example needs strict ordering. Each new comparator earns its place when an example demands it.
- *A first-class `DateRange` value type with `Within(date, range)`.* Sugar over `DateLe(from, date) and DateLe(date, to)`. Adds a value variant for negligible gain and forces decisions about whether ranges can be open-ended, intersected, subtracted, or projected. Deferred until a worked example forces a date-range operation `DateLe` cannot express directly.
- *Half-open `[from, to)` window semantics.* Common in software, but `effective_to == action_date` reads as "the protocol is valid through this date" in regulatory and clinical prose. Inclusive semantics are pinned in the `DateLe` rustdoc, in [`examples/06_clinical_trial_enrolment/README.md`](../examples/06_clinical_trial_enrolment/README.md), and by the `boundary_equality_admits_*` tests.
- *Reaching for instants, time zones, durations, business calendars, or gas-day semantics now.* All deferred. The kernel needs to keep these as separate future value variants (`Instant`, `Zoned`, `Duration`) so the distinct value types stay distinct under the type story. Pulling them in speculatively would either commit the runtime to a chrono/time/jiff feature footprint nothing currently uses, or pre-decide DST and zone semantics ahead of the worked example that needs them.

**What landed:**

- `Value::Date(String)` and `EvalValue::Date(jiff::civil::Date)`. Stored as the exact ISO-8601 source string in the IR; parsed by the evaluator on first use via `parse_date_literal`. Mirrors the `Decimal(String)` / `EvalValue::Decimal(rust_decimal::Decimal)` pattern that was already in place.
- `Expr::DateLe(Box<Expr>, Box<Expr>)`. Documented in the kernel rustdoc as the only date-comparison primitive in v0; pins inclusive `[from, to]` window semantics; `TypeMismatch` on mixed or malformed operands.
- `EvalValueOrd` extended: `Date` participates in the cross-variant total order (`Decimal < Subject < Bool < Collection < Date`), with within-variant ordering delegated to `jiff::civil::Date`. Used only inside `enumerate_derived` for deterministic key-tuple ordering.
- `predicates_referenced_by_expr` extended: `Eq | Le | DateLe | Sub | Add` all recurse into both children. Predicate-scoped loading continues to work against any programme that uses civil-date comparisons.
- JSON codec: civil dates serialise as `{"type":"date","value":"YYYY-MM-DD"}` under the existing adjacently-tagged shape. The wire format is jiff's default ISO-8601 string; pinned by a codec round-trip test. The property-based round-trip test was extended to include civil dates so future codec changes cannot drift the shape silently.
- Kernel tests pin: `DateLe` admits earlier and equal dates; rejects later dates; `TypeMismatch` on decimal-vs-date and date-vs-subject operands; `TypeMismatch` on a malformed ISO source string; literal `Value::Date` unifies positionally with admitted `EvalValue::Date` arguments. Plus the existing `predicates_referenced_by_expr` exhaustive-variant test, extended for `DateLe`.
- [`examples/06_clinical_trial_enrolment/`](../examples/06_clinical_trial_enrolment/) defines the `clinical_trial_enrolment` programme: setup transformations for opening a trial, approving protocol and consent form versions with windows, delegating investigators, recording consent, criteria, and assessments, plus the load-bearing `randomise_participant`. Three structural-uniqueness invariants (`at_most_one_protocol_window_per_version`, `at_most_one_consent_window_per_version`, `participant_randomised_once_per_trial`). No "validity" invariant - validity is admission-time only, mirroring the `verified_revenue` standing-after-correction doctrine.
- Tests pin: happy-path admission, boundary equality at both window endpoints, every per-gate rejection (expired protocol window, expired consent form, expired assessment, expired delegation, open important protocol deviation, failed assessment result), and the load-bearing standing-after-amendment scenario - admit `proto_v2` after a randomisation under `proto_v1`, the earlier admission survives, and a later participant must enrol under `proto_v2`. A PG integration test rounds the same shape through `propose_against_pg`, confirming `EvalValue::Date` round-trips through PG JSONB without loss.
- Workspace dep: `jiff = "0.2"` with `default-features = false, features = ["std", "serde"]`. No tzdb bundled; civil dates only.

**What deliberately did NOT land:**

- `DateLt`, `DateGt`, `DateGe`, date arithmetic (adding days, subtracting two dates to get a duration). No example needs strict ordering yet; date arithmetic awaits a worked example whose semantics genuinely depend on counting days.
- `Value::Instant` / `Value::Zoned`. Time-of-day, time zones, and DST require an instant primitive distinct from a civil date - "2026-03-30 in Europe/London" is not the same window as "2026-03-30 in America/New_York" once business hours enter the picture. The kernel design has been chosen to make those additions incremental (a separate value variant, not a retrofit of `Date`), but no example forces them today. Pulling them in speculatively would commit to tzdb feature footprint and DST semantics ahead of a forcing pressure.
- Business calendars, gas day, settlement period, IFRS effective-from-effective-to dual axes, retrospective amendments to protocol effective windows. Each is its own forcing pressure; none is forced by `clinical_trial_enrolment`.
- A `Forall`-over-criteria eligibility shape. The current example admits if the protocol's single matching criterion has a valid assessment; a real trial has many criteria, and the natural shape combines `Forall` with `DateLe`. The kernel already supports `Forall`; layering it on top of the date-window primitive in this example would muddy the forcing pressure. A future example combining multi-criterion eligibility with restatement-on-correction would be the natural next step.

**Pattern note:** `clinical_trial_enrolment` is the first non-finance worked example. The IR addition is small (one literal variant, one runtime variant, one expression variant, one helper), strictly subtractive in spirit (no second comparator, no instant type, no arithmetic), and the example itself stays tight - just enough surface to force the primitive. The civil-date / instant / zoned distinction is deliberately set up so future temporal primitives can be added without retrofitting `Date`. This sets the floor for any future "time-of-day," "time-zone," "gas day," "settlement period," "business calendar" work: each will arrive as its own value type with its own forcing example, and `Value::Date` will not silently widen to cover them.

### `Stmt::BindOne` (forced by the refactor arc, not a worked example)

**Forced by:** the bigger-refactor planning discussion that followed PR #46/#47/#48. The accidental-complexity audit identified `Stmt::Require` + `Stmt::Let` + `Expr::ValueOf` as a three-primitive workaround for one missing thing - extracting a uniquely-matching claim's values into the statement-level binding context. The doctrine was tripped on PR #45 (and almost re-tripped on PR #47); both ChatGPT and the in-house review converged on a narrower binding primitive.

**The pressure:** every non-trivial transformation that needed a value from a uniquely-identified claim wrote the same shape:

```
require ClaimReported(claim_id, _, _)                    -- existence gate
let policy_id = value_of ClaimReported(claim_id, _, _)   -- re-match, extract value
let aggregate_limit = value_of Policy(policy_id, _)       -- another extract
```

Three statements with two redundant claim matches. The redundancy comes from `require` deliberately discarding its match's bindings (the "yes/no gate" semantics); `let + value_of` re-runs the same match to harvest the value. Conceptually one operation; mechanically three.

**The design choice:** add a single new statement variant.

- `Stmt::BindOne(Expr)`: evaluate a predicate-shaped expression against current state, bindings, and actor; on the single returned binding set, replace the current binding context. Zero matches → lawful `Outcome::Rejected`. Multiple matches → `EvalError::TypeMismatch` (kernel error; programme expected unique state but admitted ambiguous state).

**Considered and rejected:**

- *A `Stmt::Filter` that propagates all surviving bindings, branching on multiplicity.* Mechanically equivalent to a multi-match handler, but moves transformation execution toward Datalog-style query plans: would `assert` then run once per binding, or branch the outcome? Both answers change Morpholog's semantic model. Rejected: BindOne preserves single-path execution.
- *Picking the "first" match arbitrarily on multi-match.* Non-deterministic by construction. Rejected.
- *Returning `Outcome::Rejected` on multi-match.* Considered, but multi-match means the programme is wrong (missing uniqueness invariant or corrupted state), not that the business rule failed. Surfacing as a kernel error makes the diagnostic loud rather than hiding it behind a business rejection.
- *Deleting `Expr::ValueOf` entirely.* Considered, but ValueOf remains the right tool in value-producing positions where a statement-level binding extension does not fit: inside `Sum`, `Add`, `Sub`, `Le`, `DateLe`, or inside a `DerivedClaim` value expression. Demoted to "prefer BindOne in transformation bodies" in the rustdoc and runtime-semantics doc.
- *Restricting `BindOne` to `Expr::Claim`.* Considered for safety. Rejected because the runtime's existing `NotPredicate` guard already rejects value-only expressions, and permitting `bind_one(and(claim_a, claim_b))` for joined unique lookups is a coherent natural extension.

**What landed:**

- `Stmt::BindOne(Expr)`. Replace-not-extend bindings on a single match (the matcher's returned set is the new authoritative context; `find_matches` already threads `base` through). Zero → `Outcome::Rejected`; reason includes the rendered expression. Multiple → `EvalError::TypeMismatch` with multiplicity + rendered expression.
- `dsl::bind_one(expr)` constructor.
- `format_stmt` arm rendering `bind_one <expr>` on a single line.
- `format::format_expr_inline` promoted to `pub` so the kernel can use the rendered expression in rejection reasons and multi-match errors. `Stmt::Require`'s reason also picked up the rendered-expression upgrade as part of the same change ("require failed: `<expr>` did not hold over pre-state").
- `Stmt::For` properly scopes its body. Variables bound inside the body are reset to a snapshot at the start of each iteration and restored after the loop. Without this scoping, a `bind_one` inside a `for` would have iteration-2's lookup constrained by iteration-1's residual binding - a latent footgun made acute by BindOne but already present for any future statement that exposed iteration-level state. The `bindings.remove(binding)` cleanup that the previous For arm did is subsumed by the snapshot/restore.
- 7 new kernel tests pinning: unique-match-extends-for-next-statement, zero-rejects-with-named-predicate, multi-errors-with-count, pre-bound-var-narrows-match, inside-for-body-composes (also exercises the For scoping fix), with-actor-in-pattern, rejects-value-expr-as-NotPredicate.
- Migrations: `insurance_claim_settlement::authorise_settlement` (the three-line `require + let + value_of` pair collapsed to two `bind_one`s); `settlement_netting::create_net_settlement` (the per-line `let amt = value_of(...)` inside the For body collapsed to a single `bind_one`).
- `.morph` illustrative files updated for both examples.
- `runtime-semantics.md` doctrine section rewritten as the four-way carve: require (gate), bind_one (unique lookup), let (value computation), for (iteration).
- `ValueOf` rustdoc demoted with the cross-reference to `bind_one`.

**What deliberately did NOT land:**

- `predicates_referenced_by_stmt`. The natural place for a statement-level predicate walker, and BindOne would slot into the same exhaustive match. But no current consumer exists - the future predicate-scoped write-path PR is the forcing pressure, and the walker should arrive then.
- Deleting `Expr::ValueOf`. Kept for value-producing positions.
- Changing `Require` semantics. Require still discards its match's bindings; the difference between require and bind_one is now load-bearing doctrine, not a footgun.
- Predicate declarations. The metadata layer that lets the kernel validate arity at construction time is PR C, not this PR.
- `propose_with_trace`. The structured-trace diagnostic API is PR D.

**Pattern note:** BindOne is the first PR in the bigger-refactor arc that's not "forced by a worked example" in the original sense. The forcing function is the **authoring experience itself**: every example pays the require-vs-let workaround, and the doctrine had to be rediscovered by reviewers on PR after PR. Subtracting the workaround is its own kind of forcing pressure. The discipline that "no IR change lands without a worked example" was correct for v0 when primitives were the scarce thing; the next phase tunes the **shape** of those primitives so authoring against them stops accumulating ceremony. The same logic will land predicate declarations next (PR C) and `propose_with_trace` after (PR D).

### `PredicateDecl` + `Program::predicates` + strict arity validation (forced by the refactor arc)

**Forced by:** the third item in the accidental-complexity audit. Every claim/assert/retract/value_of/derived-claim site was a positional `vec![var(), var(), ...]` tuple with no kernel awareness of arity or argument names. A swapped argument order admitted a wrong claim silently; every example essentially re-declared its predicates by repetition; future predicate-pattern matching (higher-order authority, on the CLAUDE.md deferred list) needed predicate names as first-class IR values.

**The pressure:** programmes had no central vocabulary. The kernel could not validate that `assert Policy(p)` referenced a real predicate with the right arity, because there was no notion of "real predicate" at all. The same shape kept showing up across examples (subject-subject-decimal for `Policy`; subject-decimal for `LineAmount`) but it lived in the reader's head, not in the IR.

**The design choice:** add a fourth list to `Program`.

- `Program.predicates: Vec<PredicateDecl>` declares the programme's claim vocabulary.
- Each `PredicateDecl` carries a name and a positional list of `PredicateArgDecl`s (name + kind).
- `PredicateArgKind` enumerates the kinds a position can take: `Subject`, `Decimal`, `Date`, `Bool`, `Collection`, `Any`.
- `Program::validate() -> Result<(), Vec<ValidationError>>` runs strict arity validation: every `Expr::Claim`, `Stmt::Assert`, `Stmt::Retract`, `Expr::ValueOf`, and `DerivedClaim` output reference must target a declared predicate with matching arity. Strict mode - undeclared predicates are errors, not passthrough. The validator collects every error rather than failing fast, so a migration sees the full work list at once.

**Considered and rejected:**

- *Permissive mode (skip arity check for undeclared predicates).* Lets migrations be incremental, but a half-declared programme is half-self-documenting, and forgetting to declare a new predicate produces no warning. Strict is the only mode that makes declarations real.
- *Calling the kind enum `ValueKind`.* Sounds like it classifies all runtime values, but what's actually being declared is the expected kind of a *predicate argument position* - a declaration-time concept distinct from `Value` / `EvalValue`. Renamed to `PredicateArgKind` (per the in-house review) to keep the type story clean.
- *Auto-validating inside `propose`.* Considered, rejected because `propose` operates at the statement level (it sees a `Transformation` and a `[Invariant]` slice, not a `Program`), and adding programme-level validation to every proposal would muddle the kernel boundary and add overhead. Validation is explicit: tests and the CLI call `validate()` directly.
- *Kind validation against the kinds of values flowing through the binding context.* Considered, deferred. Recording kinds now means migrations stay shallow when kind checking arrives, but enforcement requires tracking variable kinds through parameters, `bind_one`, `let`, `for`, and aggregates - a larger type pass. Arity-only is the simplest useful first step.
- *Intent declarations (`IntentDecl`).* Considered, deferred. Intents are outbox vocabulary, not admitted-claim vocabulary; the distinction is important. An `IntentDecl` parallel concept is conceivable but should not crowd PR C; the asymmetry between checked `Assert(Claim)` and unchecked `Emit(Intent)` is documented rather than papered over.
- *Builder vs constructor-with-helpers for the DSL.* Builder won on call-site readability: `predicate("Policy").subject("policy_id").decimal("aggregate_limit").build()` keeps the predicate name with its arg list and avoids polluting the DSL namespace with `subj_arg`, `dec_arg`, etc.

**What landed:**

- `PredicateDecl`, `PredicateArgDecl`, `PredicateArgKind` types (with `Serialize` + `Deserialize` so CLI inspection can emit JSON).
- `Program.predicates` field. Every built-in example populates it via an `all_predicates()` function (40+ declarations across the workspace).
- `Program::validate()`, `ValidationError` (`UndeclaredPredicate`, `ArityMismatch`, `DuplicatePredicateDecl`), `ValidationContext` (`Invariant`, `Transformation`, `DerivedClaim`).
- `dsl::predicate(name)` builder with `subject`/`decimal`/`date`/`boolean`/`collection`/`any`/`build` methods. `boolean` (not `bool`) to avoid the visual overload with the Rust type.
- `format_program` extension: predicate declarations render between the header and the invariants section, one per line as `predicate Name(arg: Kind, ...)`.
- `morpholog inspect predicates <program>` CLI subcommand returning JSON.
- Kernel tests pinning each `ValidationError` variant + the "collects all errors" behaviour.
- `program.rs` workspace test asserting every registered programme passes strict arity validation. If a future example adds a new predicate without declaring it, this test fails with the full list of missing/mismatched sites.

**What deliberately did NOT land:**

- Kind validation. Metadata recorded, enforcement deferred.
- `Expr::fold` / Visitor abstraction. The validator is one new walker; until a third one shows up, the manual exhaustive match stays the right choice (same compile-time gate as `predicates_referenced_by_expr`).
- `IntentDecl`. Intents are not claim vocabulary; this distinction is captured explicitly rather than papered over.
- Predicate-pattern matching / higher-order authority. Predicate declarations are the prerequisite; the feature itself is its own PR forced by a worked example.
- Auto-validation inside `propose`. Kernel boundary stays at the statement level.

**Pattern note:** PR C completes the three-PR refactor arc that started with PR A (public DSL + test-support + format_program) and PR B (`Stmt::BindOne`). The progression matters: PR A made the IR pleasant to author; PR B removed the biggest authoring footgun; PR C makes programmes structurally self-describing. After PR C, a Morpholog programme is no longer "some transformations and invariants" - it's a declared vocabulary of admissible claim shapes plus transformations and invariants over that vocabulary. That stronger conceptual object is what predicate-pattern matching, future kind checking, generated docs, and the eventual parser all build on.

### `propose_with_trace` (forced by the refactor arc)

**Forced by:** the fourth and final item in the accidental-complexity audit. After PR A (public DSL), PR B (`Stmt::BindOne`), and PR C (predicate declarations), the remaining friction in authoring against Morpholog was *understanding why a proposal rejected*. Today's `Outcome::Rejected { reason }` carries one line; for any non-trivial transformation body, that's not enough to attribute the failure to the right statement, and for kernel errors the `Result<_, EvalError>` shape drops everything that came before.

**The pressure:** debugging a failing example took 5 minutes of `println!` archaeology because:

- A failing `require` told you *that* a require failed, not *which* of several requires in the body.
- A failing `bind_one` zero-match told you nothing about the bindings that led up to it.
- A multi-match `bind_one` raised `EvalError::TypeMismatch` and discarded the trace; the worst debugging case dropped the most diagnostic surface.
- An invariant rejection said `"invariant X violated"` with no record of what the transformation body had staged.
- Nested `for` loops surfaced as opaque "require failed" with no iteration context.

The fix is a structured per-statement trace.

**The design choice:** a parallel `propose_with_trace` function returning a `TracedProposal` enum, plus a shared internal execution path via a `TraceSink` enum.

**Considered and rejected:**

- *`propose` grows an `&mut Option<Vec<TraceEntry>>` parameter.* Single function, fewer call-site changes. Rejected: every existing `propose` caller would need to pass `None`, the `&mut` param is a small papercut at every non-tracing site, and a separate function reads more honestly at the call site (`propose_with_trace` is opt-in, `propose` stays untouched).
- *`propose_with_trace -> Result<(Outcome, Vec<TraceEntry>), EvalError>`.* Rust-idiomatic but **drops the trace on `Err`**, exactly when the trace is most valuable (multi-match `bind_one`, type-mismatch in arithmetic, unbound actor). Rejected for the same reason ChatGPT flagged in review: it's a quiet footgun.
- *Two separate evaluators (one traced, one not).* Rejected as the path to drift. The shared `TraceSink` keeps both modes on one executor; the `Off` sink is a no-op, the `On` sink appends.
- *Trace expression internals (which conjunct of an `And` failed).* Considered, deferred. Would require a parallel `find_matches_with_trace` and visitor-style threading through every Expr variant. The simpler statement-level trace is the smallest forcing-pressure-driven primitive; expression-level can land as a follow-up if a worked example demands it.
- *Persist the trace to the audit log.* No. Trace is debugging metadata, not durable record. The audit log already pins what happened; the trace explains why.
- *Trace `enumerate_derived`.* Different concern; failures there are kernel errors over read-only state. Deferred.

**What landed:**

- `TracedProposal::{Completed { outcome, trace }, Errored { error, trace }}` - trace carried on both paths.
- `TraceEntry` enum with one variant per statement kind plus `InvariantCheck`. `Retract` records the actual retracted claims (not just a count). `InvariantCheck` records the rendered body expression. `Let`, `LetNewSubject`, `Assert`, `Emit` record the resolved value / claim / intent. `BindOne::Bound` records the full new binding context sorted by variable name.
- `ForIterationTrace { item, trace }` - nested sub-trace per iteration with the iteration item preserved. A failing third iteration is attributable to its collection element.
- `RequireOutcome::Held { match_count }` records the find-matches cardinality; `Rejected { reason }` on failure.
- `BindOneOutcome::{ Bound, NoMatch, MultipleMatches { count } }` matches the three branches of `Stmt::BindOne`'s contract.
- Shared executor via `TraceSink::{Off, On(&mut Vec<TraceEntry>)}`. `propose` calls with `Off`, `propose_with_trace` with `On`. The non-trace path allocates no trace storage; per-statement work is a single-variant enum match the optimiser collapses.
- 9 kernel tests pinning each branch, including the multi-match-error-with-partial-trace contract.
- One migrated example test (`insurance::authorise_settlement_without_authority_is_rejected_at_require`) demonstrates the DX win: instead of `reason.contains("require")`, the test asserts that both bind_ones succeeded and the specific `SettlementAuthority`-bearing require is the failing one.

**What deliberately did NOT land:**

- Expression-internal tracing. `Require` and `BindOne` trace entries carry the top-level expression as a rendered string but do not drill into its sub-tree. Documented as a possible later PR.
- CLI `--trace` flag and `propose_against_pg_with_trace`. Split to PR D2. PR D pins the kernel trace shape; PR D2 will settle the JSON wire format and the PG adapter wrapper (which has its own questions: trace covers kernel admission only, not SQL/audit/outbox stages).
- Serde derives on `TracedProposal` / `TraceEntry`. Transitively requires `Outcome` and `EvalError` to serialize, which transitively requires `State` - all out of scope for PR D. PR D2 will introduce serializable wrapper types.
- `enumerate_derived_with_trace`.
- Persisting trace to the audit log.

**Pattern note:** PR D closes the refactor arc. PR A made the IR pleasant to author; PR B removed the biggest authoring footgun; PR C made programmes structurally self-describing; PR D makes execution diagnostically transparent. The arc's spine is the same: each PR identifies one source of *authoring friction* and removes it without weakening the kernel's discipline. Trace is the smallest possible diagnostic primitive - one entry per statement, full values, partial trace on error - that genuinely changes the loop from "add println, recompile, infer" to "read the trace."

### `propose_against_pg_with_trace`, CLI `--trace`, and the refactor-arc consolidation pass (PR D2)

**Forced by:** the natural completion of PR D. The kernel `propose_with_trace` was useful for unit tests but not yet reachable through the CLI or against a real PostgreSQL database. PR D2 closes that loop and bundles a documentation pass to align the headline surfaces (README, scope-and-ambition) with the refactor arc that's just landed.

**The pressure:** after PR D, the trace API existed only as a kernel primitive. Every realistic debugging session uses the CLI against a real database. Without a CLI flag and a PG adapter wrapper, the trace was a kernel-test-only capability. Meanwhile the headline docs still described Morpholog as it existed before PR A - anyone landing on the repo got the wrong mental model.

**The design choice:** two changes bundled into one PR.

**CLI `--trace`:**

- `propose_against_pg_with_trace` in the PG adapter, returning `Result<(PgProposalOutcome, Vec<TraceEntry>), PgError>`.
- `morpholog propose --trace` flag emitting `{"result": <PgProposalOutcome>, "trace": [<TraceEntry>...]}` on stdout for committed and rejected outcomes.
- Serde derives added to `TraceEntry`, `RequireOutcome`, `BindOneOutcome`, `ForIterationTrace` - but NOT `TracedProposal` (would transitively require `Outcome`, `EvalError`, `State` to serialize; those carry larger payloads and the CLI doesn't need the kernel-level wrapper type anyway).

**Considered and rejected:**

- *Preserving trace on `PgError`.* The kernel's `TracedProposal::Errored` variant preserves trace on `EvalError`. At the PG boundary, the wrapping `PgError::Kernel(EvalError)` flows through the same `Result::Err` path as `Database` / `SerializationFailure` / `Encoding`, and the simpler `Result<(_, trace), PgError>` shape drops the trace. Documented as a known v0 limitation. Callers needing kernel-error trace can use `propose_with_trace` directly. A richer return type (a `PgTracedOutcome` enum carrying trace on kernel-errored paths) was considered but rejected on smallest-increment grounds - the rare "kernel succeeded but DB op failed and I want the trace" case is the harder design problem and should land when forced.
- *Always emit the `{result, trace}` shape from the CLI, even without `--trace`.* Would change the existing wire format for the non-trace `propose` path. Rejected: existing scripts parse stdout as bare `PgProposalOutcome`; breaking that contract for a debugging flag is the wrong trade-off.
- *Adding `--trace` to `morpholog inspect derived` and the other inspect subcommands.* The inspect subcommands are read-only over admitted state; there's no transformation body to trace. Out of scope.

**Consolidation pass:**

- Root `README.md` - "Worked examples" descriptions updated where the IR shape changed (insurance, settlement-netting); "Project status" paragraph now describes programmes as declared-vocabulary objects with structured trace; "Where this is heading" reflects that the refactor arc moved several items off the deferred list.
- `docs/scope-and-ambition.md` - "What the language actually needs" section now marks Typed predicate declarations as *(landed)*. Roadmap Level 2 lists the refactor arc and the strengthened conceptual shape of `Program` after it.
- `docs/runtime-semantics.md` - already updated by the PRs themselves (programme vocabulary contract, four-way binding doctrine, trace section).

**What landed:**

- Serde derives on `TraceEntry` (internally tagged on `"kind"`), `ForIterationTrace`, `RequireOutcome` and `BindOneOutcome` (both internally tagged on `"status"`). The wire format the CLI emits is now an explicit public surface - documented and tested; not yet promised stable across versions while Morpholog is pre-parser and actively evolving.
- `propose_against_pg_with_trace` in `morpholog-postgres`. Threaded through `finalise_outcome` so both `propose_against_pg` and the trace variant share the post-kernel persistence path.
- `--trace` flag on `morpholog propose`. JSON output shape: `{"result": <PgProposalOutcome>, "trace": [<TraceEntry>...]}`.
- Two PG integration tests (committed + rejected paths exercising the new function end-to-end against PostgreSQL).
- Two CLI parse tests (flag parses; absence defaults to false).
- One shared common test helper (`propose_pg_with_trace_using_test_actor`).
- Consolidation pass across the headline docs.

**What deliberately did NOT land:**

- Trace preservation on `PgError`. Documented limitation.
- Serde on `TracedProposal` / `Outcome` / `EvalError` / `State`. The CLI uses `PgProposalOutcome` (already serialisable) + `Vec<TraceEntry>` (now serialisable); the kernel-level `TracedProposal` enum stays Rust-only.
- Expression-internal tracing. Still PR E territory.
- `enumerate_derived_with_trace`. Out of scope.
- Persisting trace to the audit log. Out of scope.

**Pattern note:** PR D2 is the closing piece of the refactor arc. The arc removed authoring friction without weakening the kernel discipline; the next substantive PR should either force a new IR primitive via a worked example, or address expression-internal tracing if a real debugging scenario demands the conjunct-level capability.

### `PgTracedOutcome` + kernel-core module split + small cleanups

**Forced by:** the closing-loop work after the trace arc. PR D2 documented the limitation that `propose_against_pg_with_trace` dropped the trace on `PgError::Kernel(EvalError)` - exactly the case where the trace is most valuable. With the trace surface settled, the cleaner shape became available without further design risk. Bundled with two structural improvements: splitting `morpholog-core`'s 3,800-line `lib.rs` into focused submodules, and renaming a name collision between two `InvariantCheck` types at different layers.

**The PgTracedOutcome shape.** Replaces `Result<(PgProposalOutcome, Vec<TraceEntry>), PgError>` with `Result<PgTracedOutcome, PgError>` where `PgTracedOutcome` is `Outcome { outcome, trace } | KernelErrored { error, trace }`. Kernel-side outcomes (commit/reject/error) flow through `Ok`; PG-layer errors (Database, SerializationFailure, Encoding, InvalidState) flow through `Err`. The kernel-errored path explicitly rolls back the open SERIALIZABLE transaction before returning so the connection is released eagerly and any rollback-time DB failure surfaces as a distinct `PgError::Database` rather than being swallowed.

**The CLI three-way fork.** `morpholog propose --trace` now emits a structured JSON object for every kernel-side outcome:
- Committed / Rejected: `{"result": <PgProposalOutcome>, "trace": [...]}` (existing shape).
- Kernel errored: `{"result": {"status": "errored", "error": "..."}, "trace": [...]}` (new). Exit code 1.
- PG-layer errors still surface via the existing anyhow stderr chain.

**The module split.** `morpholog-core/src/lib.rs` was 3,812 lines including 1,420 lines of inline tests. Split into focused submodules: `ir` (IR types), `state` (runtime state types), `eval` (the evaluator), `propose` (transformation execution + trace types), `derive` (invariant + derived-claim evaluation), `validate` (`Program::validate` machinery), `analysis` (predicate walkers, with the new `predicates_referenced_by_stmt`). Tests stayed inline in `lib.rs` with `pub(crate)` on items they touch; redistributing 40 tests across 7 modules was rejected on smallest-increment grounds. `lib.rs` is now ~1,500 lines (module declarations, re-exports, and tests) - the navigation win is the major change; the test surface stayed coherent.

**predicates_referenced_by_stmt.** New analysis walker matching the existing `predicates_referenced_by_expr` and `predicates_referenced_by_derived` but at the statement layer. Unblocks a future predicate-scoped loading optimisation on `propose_against_pg`'s write path - currently the full claim table is loaded; with statement-level scoping, only the predicates a specific transformation actually touches need to load.

**InvariantCheck rename.** `morpholog_postgres::InvariantCheck` (the audit-row entry persisted alongside committed transitions) is renamed to `AuditedInvariantCheck` to disambiguate from `morpholog_core::TraceEntry::InvariantCheck` (the kernel's per-call diagnostic entry). Same concept name; different layer; the rename surfaces the distinction.

**Considered and rejected:**

- *Redistributing the 1,420-line inline test module across submodule `mod tests` blocks.* The Rust-idiomatic shape; rejected because the test module's `super::*` import currently gives all production items at the kernel root, and breaking that up into per-module test trees would touch 40 tests for navigation gain only. The `pub(crate)` upgrade on internals plus explicit `use crate::eval::*` in the test module is the minimal change.
- *Removing `PgError::Kernel`.* With the new shape, `propose_against_pg_with_trace` no longer produces `PgError::Kernel`. But `propose_against_pg` (non-trace) still does, so the variant stays.
- *Adding `PartialEq` / `Eq` to `PgTracedOutcome`.* Would transitively require them on `PgProposalOutcome` (currently `Serialize`-only) and on its `EvalValue`/`ClaimInstance` contents. Not needed for the test patterns (which all destructure via `let-else`), so dropped from the derive.
- *Splitting outbox machinery out of `morpholog-postgres/src/lib.rs` (1,984 lines).* Worth doing as its own PR; the same kind of mechanical change but on a different file. Not in this PR.

**What landed:**

- `PgTracedOutcome` enum in `morpholog-postgres`. `propose_against_pg_with_trace` returns it. Explicit `tx.rollback()` on the kernel-error branch.
- CLI `--trace` flag handles the three-way fork (Outcome / KernelErrored / PG error).
- `morpholog-core` split into `ir`, `state`, `eval`, `propose`, `derive`, `validate`, `analysis` submodules. `lib.rs` is now a slim re-export surface + tests.
- New `predicates_referenced_by_stmt` walker in `analysis.rs`.
- `morpholog_postgres::InvariantCheck` -> `AuditedInvariantCheck` rename.
- New PG integration test pinning the kernel-errored trace-preservation contract end-to-end against PostgreSQL.

**What deliberately did NOT land:**

- Test redistribution across kernel submodules. Deferred as a navigation-quality follow-on.
- `morpholog-postgres` outbox extraction. Deferred; same kind of work.
- `enumerate_derived_with_trace`. Out of scope.
- Expression-internal tracing. Still PR E territory.

### Predicate-scoped loading on the write path (`propose_against_pg`)

**Forced by:** the natural completion of PR #53's `predicates_referenced_by_stmt` walker, plus the perf reality the bench has surfaced for a while: `propose_against_pg` was loading the full `morpholog.claims` table on every call, regardless of how few predicates the transformation actually consults. On a 100K-claim ledger the unconditional load dominated the commit cost; for transformations touching three predicates with low cardinality, fetching and decoding ~99K irrelevant rows was pure waste.

The read path has had predicate-scoped loading since the trial-balance work (PR #25's `list_claims_for_predicates`). The write path lagged because statement-level analysis didn't yet exist; PR #53 landed it, this PR consumes it.

**The shape:**

- New analysis function `predicates_read_by_stmt` in `morpholog-core/src/analysis.rs`. Mirrors `predicates_referenced_by_stmt` (the broad walker) but excludes `Stmt::Assert`'s output predicate. The Assert is a *write* - the staged claim's predicate has no bearing on what pre-state must be loaded. `Stmt::Retract` stays in the read set: its pattern is matched against pre-state to find what to retract.
- `morpholog-postgres::load_state` gains a `scope: &[String]` parameter. SQL becomes `SELECT ... WHERE predicate_name = ANY($1)`. Empty scope returns `State::default()` without issuing a query, matching the precedent set by `list_claims_for_predicates`.
- A new internal helper `compute_load_scope(transformation, invariants) -> Vec<String>` does the union: every predicate read by every body statement, plus every predicate referenced by every invariant body. Both `propose_against_pg` and `propose_against_pg_with_trace` use it.

**Why include invariants in the scope.** Invariants evaluate against the candidate state (`pre_state ⊕ asserts ⊖ retracts`); the asserts can introduce new predicates into the candidate without affecting what's in pre-state. But if an invariant references a predicate the transformation doesn't touch, that predicate's existing pre-state matters - the invariant may be checking a relationship between asserted claims and pre-existing ones. Failing to load invariant-referenced predicates would silently make invariants vacuously hold against an incomplete view.

**Why split the walker (`predicates_read_by_stmt` vs `predicates_referenced_by_stmt`).** ChatGPT's review on PR #53 flagged this: a single walker that conflates reads and writes is fine for "what predicates does this statement mention?" but wrong for scoped loading. The split is now explicit. The broad walker stays available for callers that genuinely want every predicate (dependency tracing, future docs generation).

**Considered and rejected:**

- *Single-walker approach with a read-only flag.* Hides intent at call sites; the type system can express "this is the read-set" via a separate function more clearly.
- *Auto-deriving the scope inside `propose_against_pg` without surfacing it.* Considered, but the explicit `compute_load_scope` helper is more testable and easier to instrument later (e.g., for a future tracing pass that records which predicates were loaded).
- *Adding `predicates_written_by_stmt` in the same PR for symmetry.* Considered, deferred. No current consumer; the discipline is one walker per real consumer.

**What landed:**

- `predicates_read_by_stmt` in `analysis.rs` with exhaustive `Stmt` match (same compile-time gate as the other walkers).
- `load_state(tx, scope)` with scoped SQL and `State::default()` short-circuit on empty scope.
- `compute_load_scope` helper in `morpholog-postgres/src/lib.rs`.
- Both PG-adapter `propose` entry points call `compute_load_scope` before loading.
- Kernel test pinning the read-set contract (`Stmt::Assert` excluded; `Stmt::Retract` included; sanity-check that the broad walker still includes `Assert`).
- PG integration test: noise claims of an unreferenced predicate must not affect the outcome - assertions extended (per Copilot's review on PR #54) to pin the full Committed outcome shape against the no-noise baseline, not just `matches!(Committed)`.
- Parity: all 23 pre-existing PG integration tests pass unchanged under scoped loading.
- `--noise-claims K` flag on `morpholog-bench` to make the perf win visible. The bench README's "Observations" section now carries a four-row comparison (scoped vs. unscoped, with and without noise) at `N = 100 000`: with 200 000 noise claims, unscoped `propose_one` grows by ~54% (1 667 -> 2 562 ms) while scoped `propose_one` stays flat at ~1 600 ms.

**What deliberately did NOT land:**

- `predicates_written_by_stmt`. No forcing consumer.
- A `--noise-claims` axis on the `as-of` scenario. Its fixture bypasses `propose_against_pg` (audit rows are fabricated directly), so noise-tolerance is not a fair comparison there; this can come if `reconstruct_state_at_for_predicates` ever needs benchmarking under noise pressure.
