# Derived claims: design sketch

Status: design sketch for Example 5. Not a doctrine doc; records intent and open design questions before any kernel work lands. This sketch plus a spike test are PR #19; the implementation PR follows once the design questions below are answered.

## Problem

After PR #18 the runtime can durably enforce write-side legitimacy and inspect what was admitted, but the read side is bare. A caller who wants to ask "what is the balance of account X?" has to:

- pull all `JournalLine` claims for `X` out of `morpholog.claims`,
- sum the debits, sum the credits, subtract,
- repeat for every other account.

That is ad-hoc SQL in the caller. The same logic, in the same shape, exists in every accounting system that talks to a Morpholog ledger. The current state of the runtime makes that logic *possible* but does not make it *governed*. There is no way to say "trial balance is part of the model, and these are the rules that define it" alongside the invariants and transformations.

Derived claims are the smallest thing that lets a governed model carry its own read-side definitions. A derived claim is a named predicate whose extension is computed from admitted claims via an expression, rather than asserted directly by a transformation. It is admissible iff its defining expression holds against admitted state; it is enumerable iff the runtime can iterate over the variable bindings that make the body hold.

`scope-and-ambition.md` already names derived claims as one of the four candidate language affordances. Example 5 is the example that should force them.

## The smallest forcing example: trial balance over Example 4

The double-entry ledger from Example 4 already admits `JournalEntry` and `JournalLine` claims. The trial balance is the canonical derived view over them:

> For each account that appears on any journal line, the trial balance row is `(account, balance)` where `balance` is the sum of debits to that account minus the sum of credits.

Every accounting reader recognises this. The underlying state already exists in Example 4's tests. The derived view is the only missing piece. If Example 5 cannot express trial balance cleanly, the design is wrong.

A target syntax, illustrative only:

```
derived TrialBalanceRow(account, balance):
    balance == sum { d | JournalLine(_, account, d, _) }
             - sum { c | JournalLine(_, account, _, c) }
```

This composes existing IR primitives (`Sum`, `Claim`, `Eq`) plus one operation that the IR does not have today (`-` between two `Sum` expressions, or arithmetic in `Sum`'s value position). Which of those is the right shape is the first open design question below.

## Likely IR addition

The smallest plausible kernel addition:

```rust
pub struct DerivedClaim {
    pub predicate: String,         // e.g. "TrialBalanceRow"
    pub parameters: Vec<String>,   // e.g. ["account", "balance"]
    pub body: Expr,                // the defining expression
}
```

A `Program` would then carry derived claims alongside invariants and transformations:

```rust
pub struct Program {
    pub name: String,
    pub invariants: Vec<Invariant>,
    pub transformations: Vec<Transformation>,
    pub derived_claims: Vec<DerivedClaim>,  // new
}
```

An evaluation primitive on the kernel side, the shape of which is part of what the spike test should surface:

```rust
pub fn enumerate_derived(
    d: &DerivedClaim,
    state: &State,
) -> Result<Vec<ClaimInstance>, EvalError>;
```

A given derived claim's body is an expression over the same `Bindings` mechanic that invariants already use. Variables in the body that are not bound by an outer context are existentially quantified by `find_matches`, in exactly the same way they are inside an `Implies` right-hand side. The new thing is the *enumeration* outer loop: rather than asking "does this hold for some bindings?" the runtime asks "what are all the binding combinations that make this hold, projected onto the parameters?"

## Open design questions

These are the questions the implementation PR has to answer. The spike test in this PR is meant to surface them concretely.

1. **`Sum` value as expression vs new `Subtract` operator.** Trial balance's `balance == sum(debits) - sum(credits)` cannot be written today: `Expr::Eq(Box<Expr>, Box<Expr>)` works fine, `Expr::Sum` produces an `EvalValue::Decimal`, but there is no subtraction operator. Two plausible shapes:
   - Add `Expr::Sub(Box<Expr>, Box<Expr>)` and let the body be `Eq(Var("balance"), Sub(Sum(debits), Sum(credits)))`.
   - Extend `Sum.value` from `Term` to a small expression sublanguage so `sum { d - c | JournalLine(_, account, d, c) }` is one aggregation over signed amounts.
   The first is more general (subtraction is useful beyond aggregation), the second is more compact (one sum instead of two). The spike should make this choice visible.

2. **Enumeration domain.** Trial balance produces one row per *distinct account*. Where does "distinct account" come from? Options:
   - Implicit: the runtime infers the enumeration domain from the body's free variables and the predicates they appear in.
   - Explicit: the derived claim names its enumeration source (`for each account in JournalLine`).
   Implicit is cleaner if it works; explicit is more honest if the inference is non-obvious.

3. **Materialisation.** A derived claim's extension can be computed on demand or cached in a table. Example 5 should make derived claims *evaluable*; materialisation is a follow-on. The spike test should not assume materialised storage exists.

4. **Provenance.** When a derived claim row is read, can the runtime answer "which admitted claims contributed to this row?" Useful for audit; probably non-trivial; almost certainly post-Example-5.

5. **Recursion.** Can a derived claim's body reference another derived claim? `scope-and-ambition.md` says derived claims subsume "phase, balance, current pointer, report row" - the last two cases would benefit from layering (an aged-receivables row derived from a current-balance row derived from journal lines). Example 5 should probably ban recursion for now; revisit when forced.

6. **Naming.** `DerivedClaim` is the name in `scope-and-ambition.md`. Alternatives: `Projection`, `View`, `ComputedClaim`. The name should make the relationship to `Claim` clear: a `ClaimInstance` produced by evaluating a `DerivedClaim` is interchangeable with one admitted by a transformation, in terms of how invariants and other queries see it. `DerivedClaim` carries that hint; `View` does not. Defer the decision but lean `DerivedClaim`.

7. **As-of.** Derived claims over historical state needs the audit-log replay primitive that `scope-and-ambition.md` names. Out of scope for Example 5; current state only.

## What this PR delivers

- This document.
- One spike test in `crates/morpholog-core/tests/derived_claims_spike.rs`. Two tests actually: one that shows the *current* workaround (manual evaluator glue producing a trial balance with no IR support), and one that pins the *target* declarative form (marked `#[ignore]` with a panic body, so CI stays green; will become a real test once the implementation PR lands and the kernel supports derived claims).

This PR explicitly does *not* deliver:

- The IR addition (`DerivedClaim`, `Program::derived_claims`, evaluation primitive).
- A `derived_trial_balance()` constructor on the double-entry ledger module.
- Any CLI surface for derived claims (`morpholog inspect derived ...` is post-implementation).
- Any new IR variant for subtraction or signed-amount aggregation. The spike's "current workaround" test uses Rust code outside the IR to compute the balance; the design question of which kernel addition is the right one is left open for the implementation PR to answer.

## What the implementation PR will deliver

Conditional on the design questions above:

- A `DerivedClaim` struct in `morpholog-core` (likely shape per "Likely IR addition" above).
- Whichever of `Expr::Sub` or `Sum`-with-expression-value the spike forces.
- An evaluation primitive (`enumerate_derived` or similar).
- `Program::derived_claims` added.
- A `trial_balance_row()` constructor on `double_entry_ledger`.
- Promotion of the spike's target test to a real (un-ignored) test.
- Updated `forced-by-examples.md` recording what Example 5 forced.

That is the full Example 5 arc. The split between this sketch PR and the implementation PR is the standard project pattern (the same pattern that `postgres-persistence-v0.md` originally established for the PG adapter, modulo this doc's "sketch", not "design pin", framing). The point is to surface the design questions before committing kernel code to one shape.
