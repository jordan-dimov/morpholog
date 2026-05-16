# What the first three examples forced into the language

Status: retrospective. Companion to [`scope-and-ambition.md`](scope-and-ambition.md) and [`runtime-semantics.md`](runtime-semantics.md). This doc records design moves the runtime made *because a worked example forced them*, not moves we set out to make ex ante. Updated when a new example crystallizes a decision the doctrine docs hadn't yet.

## Why this doc exists

The project's pacing discipline — *smallest possible increment that produces a working artefact* — means design decisions are deferred until concrete pressure forces them. The corresponding risk: when pressure does come, the response gets rationalized after-the-fact as "what we always meant." This doc tries to keep the actual record honest. For each significant decision, it answers three questions:

- **What forced this?** Which example, and what was the concrete pressure.
- **What did we consider and reject?** The credible alternatives, and why we did not pick them.
- **What does this mean for future examples?** The pattern the decision sets and where it is likely to apply or break next.

The doc now covers Examples 1–4 (settlement netting, revenue restatement, claim standing, double-entry ledger with period close). It is extended each time a worked example crystallizes a decision the forward-looking doctrine docs hadn't yet pinned.

## Decisions forced by the examples

### `Value::Subject` IR variant

**Forced by:** Example 3 — claim standing.

**The pressure:** purposes (`bank_debt_service`, `investor_reporting`) needed to appear as constant references in invariant bodies and transformation `require` clauses. The pre-Example-3 IR had `Value::Decimal(String)` only; constant subjects had no representation.

**The alternative we considered and rejected:** pass purpose as a transformation parameter the caller already knows. Functional, but ceremonial *and bug-prone* — the caller could pass the wrong purpose to a purpose-specific transformation, e.g. `subj("investor_reporting")` to `admit_debt_service_revenue`, and the transformation would silently admit a `DebtServiceRevenue` claim against the wrong standing.

**The change:** added `Value::Subject(String)` to the `Value` enum; matched in `unify_args` (against `EvalValue::Subject`) and `resolve_term` (returning `EvalValue::Subject`). Two unit tests pin the contract. ~30 lines of kernel change; one new IR variant. Smallest possible change that closes the gap.

**Pattern for future examples:** if a transformation needs to embed a specific constant subject in its body (a status name, a fixed authority, an enumerated category), it can now be expressed as `Term::Literal(Value::Subject(...))` rather than as a caller-supplied parameter. If a constant of a *different* kind shows up (e.g. a boolean literal, a fixed datetime), the same pattern applies — add the variant when forced, not before.

### The `require`-vs-`invariant` semantic distinction

**Forced by:** Example 3 — claim standing.

**The pressure:** the natural-sounding rule *"a `DebtServiceRevenue` claim implies an active `AdmissibleFor`"* turned out to be an invariant trap. If written as an invariant, revoking standing later would either:
- reject the revoke (breaking the rule that standing can be lost), or
- force cascade-retraction of every historical decision that relied on it (breaking the rule that history is preserved).

Neither matched the real-world semantics we wanted: *a calculation made under valid standing at time T stays valid even if standing is revoked at T + 1.*

**The resolution:** `require` is the *admission gate* — checked at the moment of admission, and only then. `invariant` is the *eternal rule* — must always hold against admitted state. The two are not interchangeable; they answer different questions. Example 3 uses `require AdmissibleFor(...)` on decision transformations and reserves invariants for the standing claims' own internal consistency (`admissibility_has_provenance`, `admissibility_excludes_revocation`).

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

**Pattern for future examples:** when modelling *"which X is canonical for purpose Y at time Z?"* the first question is whether currentness alone is enough, or whether purpose-specific standing is needed. Examples 4 and 5 will likely both apply — period-closed accounting state has currentness *and* admissibility-for-purpose questions (current journal entry vs admissible-for-statutory-report).

### History-as-append-only

**Forced by:** Examples 2 and 3.

**The pattern:**

| Pattern | Predicates that fit |
|---|---|
| Append-only history | `BankRecognisedRevenue`, `IndependentlyVerifiedRevenue`, `StandingGrantedBy`, `DebtServiceRevenue`, `InvestorReportedRevenue` |
| Retractable pointers / admissibility | `CurrentBankRecognition`, `AdmissibleFor` |
| Append-only superseding / revocation | `Supersedes(new, old)`, `StandingRevoked` |

The discipline: *content* claims (what was admitted) are append-only; *pointer* and *admissibility* claims are retractable; *lineage* and *revocation* claims are append-only again.

