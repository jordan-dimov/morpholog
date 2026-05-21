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
