# Approval Controls

A business approves documents. Some approvals are simple sign-offs - vendor onboarding, policy changes - and the only question is whether the person has authority for that kind of document. Others are about money - invoices, purchase orders, expense reimbursements - and the question becomes whether the person has authority *up to the amount*. A real organisation needs both.

Three quarters from now, when an auditor asks *who admitted this £4,300 payment, and under what authority, on what day, and is that authority still current?*, the answer should be one query against the audit log, not detective work.

## The scenario

**Vendor onboarding** is a simple sign-off. The compliance officer holds the authority; the marketing intern does not. When the intern tries, the runtime refuses. When the compliance officer does, the durable record carries the compliance officer's identity. No amount is involved.

**Invoice approval** is amount-sensitive. A junior clerk may approve invoices up to £1,000; a finance manager up to £10,000; the CFO up to £100,000. The £4,300 invoice the junior tries is refused; the £4,300 the manager tries admits. The durable record carries both the approver and the amount that was authorised.

Across both: revocation of authority prevents *future* approvals. It does not invalidate the historical record. The junior who is later promoted and loses their clerk-level authority still has approved invoices in the books from the period when their authority was current.

## The program

See [`approval_controls.morph`](approval_controls.morph) for the illustrative surface syntax.

### Claims

| Predicate | Role |
| --- | --- |
| `MayApprove(actor, doc_type)` | Unconditional authority. Retractable. |
| `ApprovalLimit(actor, doc_type, limit)` | Quantitative authority. Retractable. Multiple grants for the same `(actor, doc_type)` stack - the require finds *some* satisfying grant. |
| `Approval(doc_id, doc_type, actor)` | Recorded unconditional approval. Append-only. Third arg is the proposing actor. |
| `LimitedApproval(doc_id, doc_type, amount, actor)` | Recorded quantitative approval. Append-only. Fourth arg is the proposing actor; third is the authorised amount. |

### Invariants

None. The absence is load-bearing.

Tying recorded approvals to live authority via an invariant would force the runtime to either reject every revocation (because historical approvals would violate the rule) or cascade-retract those historical approvals. Neither matches the business. The gate lives in `require`; admission is checked once, at the moment of the transition; the record stands. Same lesson as the verified-revenue example, applied here.

### Transformations

| Transformation | Effect |
| --- | --- |
| `grant_approval_authority(actor, doc_type)` | Asserts `MayApprove`. Ungated in v0. |
| `revoke_approval_authority(actor, doc_type)` | Requires the authority to be currently held; retracts it. |
| `approve_document(doc_id, doc_type)` | The unconditional approval. **Declares no `actor` parameter.** The proposing actor flows through transition context, is consulted via `$actor` in the require, and is stamped onto the asserted `Approval`. |
| `grant_approval_limit(actor, doc_type, limit)` | Asserts `ApprovalLimit`. Ungated. |
| `revoke_approval_limit(actor, doc_type, limit)` | Requires the specific `(actor, doc_type, limit)` triple; retracts it. |
| `approve_within_limit(doc_id, doc_type, amount)` | The quantitative approval. No `actor` parameter. Require: `ApprovalLimit($actor, doc_type, limit) and amount <= limit` - binds `limit` from the authority claim; `Expr::Le` compares the proposed amount. Boundary equality is inclusive. |

## How to run it

```bash
# In-memory
cargo test -p morpholog-examples --test approval_controls

# Durable
DATABASE_URL=postgres:///morpholog_dev \
  cargo test -p morpholog-postgres --test integration -- --test-threads=1 \
    approval_controls_full_chain_through_pg
```

In-memory tests cover both shapes, the rule that revoking authority preserves the history of decisions made under it, boundary equality on amount limits, scoping (per-actor, per-doc-type), stacked grants, and runtime safety pins on the proposing-actor reference and non-decimal limits. The PG integration test walks the whole story end to end.

---

## Design notes

### Typing assumption

`Expr::Le` requires both operands to evaluate to `EvalValue::Decimal`. A non-decimal value in the `limit` position of an admitted `ApprovalLimit` makes the require's `Le` raise `TypeMismatch`, not a business rejection. That is correct: a structurally-malformed claim is corruption, not lawful rejection.

The complete fix - rejecting non-decimal limits at admission time on `grant_approval_limit` - is the work of typed predicate declarations, deferred until forced. Until then this example trusts its callers to admit decimal values.

### Routing policy is conventional, not enforced

The example uses `MayApprove` for non-monetary documents and `ApprovalLimit` for monetary ones. This is the right convention.

The runtime does **not** enforce mutual exclusion. Both `MayApprove(jordan, invoice)` and `ApprovalLimit(jordan, invoice, 1000)` could coexist; jordan could then approve an invoice for any amount by routing through `approve_document` (no amount parameter), bypassing the limit. The v0 honest position.

A complete policy model would either add typed document classes (so a `doc_type` declares which approval shape applies) or unify both shapes under a higher-order `ApprovalPolicy(actor, predicate_pattern, limit_or_unbounded)`. Both await their forcing examples. Until then, routing discipline is deployment convention.

### What this example deliberately does not cover

- **Strict comparison (`<`, `>`, `>=`).** `Le` lands here; the others arrive when an example needs them.
- **Cumulative limits across a window.** "Up to £10,000 total per day" needs `Sum` over a time-bounded subset plus audit-log lookup from inside a require.
- **Administrative authority for granting.** Both grant transformations are ungated; a real system gates them on `MayGrant($actor, ...)` once the higher-order shape is justified.
- **Predicate-pattern matching.** `ApprovalAuthorityFor(actor, predicate_pattern, limit)` - one claim governing a family of transformations. Deferred until forced.
- **Delegation and segregation of duties.** Mentioned in [`docs/scope-and-ambition.md`](../../docs/scope-and-ambition.md); both await their forcing examples.
