# Approval Limits

The seventh worked example. Extends [Example 6 (actor authority)](../06_actor_authority/) with *quantitative* authority: an actor may approve documents of a kind *up to a limit*. Forces the IR's first decimal-comparison primitive (`Expr::Le`).

## The scenario

A bank's invoice approval policy reads, in human terms: a clerk may approve invoices up to £1,000; a manager, up to £10,000; a director, up to £100,000. Anyone may *propose* an invoice for approval, but only an actor whose granted ceiling covers the invoice amount may admit it. When the invoice for £4,300 lands, the clerk's attempt is refused; the manager's clears; the director's would have cleared too. Either way, the audit log answers who admitted what and *under what ceiling* - the limit is preserved as a separate admitted claim, not embedded in the `Approval` record.

Most systems do this with role hierarchies, threshold tables, and approval workflows. Morpholog does it with one new IR primitive (`Expr::Le`), one authority claim (`ApprovalLimit`), and one `require` that binds the limit from the claim and compares it to the proposed amount.

## The program

See [`approval_limits.morph`](approval_limits.morph) for the (illustrative) surface syntax.

### Claims

| Predicate | Role |
| --- | --- |
| `ApprovalLimit(actor, doc_type, limit)` | Quantitative authority claim. The actor may approve documents of `doc_type` for amounts `<= limit`. **Retractable.** Multiple grants for the same `(actor, doc_type)` with different ceilings are allowed and stack (the `approve_within_limit` require finds *any* satisfying grant). |
| `LimitedApproval(doc_id, doc_type, amount, actor)` | Recorded approval. **Append-only.** The fourth argument is *always* the proposing actor of the transition that admitted it - the `assert` clause writes `$actor` there explicitly. |

### Invariants

None. The same reasoning as [Example 6](../06_actor_authority/) applies: tying a recorded `LimitedApproval` to a live `ApprovalLimit` via an invariant would either reject revocations or cascade-retract historical approvals. A document approved on June 30 stays approved even if the approver's ceiling is cut on July 1.

### Transformations

| Transformation | Effect |
| --- | --- |
| `grant_approval_limit(actor, doc_type, limit)` | Asserts `ApprovalLimit(actor, doc_type, limit)`. In v0, ungated - any caller can grant a limit to any subject. A real system would gate this on an administrative-authority claim about the proposing actor. |
| `revoke_approval_limit(actor, doc_type, limit)` | Requires the specific `(actor, doc_type, limit)` triple; retracts it. Historical approvals under that grant survive. |
| `approve_within_limit(doc_id, doc_type, amount)` | The load-bearing transformation. Declares no `actor` parameter. Requires `ApprovalLimit($actor, doc_type, limit) and amount <= limit` - the And binds `limit` from the authority claim, then the new `Expr::Le` compares the proposed `amount` against it. Asserts `LimitedApproval` with the proposing actor stamped onto the durable record. |

The shape of the require is what this example exists to demonstrate. `Expr::Le` is the smallest possible step that lets the language say "this much, not more" without smuggling the comparison through equality games or an aggregation primitive.

## How to run it

Both layers, as for every other worked example.

### In-memory (sync kernel)

```bash
cargo test -p morpholog-core --test approval_limits
```

In-memory tests:

1. **`approval_without_limit_grant_is_rejected`** - empty pre-state, the require's ApprovalLimit lookup finds nothing, the And short-circuits, the proposal is rejected.
2. **`approval_under_limit_commits_with_actor_and_amount`** - grant 1000, approve 750, `LimitedApproval` carries `(doc_id, doc_type, 750, jordan)`.
3. **`approval_exactly_at_limit_commits`** - the boundary test. `Expr::Le` is inclusive; an amount equal to the limit admits.
4. **`approval_above_limit_is_rejected`** - grant 1000, approve 1001, the Le returns no satisfying binding and the require fails.
5. **`limit_grant_is_per_actor`** - jordan has a limit; alice does not. alice cannot approve even a small amount.
6. **`limit_grant_is_per_doc_type`** - jordan has an invoice limit but not a contract limit; the contract approval is rejected.
7. **`multiple_grants_take_the_satisfying_one`** - jordan holds both a £500 and a £5,000 grant. A £3,000 approval admits via the higher grant. The require's existential bind on `limit` finds *some* satisfying value.
8. **`revoking_a_limit_blocks_future_but_preserves_past`** - the require-vs-invariant payoff again. A revoked grant retracts the authority going forward; every historical `LimitedApproval` survives in admitted state.

