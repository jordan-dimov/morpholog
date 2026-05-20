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
| Sharing IR fixture helpers across example modules | The `morpholog_examples::*` modules share an internal `helpers` constructor module for IR brevity, but each example module declares its own predicates, transformations, and invariants - they happen to reuse predicate names (e.g. `IndependentlyVerifiedRevenue` in Examples 2 and 3) without sharing constructor code. Keeps examples independent. |
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

- `Expr::Sub(Box<Expr>, Box<Expr>)` for decimal subtraction. Both operands evaluate as decimals; the result is `EvalValue::Decimal(a - b)`. Non-decimal operands surface as `EvalError::TypeMismatch`. Deliberately the only arithmetic primitive: no addition, multiplication, or division until a real example forces them.
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

**Honest disclosure:** this entry is the first deviation from the strict "forced by a worked example" discipline. The shape was paid for in PR #35 *before* an example required it. Recorded here so the deviation is visible rather than rationalised after the fact.

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