**Pattern for future examples:** in any new example, classify each predicate into one of these three buckets before writing transformations. Only the middle bucket may be retracted. This is a useful design check — if a transformation wants to retract something in bucket one or three, the model probably has the wrong shape.

### Generic standing on any subject (Example 3's deferred typed-predicate question)

**Forced by:** Example 3 — claim standing.

**The behaviour:** `grant_standing` will admit `AdmissibleFor(any_subject, any_purpose)` even if the named subject has no underlying claim of any predicate. The decision transformation later catches "no underlying IV" via its own `require IndependentlyVerifiedRevenue(...)`.

**Why this is intentional in v0:** the runtime has no predicate type system that would let `grant_standing` enforce *"the supplied subject names a verification claim, not something else."* `AdmissibleFor` is a generic relation by design — the same shape applies to verifications, journal entries, curve snapshots, audit artefacts, valuation reports, and other claim kinds we may add later.

**Future trigger:** a fourth or fifth example that finds this looseness too loose will force the introduction of typed predicate declarations — declaring that a predicate's *n*th argument is a subject identifying a specific claim kind. Until then, generic standing is the honest position. `scope-and-ambition.md` already lists *typed predicate declarations* as one of the four candidate language affordances for this reason.

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

## What this means for Example 4 and beyond

If the next worked example is double-entry ledger with period close (as outlined in `scope-and-ambition.md`'s roadmap), some predictions:

- **Likely to reuse:** the require-vs-invariant distinction (period close is admission gating); history-as-append-only (journal entries are content; period-closed status is a pointer / admissibility; restatement lineage is `Supersedes`); generic standing (a posted journal may have multiple admissibilities — statutory, tax, management).
- **Likely to stretch:** the *require* semantics — period-close transformations need to reject postings dated within the closed period via require, and the same period cannot be "un-closed" without going through a new restatement transformation. This is the same shape as standing revocation being terminal in v0.
- **Likely to introduce:** sum-of-balances invariants under the existing `Expr::Sum`. The "debits and credits balance per posted entry" rule is exactly this shape.
- **Likely to defer:** typed predicate declarations, derived claims (trial balance as projection), actor context, as-of evaluation. Each is an "if forced" affordance from `scope-and-ambition.md` — Example 4 alone probably does not force them.

If those predictions hold, Example 4 reuses the affordances Examples 1–3 already forced. If a new affordance is forced, this document gains another section.

### Update — confirmed by Example 4

The predictions above were written before Example 4 was implemented. Both the in-memory and durable proofs landed in the same PR and **all four predictions held**. No new IR primitive was forced.

Specifics:

- **Balance is `Eq(Sum, Sum)` with no new aggregation primitive.** The fundamental accounting equation — `sum { d | JournalLine(entry, _, d, _) } == sum { c | JournalLine(entry, _, _, c) }` — composes directly with existing IR variants. `eval_value` already handles `Expr::Sum` (returning `EvalValue::Decimal`); `Expr::Eq` evaluates both sides through `eval_value`; the comparison is decimal-to-decimal equality. Confirmed by the `unbalanced_entry_rejected_by_invariant` test, which catches a 5-unit credit shortfall on candidate state.
- **Period close is admission-gating via `require not PeriodClosed(period)`** in the posting transformations. No invariant ties `JournalEntry` to `PeriodClosed`, which means closing a period does not invalidate historical entries (the same lesson as the require-vs-invariant section above). Confirmed by `closed_period_rejects_normal_posting` and by `restatement_into_closed_period_preserves_original`.
- **Restatement reuses `Supersedes`** from Example 2 with no shape changes — `Supersedes(new_entry_id, prior_entry_id)` works for journal entries exactly as for revenue verifications. The `at_most_one_direct_successor` invariant is re-declared in `double_entry_ledger` but is structurally identical to the Example 2 version.
- **The three-bucket append-only / retractable / append-only classification holds completely** for Example 4: every predicate is content (`JournalEntry`, `JournalLine`), terminal state (`PeriodClosed`), or lineage (`Supersedes`). No retractable pointers were needed — callers walk the `Supersedes` chain instead of consulting a current pointer.

The clean reuse outcome is itself informative: the accumulated affordances from Examples 1–3 are sufficient to express a textbook accounting workflow. The next semantic frontier — *derived claims* for read-side projections like trial balance and account-balance lookups — is what Example 5 will push on. That is where new pressure is expected to surface; Examples 1–4 give a stable baseline for the write/admission boundary, but "stable baseline" is not the same as "finished," and later examples may yet stress admission via derived state or as-of evaluation in ways the current shape cannot express.
