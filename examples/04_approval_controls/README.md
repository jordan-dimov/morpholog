# Approval Controls

A business approves documents. Some approvals are simple sign-offs - vendor onboarding, policy changes - and the only question is whether the person has authority for that kind of document. Others are about money - invoices, purchase orders, expense reimbursements - and the question becomes whether the person has authority *up to the amount*. A real organisation needs both.

Three quarters from now, when an auditor asks *who admitted this £4,300 payment, and under what authority, on what day, and is that authority still current?*, the answer should be one query against the audit log, not detective work.

## The scenario

Every document goes on the record before anyone approves it: its kind, and if it is about money, its amount. The approver names only the document. Which approval route applies, and what amount the ceiling is measured against, come from the record, never from the approver.

**Vendor onboarding** is a simple sign-off. The compliance officer holds the authority; the marketing intern does not. When the intern tries, the runtime refuses. When the compliance officer does, the durable record carries the compliance officer's identity. No amount is involved.

**Invoice approval** is amount-sensitive. A junior clerk may approve invoices up to £1,000; a finance manager up to £10,000; the CFO up to £100,000. The £4,300 invoice the junior tries is refused; the £4,300 the manager tries admits. The durable record carries both the approver and the amount that was authorised. Because the invoice was registered with its amount, the sign-off route is closed to it: holding unconditional authority for invoices does not let anyone approve this one without its ceiling.

Across both: revocation of authority prevents *future* approvals. It does not invalidate the historical record. The junior who is later promoted and loses their clerk-level authority still has approved invoices in the books from the period when their authority was current.

## The program

See [`approval_controls.morph`](approval_controls.morph) for the surface syntax.

### Claims

| Predicate | Role |
| --- | --- |
| `MayApprove(actor, doc_type)` | Unconditional authority. Retractable. |
| `ApprovalLimit(actor, doc_type, limit)` | Quantitative authority. Retractable. Multiple grants for the same `(actor, doc_type)` stack - the require finds *some* satisfying grant. |
| `Approval(doc_id, doc_type, actor)` | Recorded unconditional approval. Append-only. Third arg is the proposing actor. |
| `LimitedApproval(doc_id, doc_type, amount, actor)` | Recorded quantitative approval. Append-only. Fourth arg is the proposing actor; third is the authorised amount. |
| `Document(doc_id, doc_type)` | A registered document and its kind. One kind per document, append-only. |
| `DocumentAmount(doc_id, amount)` | The amount of a monetary document. One amount per document, append-only. Its presence is what sends a document down the limited route. |

### Invariants

Document identity is constrained by invariants; approval authority is not.

A document id determines one kind and, where it has one, one amount: the two `unique by (doc_id)` clauses. These are lasting properties of the document.

Approval authority is different. It is checked when an approval is proposed. Tying recorded approvals to live authority with an invariant would force the runtime either to refuse every revocation, because past approvals would break the rule, or to erase those approvals. Neither matches the business. The gate lives in `require`, checked once at the moment of approval, and the record stands. Same lesson as the verified-revenue example.

### Transformations

| Transformation | Effect |
| --- | --- |
| `grant_approval_authority(principal, doc_type)` | Asserts `MayApprove` for the principal, the person receiving authority (the proposer is the `actor`). Refused if that authority is already held. |
| `revoke_approval_authority(principal, doc_type)` | Requires the authority to be currently held; retracts it. |
| `submit_document(doc_id, doc_type)` | Registers a document with no amount. Refused if the id is already on the record. |
| `submit_monetary_document(doc_id, doc_type, amount)` | Registers a document with a positive amount. Refused if the id is already on the record, so a document cannot gain or lose an amount after it is registered. |
| `approve_document(doc_id)` | The unconditional approval. Reads the kind from the record and refuses a document with an amount, or one the actor has already approved. **Declares no `actor` parameter.** The proposing actor comes from the transition, is consulted as `actor` in the require, and is stamped onto the asserted `Approval`. |
| `grant_approval_limit(principal, doc_type, limit)` | Asserts `ApprovalLimit`. Refused if that exact ceiling is already granted. |
| `revoke_approval_limit(principal, doc_type, limit)` | Requires the specific `(principal, doc_type, limit)` triple; retracts it. |
| `approve_within_limit(doc_id)` | The quantitative approval. Reads the kind and amount from the record and refuses a document the actor has already approved. No `actor` parameter. Require: `ApprovalLimit(actor, doc_type, limit) and amount <= limit` - binds `limit` from the authority claim; the `<=` comparison checks the recorded amount. Boundary equality is inclusive. |

## How to run it

```bash
# In-memory
cargo test -p morpholog-examples --test approval_controls

# Durable
DATABASE_URL=postgres:///morpholog_dev \
  cargo test -p morpholog-postgres --test integration -- --test-threads=1 \
    approval_controls_full_chain_through_pg
```

In-memory tests cover both shapes, the rule that revoking authority preserves the history of decisions made under it, boundary equality on amount limits, scoping (per-actor, per-doc-type), stacked grants, the route the record decides (a monetary document refused the sign-off route; no later submission adding or dropping an amount), and runtime safety pins on the proposing-actor reference and non-decimal limits. The PG integration test walks the whole story end to end.

---

## Design notes

### Typing assumption

The `<=` comparison requires both operands to evaluate to `EvalValue::Decimal`. A non-decimal value in the `limit` position of an admitted `ApprovalLimit` makes the require raise `TypeMismatch`, not a business rejection. That is correct: a structurally-malformed claim is corruption, not lawful rejection.

Typed predicate declarations and `morpholog check` catch the *static* form of this mistake: a non-decimal literal in the `limit` slot, or a variable whose kind conflicts with the `Decimal` declaration, fails validation before the programme runs. At runtime, `propose --args-named` checks each argument against the kind its parameter is used as; the lower-level tagged codec, `--args`, does not, so a subject passed where a decimal belongs surfaces there as `TypeMismatch` at evaluation.

### The record decides the route

A document with an amount can only be approved against a ceiling. `MayApprove(jordan, invoice)` and `ApprovalLimit(jordan, invoice, 1000)` can coexist, and jordan still cannot approve a £4,300 invoice through `approve_document`: the invoice's recorded amount closes that route. The approver chooses which document to approve, never the kind or amount that decide whether they may.

That holds for documents as registered. The example assumes the submitting system records each document's true kind and amount: register a £4,300 invoice with no amount, and the record will treat it as a sign-off. Authenticating the source and checking the registration against the paper invoice are separate controls. Amounts are positive here; credit notes and refunds would need approval rules of their own.

### What this example deliberately does not cover

- **Cumulative limits across a window.** "Up to £10,000 total per day" needs `Sum` over a time-bounded subset plus audit-log lookup from inside a require.
- **Who is acting.** The example governs approval authority, not who the approver is. It takes the actor the proposing application names; nothing here stops one application naming another approver. [Example 13](../13_biometric_identification_oversight/) shows how to restrict which database logins may speak for which names.
- **Administrative authority for granting and submitting.** The grant and submission transformations are ungated; a real system gates them on authority of their own, such as `MayGrant(actor, ...)`, once that shape is justified.
- **Predicate-pattern matching.** `ApprovalAuthorityFor(actor, predicate_pattern, limit)` - one claim governing a family of transformations. Deferred until forced.
- **Delegation and segregation of duties.** Mentioned in [`docs/scope-and-ambition.md`](../../docs/scope-and-ambition.md); both await their forcing examples.
