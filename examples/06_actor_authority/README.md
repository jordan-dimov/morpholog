# Actor Authority

The sixth worked example. The smallest model that earns the IR's `Term::Actor` consultation primitive: a transformation that admits a record only if the *proposing actor* holds authority for the *kind of record* being proposed.

## The scenario

A business approves documents - invoices, contracts, change orders, expense claims. Approving a document is not just a click; it is a transition that commits the organisation to whatever the document says. The runtime's job is to refuse approvals from people the business has not authorised, and to make the durable record of every approval answer "who, exactly, did this?" - by name, three quarters from now, when a regulator asks.

Most systems answer this with a permissions table, a middleware check, an `approved_by` column, and a comment somewhere that the audit log "should" capture the user. Morpholog answers it with one IR primitive (`Term::Actor`), one `require`, and one `assert` - reading the actor *from the transition itself*, not from a parameter the caller could mismatch.

The contract that comes out of it: the actor on every committed `Approval` claim is the actor of the transition that committed it. There is no other way to put one there.

## The program

See [`actor_authority.morph`](actor_authority.morph) for the (illustrative) surface syntax.

### Claims

| Predicate | Role |
| --- | --- |
| `MayApprove(actor, doc_type)` | Authority claim. Acquired by `grant_approval_authority`, lost by `revoke_approval_authority`. **Retractable.** |
| `Approval(doc_id, doc_type, actor)` | Recorded approval. **Append-only.** The third argument is *always* the proposing actor of the transition that admitted it - the `assert` clause writes `$actor` there explicitly. |

### Invariants

None.

The absence is load-bearing. The natural-sounding rule "every `Approval` implies the actor still has `MayApprove`" is the same invariant trap [`docs/forced-by-examples.md`](../../docs/forced-by-examples.md) records for Example 3. Encoded as an invariant, revoking authority later would force either:

- rejection of the revocation (because every historical `Approval` now breaks the rule), or
- cascade-retraction of every historical `Approval` admitted under the now-revoked authority (which destroys the record).

Neither matches the business: a document approved on June 30 stays approved even if the approver leaves the company on July 1. The legitimacy of a past decision was established when it was made; revoking authority prevents *future* decisions, not past ones. So the gate lives in `require`, not in an invariant. The require check fires at admission time and is never re-checked against admitted state.

This is the same lesson Example 3 (claim standing) crystallised, applied to a different shape of authority.

### Transformations

| Transformation | Effect |
| --- | --- |
| `grant_approval_authority(actor, doc_type)` | Asserts `MayApprove(actor, doc_type)`. In v0 this is ungated - any caller can grant authority to any subject. A real system would gate this on an administrative authority claim about the proposing actor; that follow-on lands once approval limits and predicate-pattern matching arrive. |
| `revoke_approval_authority(actor, doc_type)` | Requires the authority to be currently held; retracts it. Historical `Approval` claims under that authority survive. |
| `approve_document(doc_id, doc_type)` | The key transformation. **Declares no `actor` parameter.** The actor flows through transition context and is read via `$actor` (`Term::Actor` in the IR). `require MayApprove($actor, doc_type)` consults the *proposing* actor; `assert Approval(doc_id, doc_type, $actor)` stamps that same actor onto the durable record. |

The shape of `approve_document` is what this example exists to demonstrate: a domain transformation whose argument list contains only domain payload (which document, which type), with the actor reached from the surrounding transition context. The caller cannot pass the wrong actor; there is no actor parameter to pass.

## How to run it

The same scenario is proven at two layers - in-memory through the sync kernel, and durably through the PostgreSQL adapter.

### In-memory (sync kernel)

```bash
cargo test -p morpholog-core --test actor_authority
```

In-memory tests:

