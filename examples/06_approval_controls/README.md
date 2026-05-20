# Approval Controls

A business approves documents. Some approvals are simple sign-offs - a new vendor onboarded, a policy change ratified - and the only question is whether the person has the authority for that kind of document. Other approvals are about money - an invoice, a purchase order, an expense reimbursement - and the question becomes whether the person has authority *up to the amount* they are admitting. A real organisation needs both. So does this example.

The hard part is not the rules themselves; an organisation chart can describe them. The hard part is that three quarters from now, when an auditor or a regulator or a board director asks *who admitted this £4,300 payment, and under what authority, on what day, and is that authority still current?*, the answer should not require detective work. It should be one query against the audit log, with a definite answer that the runtime guaranteed at the moment the approval landed.

## The scenario

Two business stories run side by side in this example.

**Vendor onboarding** is a simple sign-off. The compliance officer holds the authority to approve new vendors of any size; the marketing intern does not. When the intern tries, the runtime refuses; when the compliance officer does, the durable record carries the compliance officer's identity as the approver. There is no amount involved.

**Invoice approval** is amount-sensitive. A junior accounts-payable clerk may approve invoices up to £1,000; a finance manager up to £10,000; the CFO up to £100,000. The £4,300 invoice that the junior tries to approve is refused; the £4,300 invoice the manager tries to approve admits. The durable record carries both the approver and the amount that was authorised - so an audit later can ask not just *who* approved but *up to what limit*.

Across both stories the same rule applies: revocation of authority prevents *future* approvals. It does not retroactively invalidate the historical record. The junior who is later promoted and then loses their clerk-level authority still has approved invoices in the books from the period when their authority was current. The history is preserved by construction; the rule is enforced by the runtime.

## The program

See [`approval_controls.morph`](approval_controls.morph) for the (illustrative) surface syntax.

### Claims

| Predicate | Role |
| --- | --- |
| `MayApprove(actor, doc_type)` | Unconditional authority. **Retractable.** |
| `ApprovalLimit(actor, doc_type, limit)` | Quantitative authority. **Retractable.** Multiple grants for the same `(actor, doc_type)` with different ceilings are allowed and stack - the require finds *some* satisfying grant. |
| `Approval(doc_id, doc_type, actor)` | Recorded unconditional approval. **Append-only.** Third arg is always the proposing actor of the transition that admitted it - the `assert` clause writes `$actor` there explicitly. |
| `LimitedApproval(doc_id, doc_type, amount, actor)` | Recorded quantitative approval. **Append-only.** The fourth arg is the proposing actor; the third is the amount that was authorised under whichever limit satisfied the require. |

### Invariants

None.

The absence is load-bearing. Tying a recorded `Approval`/`LimitedApproval` to a live `MayApprove`/`ApprovalLimit` via an invariant would force the runtime to either reject every revocation (because historical approvals now violate the rule) or cascade-retract those historical approvals (which destroys the record). Neither matches the business. The gate lives in `require`; admission is checked once, at the moment of the transition, and the record then stands.

This is the same lesson Examples 2 and 3 crystallised under different shapes. Carry it forward.

### Transformations

| Transformation | Effect |
| --- | --- |
| `grant_approval_authority(actor, doc_type)` | Asserts `MayApprove`. In v0 ungated; a real system would gate it on an administrative-authority claim about the proposing actor. |
| `revoke_approval_authority(actor, doc_type)` | Requires the authority to be currently held; retracts it. |
| `approve_document(doc_id, doc_type)` | The unconditional-authority approval. **Declares no `actor` parameter.** The proposing actor flows through transition context, is consulted via `$actor` in the require, and is stamped onto the asserted `Approval`. |
| `grant_approval_limit(actor, doc_type, limit)` | Asserts `ApprovalLimit`. Also ungated in v0. |
| `revoke_approval_limit(actor, doc_type, limit)` | Requires the specific `(actor, doc_type, limit)` triple; retracts it. |
| `approve_within_limit(doc_id, doc_type, amount)` | The quantitative-authority approval. Declares no `actor` parameter. The require is `ApprovalLimit($actor, doc_type, limit) and amount <= limit` - binds `limit` from the authority claim, then `Expr::Le` compares the proposed amount against it. Boundary equality is inclusive (amount == limit admits). |

## How to run it

### In-memory (sync kernel)

```bash
cargo test -p morpholog-core --test approval_controls
```

The in-memory tests cover both shapes plus the require-vs-invariant doctrine:

- **Unconditional path**: rejection without grant, asserted `Approval` carries the proposing actor, one actor cannot impersonate another, revocation preserves history.
- **Quantitative path**: rejection without grant, under-limit admits, **boundary equality admits** (the load-bearing test for `Expr::Le`), over-limit rejects, per-actor and per-doc-type scoping, stacked grants find the satisfying limit, revocation preserves history.
- **Kernel-level pins**: `Term::Actor` in an invariant body raises `UnboundActor` regardless of arg position; a non-decimal `limit` value in an admitted `ApprovalLimit` claim surfaces as `TypeMismatch`.

### Durable (PostgreSQL adapter)

```bash
DATABASE_URL=postgres:///morpholog_dev \
  cargo test -p morpholog-postgres --test integration -- --test-threads=1 \
    approval_controls_full_chain_through_pg
```

The PG test walks the whole story end-to-end through `propose_against_pg`: grant unconditional, approve, attempt impersonation (rejected), grant quantitative, approve within limit, attempt over-limit (rejected), revoke, confirm history survives.

---

## Design notes

### Typing assumption (an honest v0 doctrine point)

`Expr::Le` requires both operands to evaluate to `EvalValue::Decimal`. A non-decimal value in the `limit` position of an admitted `ApprovalLimit` claim makes the require's `Le` raise `EvalError::TypeMismatch` (surfaced from `propose_against_pg` as `PgError::Kernel(EvalError)`) - not `Rejected`. That is correct behaviour for a structurally-malformed claim: the runtime surfaces the corruption rather than papering over it.

The complete fix - rejecting non-decimal limits at admission time on `grant_approval_limit` - is the work of typed predicate declarations, which `docs/scope-and-ambition.md` already lists as a candidate language affordance. Until an example forces typed predicates, this example trusts its callers to admit decimal values.

### What this example deliberately does not cover

1. **Strict comparison (`<`, `>`, `>=`).** `Le` lands here. The others arrive when an example needs them.
2. **Cumulative limits across a window.** "May approve up to £10,000 total per day" needs `Sum` over a time-bounded subset of admitted state, plus a way to look up the audit log from inside a require. Out of scope.
3. **Administrative authority for granting.** `grant_approval_authority` and `grant_approval_limit` are both ungated. A real system has an administrative role that may grant; that becomes another `require MayGrant($actor, ...)` clause once the higher-order shape is justified.
4. **Predicate-pattern matching.** `ApprovalAuthorityFor(actor, predicate_pattern, limit)` - one authority claim governing a family of transformations - is the more general shape. Forces predicate names as first-class IR values. Deferred until forced.
5. **Delegation and segregation of duties.** Mentioned in [`docs/scope-and-ambition.md`](../../docs/scope-and-ambition.md); both await their own forcing examples.
