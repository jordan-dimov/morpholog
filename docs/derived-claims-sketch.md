# Derived claims: design sketch

Status: historical design sketch. PR #19 added this doc plus a spike test that pinned the target API. PR #20 implemented `DerivedClaim`, `DerivedValue`, `Expr::Sub`, `Program::derived_claims`, and `enumerate_derived`, and added `trial_balance_row()` to the double-entry-ledger example. The shape that landed matches the "revised shape (current lean)" below; the open design questions are now settled and recorded in `forced-by-examples.md` as part of the Example 5 retrospective. This document is preserved as the record of the design conversation that produced the implementation.

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

This section sketches two candidate shapes. The first is the naive minimum; the second is a refinement that emerged from review and is the current lean for the implementation PR. Both are recorded so the design history is visible.

### Naive shape (rejected after review)

The smallest plausible kernel addition:

```rust
pub struct DerivedClaim {
    pub predicate: String,         // e.g. "TrialBalanceRow"
    pub parameters: Vec<String>,   // e.g. ["account", "balance"]
    pub body: Expr,                // the defining expression
}
```

This was the first sketch. It is probably too weak because trial balance has two different kinds of output-position variables:

- `account` is an *enumerated* key. The runtime iterates over distinct accounts.
- `balance` is a *computed* value. The runtime evaluates a sum-of-debits-minus-sum-of-credits expression for each account.

A `parameters + body` shape treats both as the same kind of variable, which leaves the evaluator without a clear rule for which variables to enumerate and which to compute. The existing evaluator (`find_matches`) is admissibility-oriented: it checks whether expressions hold under bindings. Derived claims need to *construct* values, which is a different mode.

### Revised shape (current lean)

Separate the two kinds of parameters explicitly:

```rust
pub struct DerivedClaim {
    pub predicate: String,
    pub keys: Vec<String>,           // enumerated/grouping (e.g. ["account"])
    pub values: Vec<DerivedValue>,   // computed per-key (e.g. balance = ...)
    pub domain: Expr,                // enumerates distinct key bindings
}

pub struct DerivedValue {
    pub name: String,
    pub expr: Expr,
}
```

Semantics of `enumerate_derived`:

1. Evaluate `domain` to produce all bindings for key variables.
2. Project onto `keys` and deduplicate.
3. For each distinct key binding, evaluate each `DerivedValue.expr`.
4. Emit `ClaimInstance(predicate, key values ++ computed values)`.

For trial balance:

```text
predicate: "TrialBalanceRow"
keys:      ["account"]
values:    [{ name: "balance", expr: sum(debits) - sum(credits) }]
domain:    JournalLine(_, account, _, _)
```

Output: one `ClaimInstance` per distinct account, with `account` and `balance` as positional args.

The cost of this shape is one extra struct (`DerivedValue`) and one extra field on `DerivedClaim` (`domain`). The benefit is the evaluator has a clear, mechanical algorithm rather than having to invent one from the body expression.

### Program integration

```rust
pub struct Program {
    pub name: String,
    pub invariants: Vec<Invariant>,
    pub transformations: Vec<Transformation>,
    pub derived_claims: Vec<DerivedClaim>,  // new
}
```

And a kernel evaluation primitive:

```rust
pub fn enumerate_derived(
    d: &DerivedClaim,
    state: &State,
) -> Result<Vec<ClaimInstance>, EvalError>;
```

## What derived claims are NOT (in v0)

Worth pinning explicitly so the implementation PR does not drift into being something larger:

- **Not admitted state.** A `ClaimInstance` returned by `enumerate_derived` is a computed view, not a persisted assertion. It is not added to `State.claims`.
- **Not visible to invariants.** v0 invariants quantify over admitted claims only. Asking whether an invariant should be allowed to reference a derived claim is a real question, but it pulls in evaluation-order and recursion concerns that should wait for the second derived-claims example to force the shape.
- **Not visible to transformations.** v0 transformations cannot `require` a derived claim, cannot iterate over one in a `for` loop, and cannot assert one. Their world is admitted claims only.
- **Not persisted in PostgreSQL.** No new tables, no materialised views. The PG adapter's read API (`list_claims`, `list_audit_rows`, `list_pending_outbox`) is unchanged. Enumeration is on-demand against the in-memory `State` loaded from the adapter.
- **Not exposed via the CLI.** ~~`morpholog inspect derived <program> <name>` is a follow-on after the kernel work proves out.~~ This non-goal lapsed: the follow-on PR after Example 5 added `morpholog inspect derived <program> <name>` and a thin `list_derived` helper on the PostgreSQL adapter that recomputes the extension on each call. Still no materialised storage, no PG-side index, no recursion through derived claims, no invariant or transformation visibility.
- **Not recursive.** A derived claim's body cannot reference another derived claim. When a later example needs layered projections (an aged-receivables view computed from an account-balance view), the recursion semantics can be designed against that real pressure.
- **Not as-of.** Current state only.