### Durable (PostgreSQL adapter)

```bash
DATABASE_URL=postgres:///morpholog_dev \
  cargo test -p morpholog-postgres --test integration -- --test-threads=1 \
    approval_limits_full_chain_through_pg
```

The PG test exercises the chain end-to-end through `propose_against_pg`: grant, under-limit approval, over-limit rejection. It asserts that the proposing actor lands on the audit row, on the commit receipt, and on the durable `LimitedApproval` claim's fourth argument.

---

## Design notes

### What this example forces

- **`Expr::Le(Box<Expr>, Box<Expr>)`** - decimal less-than-or-equal. Predicate-shaped: returns the binding set unchanged when true, the empty set when false. Both operands must evaluate to `EvalValue::Decimal`; anything else surfaces as `EvalError::TypeMismatch`.
- The single comparison primitive added in this PR. `Lt`, `Gt`, `Ge` are not yet here. Each lands when an example forces it; for approval ceilings, `<=` is the natural shape (you may approve up to and including the limit) and is sufficient.

### Typing assumption (a real v0 doctrine point)

`approve_within_limit` evaluates `amount <= limit` against the binding for `limit` produced by the `ApprovalLimit` claim. **`Expr::Le` requires both operands to evaluate to `EvalValue::Decimal`.** If either is a different kind (a subject, a bool, a collection), the require raises `EvalError::TypeMismatch`, which surfaces from `propose_against_pg` as `PgError::Kernel(EvalError)` - **not** as `PgProposalOutcome::Rejected`.

That distinction matters. A non-decimal `limit` admitted into `ApprovalLimit` is *structural corruption* of the model, not lawful business rejection. The runtime is honest about which it is: a malformed claim that was admitted (perhaps before the program had its current shape, perhaps by direct insertion bypassing the kernel) makes some admission gates fail loudly rather than silently misbehave. The pinning test `non_decimal_limit_in_authority_claim_surfaces_as_type_mismatch` makes this explicit.

A complete fix - rejecting non-decimal limits *at admission time* on the `grant_approval_limit` transformation - is the work of **typed predicate declarations**, which `docs/scope-and-ambition.md` already lists as a candidate language affordance. Until an example forces typed predicates, this example assumes its callers admit decimal values for `amount` and `limit`, and treats violations as kernel errors rather than business rejections.

### What this example deliberately does not cover

1. **Strict comparison (`<`, `>`, `>=`).** Add when an example needs them. A "must be strictly under daily exposure cap" rule, for instance.
2. **Cumulative limits across a window.** "May approve up to £10,000 total per day" requires summing past approvals and comparing - that pulls in `Sum` over a time-bounded subset and a richer require, plus a way to look up the audit log from inside an admission gate (likely via a derived claim). Out of scope here.
3. **Tiered authorities.** "Clerk approves up to 1k, manager 10k, director 100k" is expressible as separate `ApprovalLimit` grants per role; no new primitive needed. A real system might want to enforce that an actor is granted at most one limit per `(doc_type, role)`, which would be an invariant on the authority record (not on the approvals). Deferred.
4. **Predicate-pattern matching.** "May approve any claim of this shape up to N" - one `ApprovalAuthorityFor(actor, predicate_pattern, limit)` claim governing a family of transformations - is the more general shape. Forces predicate names as first-class IR values. Deferred until forced.
5. **Multi-signature approval.** "Two approvers, each within their limit, jointly authorise" introduces the audit log as a queryable input to admission. A separate example would force the right primitive for that.

### Where this fits in the arc

Example 6 added the consultation primitive (`Term::Actor`) and demonstrated unconditional authority. Example 7 extends the same arc with the comparison primitive (`Expr::Le`) and demonstrates quantitative authority. The next forced step is whichever business shape demands one of the deferred items above - likely cumulative limits or predicate-pattern matching, depending on the worked example that earns them.
