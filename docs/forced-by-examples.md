# What the worked examples forced into the language

Status: retrospective. Companion to [`scope-and-ambition.md`](scope-and-ambition.md) and [`runtime-semantics.md`](runtime-semantics.md). This doc records design moves the runtime made *because a worked example forced them*, not moves we set out to make ex ante. Updated when a new example crystallizes a decision the doctrine docs hadn't yet.

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

**Pattern for future examples:** when modelling *"which X is canonical for purpose Y at time Z?"* the first question is whether currentness alone is enough, or whether purpose-specific standing is needed. Examples 4 and 5 will likely both apply - period-closed accounting state has currentness *and* admissibility-for-purpose questions (current journal entry vs admissible-for-statutory-report).

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

**Future trigger:** a fourth or fifth example that finds this looseness too loose will force the introduction of typed predicate declarations - declaring that a predicate's *n*th argument is a subject identifying a specific claim kind. Until then, generic standing is the honest position. `scope-and-ambition.md` already lists *typed predicate declarations* as one of the four candidate language affordances for this reason.

## What deliberately stayed minimal

Each entry below was actively considered during one of the three examples and deferred. They are listed here so the reasoning survives the conversation.

| Considered | Why deferred |
|---|---|
| `ReliedOnBy(decision_id, claim_id)` decision-snapshot pattern | Example 3's simpler `require`-based design proved sufficient; the snapshot pattern adds a third "claims-about-claims-about-claims" layer that would only earn its keep when the simple model breaks. |
| `RevocationLifted` claim or grant-supersession to allow re-grant | Example 3 makes revocation terminal in v0. Re-granting after revocation would need a clear lifecycle model; deferred until a real example forces it. |
| Temporal qualification claims (`OccurredOn`, `EffectiveFor`, `KnownAsOf`) | Mentioned in `scope-and-ambition.md` as candidates for the as-of operator. No example yet needs them. |
| Actor context on transformations | Mentioned in `scope-and-ambition.md`. No example yet needs it. |
| Cascading retraction of historical decisions on standing revocation | Considered as option B for Example 3's design; rejected because it contradicts the "history is preserved" rule. |
| Sharing IR fixture helpers across example modules | The `morpholog_core::examples::*` modules each re-declare their own IR fixtures; they happen to use the same predicate names (e.g. `IndependentlyVerifiedRevenue` in Examples 2 and 3) without sharing constructor code. Keeps examples independent. |
| Per-example PostgreSQL schemas | All three examples share `crates/morpholog-core/sql/schema.sql`. The schema is canonical runtime infrastructure (`claims`, `audit`, `outbox`); examples differ in which predicates they admit into those tables, not in their storage shape. |

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

**Implication for future examples:** as-of is now reachable. The next natural pressure points:

- A bench scenario that surfaces the replay cost at long-audit-log scale. The sketch documents the O(transitions up to T) cost; until a real workload bites, materialisation is speculative.
- A CLI flag `--as-of <transition_id>` on the inspect subcommands. The adapter has the helpers; only argument parsing and threading remain.
- A worked example that combines as-of with effective-time claims (`EffectiveFor(subject, period)`), which is how the four temporal axes - event time, admission time, effective time, knowledge time - become accessible without polluting any schema with bitemporal flags.
- Write-path as-of (as a primitive inside invariants or transformations) remains explicitly deferred. The failure mode is much worse - a missed predicate in a write-path footprint analysis could let invalid commits through - so it gets its own forcing example and its own scrutiny when forced.