1. **`approve_without_authority_is_rejected_at_require`** - empty pre-state, jordan proposes an approval. `require MayApprove($actor, ...)` matches nothing in pre-state. Rejected with `require failed`. No claims, no audit, no intent.
2. **`approve_with_authority_admits_approval_carrying_proposing_actor`** - grant authority, then approve. The asserted `Approval` claim has the proposing actor as its third argument.
3. **`approve_uses_proposing_actor_not_a_caller_parameter`** - the load-bearing test. jordan has authority for invoices; alice does not. alice proposes the same approval jordan would. The `require MayApprove($actor, doc_type)` resolves `$actor` to alice, not jordan, and fails - even though jordan is right there in the pre-state. The actor is the *transition's*, not a parameter the caller controls.
4. **`revoked_authority_blocks_future_but_preserves_past`** - the require-vs-invariant payoff. jordan approves a document; her authority is revoked; the historical `Approval` survives in admitted state; a fresh approval attempt by jordan is rejected.
5. **`term_actor_in_invariant_body_surfaces_as_unbound_actor`** - the IR enforcing doctrine. An invariant body that references `Term::Actor` fails evaluation with `EvalError::UnboundActor`. Invariants have no transition in scope; authority checks belong in `require`, not in invariants. This makes the doctrine catchable, not conventional.

### Durable (PostgreSQL adapter)

```bash
DATABASE_URL=postgres:///morpholog_dev \
  cargo test -p morpholog-postgres --test integration -- --test-threads=1 \
    actor_authority_full_chain_through_pg
```

The integration test exercises every step of the in-memory scenarios end-to-end through `propose_against_pg`, asserting that:

- the `actor` field on `PgProposalOutcome::Committed` echoes the proposing actor;
- the audit row's `actor` column persists the same actor;
- the durable `Approval` claim carries the proposing actor as its third argument;
- alice's unauthorised attempt is rejected, writes no audit row, and enqueues no intent;
- after revocation, jordan's earlier approval is still queryable from the durable claim set, but a fresh approval attempt is rejected.

The shared schema (`claims`, `audit`, `outbox`) is unchanged from earlier examples; the `audit.actor` column was added in the PR that landed `Transition.actor`.

---

## Design notes

### What this example proves about the doctrine

- **The actor is transition context, not a transformation parameter.** `approve_document` declares no `actor` parameter; the actor is reached from `$actor`. This keeps domain payload free of cross-cutting authority plumbing.
- **`Term::Actor` is rejected in invariant bodies.** The IR makes the require-vs-invariant lesson enforceable, not conventional. An invariant that reaches for the actor gets `EvalError::UnboundActor` and the program does not run.
- **Authority is acquired and lost without touching the durable record of past decisions.** No invariant ties `Approval` to live `MayApprove`. The pattern matches Examples 2 and 3.

### What this example deliberately does not cover

1. **Approval limits.** A real authority claim might say "may approve invoices up to $50,000." That requires decimal comparison (`<=`) in the expression language, which the IR does not yet have. The next worked example that genuinely needs this is the right forcing function; the comparison primitive lands then, not before.
2. **Predicate-pattern matching.** `ApprovalAuthorityFor(actor, predicate_pattern, limit)` - "may approve any claim whose predicate matches this pattern" - is the more general shape. It needs the IR to take predicate names as first-class values. Deferred until an example forces it.
3. **Administrative authority for granting.** `grant_approval_authority` is currently ungated. A real system has an administrative role that may grant authority; the proposing actor of the *grant* transformation should itself have authority to grant. The natural extension is another `require MayGrant($actor, doc_type)` clause once the higher-order shape is justified by an example.
4. **Delegation.** `DelegatedBy(delegate, delegator, scope)` and the consequent invariants are mentioned in [`docs/scope-and-ambition.md`](../../docs/scope-and-ambition.md) as the eventual shape. Out of scope here.
5. **Segregation of duties.** "The actor who proposed X cannot also approve X" is a richer rule that consults both the proposing actor and the audit log. Out of scope here; a worked example for this would force a different IR primitive (reading the actor of *another* transition by id).
6. **Lineage between user-proposed transitions and runtime-initiated ones.** The outbox compensation path proposes its compensating transition under `system_actor()` ("morpholog-system"). The audit log records this faithfully but does not yet model the lineage "this system transition compensates that user transition" as a first-class concept. Deferred until a real example shows the loss.

### Where this fits in the arc

Example 5 (the trial-balance derived claim, hosted inside Example 4) closed out the read-side primitives. The actor PR that preceded this example plumbed actor identity through the runtime as transition context; this example is the forcing function for the *consultation* primitive (`Term::Actor`). The next forced step in the authority arc is approval limits, which will force decimal comparison.