Each of these is a real design question that some derived-claims system somewhere has had to answer. Deferring them is not a v0 limitation; it is the discipline that keeps Example 5 from becoming a projection framework.

## Open design questions

These are the questions the implementation PR has to answer. The spike test in this PR is meant to surface them concretely. Several have been narrowed since the first draft of this doc; the narrowed ones still get the open-question section because the implementation PR should confront them deliberately even if the current lean is clear.

1. **Subtraction primitive: lean `Expr::Sub`.** Trial balance's `balance == sum(debits) - sum(credits)` cannot be written with current IR primitives. Two candidates:
   - Add `Expr::Sub(Box<Expr>, Box<Expr>)` operating on decimal-valued expressions. `Eq(Var("balance"), Sub(Sum(debits), Sum(credits)))` is then expressible.
   - Extend `Sum.value` from `Term` to a small expression sublanguage so `sum { d - c | JournalLine(_, account, d, c) }` is one aggregation over signed amounts.

   The current lean is `Expr::Sub`. It is more general (arithmetic outside aggregation has obvious uses elsewhere: `net == gross - fees`, `variance == expected - actual`, etc.), it is the smaller semantic step (one new variant, decimal-on-decimal), and it does not require teaching `Sum` to host a sub-expression sublanguage. The implementation PR should add `Expr::Sub` only; no generic arithmetic tower, no multiplication/division, no `Sum`-expression extension. If a later example forces broader arithmetic, that is a separate move.

2. **Enumeration: lean explicit `domain` over implicit inference.** Trial balance produces one row per distinct account. Two candidates:
   - Implicit: the runtime infers the enumeration domain from the body's free variables and the predicates they appear in.
   - Explicit: the derived claim names its enumeration source as a separate field (the `domain: Expr` in the revised shape above).

   The current lean is explicit. Implicit inference sounds elegant until you confront the keys-vs-values distinction recorded in the "Likely IR addition" section: in trial balance, `account` is enumerated but `balance` is computed, and both are free in the body. An implicit-inference evaluator has no principled way to tell those apart. An explicit `domain` field (typically a `Claim` over the source predicate, with the key variables free and other positions wildcarded) makes the distinction mechanical: the domain enumerates keys; the value expressions consume them.

3. **Materialisation.** A derived claim's extension can be computed on demand or cached in a table. Example 5 should make derived claims *evaluable*; materialisation is a follow-on. The spike test should not assume materialised storage exists.

4. **Provenance.** When a derived claim row is read, can the runtime answer "which admitted claims contributed to this row?" Useful for audit; probably non-trivial; almost certainly post-Example-5.

5. **Recursion.** Can a derived claim's body reference another derived claim? `scope-and-ambition.md` says derived claims subsume "phase, balance, current pointer, report row" - the last two cases would benefit from layering (an aged-receivables row derived from a current-balance row derived from journal lines). Example 5 should ban recursion for now; revisit when a real example forces layered projection.

6. **Naming.** `DerivedClaim` is the name in `scope-and-ambition.md`. Alternatives: `Projection`, `View`, `ComputedClaim`. The current lean is `DerivedClaim`: the output shape (`ClaimInstance`) is the same as for admitted claims, which the name signals. `View` and `Projection` would suggest a separate datatype family. Defer firmly but lean `DerivedClaim`.

7. **Interchangeability with admitted state.** A `ClaimInstance` returned by `enumerate_derived` has the same shape as one admitted by a transformation. Should it have the same standing? v0 says no, recorded explicitly in the "What derived claims are NOT" section above: derived results enumerate to `ClaimInstance` values but are not added to `State.claims`, not visible to invariants, not visible to transformations, not persisted. The full interchangeability question (can invariants quantify over derived claims; can transformations require them) pulls in evaluation-order and recursion concerns that should wait until a second derived-claims example forces them.

8. **As-of.** Derived claims over historical state needs the audit-log replay primitive that `scope-and-ambition.md` names. Out of scope for Example 5; current state only.

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
