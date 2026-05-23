//! Insurance claim settlement IR: actor authority + cumulative
//! aggregate policy limit. The load-bearing transformation gates
//! authorisation on `Le(Add(running_paid, proposed), aggregate_limit)`,
//! which is the IR shape Expr::Add was added for. See
//! `examples/05_insurance_claim_settlement/README.md` for the business
//! framing.

use morpholog_core::{Invariant, Transformation};

use morpholog_core::dsl::*;

// ============================================================
// Invariants
// ============================================================

/// Every `SettlementPaid(_, claim, settlement, amount)` must be
/// backed by a `SettlementAuthorised(claim, settlement, amount, _)`
/// record. The transformation never asserts one without the other,
/// but the runtime contract is "candidate state is admissible under
/// invariants," so the gap is closed explicitly. Actor identity on
/// the authorisation is wildcarded - the invariant cares about the
/// authorising record existing, not who recorded it.
pub fn paid_implies_authorised() -> Invariant {
    Invariant {
        name: "paid_implies_authorised".to_string(),
        version: 1,
        body: implies(
            claim(
                "SettlementPaid",
                vec![wildcard(), var("c"), var("s"), var("a")],
            ),
            claim(
                "SettlementAuthorised",
                vec![var("c"), var("s"), var("a"), wildcard()],
            ),
        ),
    }
}

/// At most one `Policy` per `policy_id`. The eternal structural rule
/// that `authorise_settlement`'s `bind_one Policy(policy_id,
/// aggregate_limit)` implicitly depends on - without it a duplicate-
/// policy admission would surface as `bind_one matched 2 candidates`
/// (a kernel error) rather than a lawful business rejection. Reuses
/// the singleton-shape from
/// `verified_revenue::at_most_one_current_verification_per_asset_period`.
pub fn at_most_one_policy_per_id() -> Invariant {
    Invariant {
        name: "at_most_one_policy_per_id".to_string(),
        version: 1,
        body: implies(
            and(vec![
                claim("Policy", vec![var("policy"), var("a")]),
                claim("Policy", vec![var("policy"), var("b")]),
            ]),
            eq(term(var("a")), term(var("b"))),
        ),
    }
}

/// At most one `ClaimReported` per `claim_id`. Mirrors
/// `at_most_one_policy_per_id`; pins the structural uniqueness that
/// `authorise_settlement`'s `bind_one ClaimReported(claim_id,
/// policy_id, _)` depends on. Duplicate reports must agree on every
/// field.
pub fn at_most_one_claim_report_per_id() -> Invariant {
    Invariant {
        name: "at_most_one_claim_report_per_id".to_string(),
        version: 1,
        body: implies(
            and(vec![
                claim(
                    "ClaimReported",
                    vec![var("c"), var("policy_a"), var("amount_a")],
                ),
                claim(
                    "ClaimReported",
                    vec![var("c"), var("policy_b"), var("amount_b")],
                ),
            ]),
            and(vec![
                eq(term(var("policy_a")), term(var("policy_b"))),
                eq(term(var("amount_a")), term(var("amount_b"))),
            ]),
        ),
    }
}

/// At most one `PolicyHeadroom` per `policy_id`. Mirrors
/// `at_most_one_policy_per_id`; pins the structural uniqueness that
/// the payment transformation's `bind_one PolicyHeadroom(policy_id,
/// remaining)` (added in the conservation-invariant follow-up) will
/// rely on. Even before any transformation reads PolicyHeadroom via
/// `bind_one`, the uniqueness invariant matters: an unexpected
/// duplicate at admission time would mean two competing answers to
/// "how much capacity remains?" - exactly the kind of ambiguity a
/// governed model exists to forbid.
pub fn at_most_one_headroom_per_policy() -> Invariant {
    Invariant {
        name: "at_most_one_headroom_per_policy".to_string(),
        version: 1,
        body: implies(
            and(vec![
                claim("PolicyHeadroom", vec![var("policy"), var("a")]),
                claim("PolicyHeadroom", vec![var("policy"), var("b")]),
            ]),
            eq(term(var("a")), term(var("b"))),
        ),
    }
}

/// Transition invariant: each newly-admitted `SettlementPaid` claim
/// for a policy must consume the corresponding decrement from that
/// policy's `PolicyHeadroom`. Reads as: "if a `PolicyHeadroom`
/// exists both before and after, AND a new `SettlementPaid` was
/// admitted in this transition for that policy with amount `amt`,
/// then the post-state remaining = pre-state remaining - `amt`."
///
/// This is the conservation rule the `aggregate_limit` `require`
/// gate alone could not enforce. The `require` is an admission
/// gate ("is there enough headroom for this payment?"); this
/// invariant is the conservation law ("did the payment actually
/// consume exactly its amount of headroom?"). Both are kept: a
/// buggy transformation that admitted `SettlementPaid` without
/// retracting and re-asserting `PolicyHeadroom` would pass the
/// require but fail this invariant, surfacing the bug at admission.
///
/// Genesis behaviour: `issue_policy` admits the initial
/// `PolicyHeadroom` against an empty pre-state. The `pre(PolicyHeadroom
/// (p, before))` conjunct matches nothing, the whole `and` is
/// empty, and the rule is vacuously true under `implies`. Once the
/// policy exists, every subsequent settlement is constrained.
///
/// Quantifier composition note: the body uses `and` to thread `p`
/// across the post-state `PolicyHeadroom`, pre-state
/// `PolicyHeadroom`, and the newly-admitted `SettlementPaid`. There
/// is no `forall`; the binding-extension semantics of `And` produce
/// one binding set per (p, after, before, s, amt) tuple and the
/// `implies` must hold for each. Multiple newly-admitted settlements
/// for the same policy in a single transition (if a buggy
/// transformation ever produced them) would each be constrained
/// against the same `(before, after)` pair, which would only
/// succeed if every amount were identical - effectively forbidding
/// multi-settlement transitions per policy.
pub fn headroom_consumed_by_payment() -> Invariant {
    Invariant {
        name: "headroom_consumed_by_payment".to_string(),
        version: 1,
        body: implies(
            and(vec![
                claim("PolicyHeadroom", vec![var("p"), var("after")]),
                pre(claim("PolicyHeadroom", vec![var("p"), var("before")])),
                claim(
                    "SettlementPaid",
                    vec![var("p"), wildcard(), var("s"), var("amt")],
                ),
                not(pre(claim(
                    "SettlementPaid",
                    vec![var("p"), wildcard(), var("s"), var("amt")],
                ))),
            ]),
            eq(
                term(var("after")),
                sub(term(var("before")), term(var("amt"))),
            ),
        ),
    }
}

/// `settlement_id` is globally unique across admitted payments. Two
/// `SettlementPaid` claims sharing a `settlement_id` must agree on
/// every other field. Without this, an audit log could carry two
/// distinct payments under the same id - the money side stays
/// conservative (cumulative cap counts both), but the identity side
/// gets muddy. For an audit-grade settlement example, identity
/// uniqueness pulls its weight.
pub fn settlement_id_uniquely_identifies_payment() -> Invariant {
    Invariant {
        name: "settlement_id_uniquely_identifies_payment".to_string(),
        version: 1,
        body: implies(
            and(vec![
                claim(
                    "SettlementPaid",
                    vec![var("policy_a"), var("claim_a"), var("s"), var("amount_a")],
                ),
                claim(
                    "SettlementPaid",
                    vec![var("policy_b"), var("claim_b"), var("s"), var("amount_b")],
                ),
            ]),
            and(vec![
                eq(term(var("policy_a")), term(var("policy_b"))),
                eq(term(var("claim_a")), term(var("claim_b"))),
                eq(term(var("amount_a")), term(var("amount_b"))),
            ]),
        ),
    }
}

// ============================================================
// Transformations
// ============================================================

/// Open a policy with an aggregate limit. Admits two claims at
/// once: the immutable `Policy(policy_id, aggregate_limit)` record,
/// and an initial `PolicyHeadroom(policy_id, aggregate_limit)` -
/// the operational remaining-capacity counter that settlements
/// consume. At issuance the headroom equals the aggregate limit; no
/// settlement has reduced it yet.
///
/// The distinction matters: `Policy` is the contractual cap (set
/// once, never changes), `PolicyHeadroom` is operational state
/// (decremented per settlement). A duplicate `policy_id` admission
/// is caught by `at_most_one_policy_per_id` and
/// `at_most_one_headroom_per_policy` against the candidate state.
pub fn issue_policy() -> Transformation {
    Transformation {
        name: "issue_policy".to_string(),
        parameters: params(&["policy_id", "aggregate_limit"]),
        body: vec![
            assert_("Policy", vec![var("policy_id"), var("aggregate_limit")]),
            assert_(
                "PolicyHeadroom",
                vec![var("policy_id"), var("aggregate_limit")],
            ),
            emit("PolicyIssued", vec![var("policy_id")]),
        ],
    }
}

/// Record a reported loss against a policy. Requires the policy to
/// exist; the claimed amount is informational - the binding
/// authorisations make against the policy aggregate limit, not the
/// claimed amount. A duplicate `claim_id` admission is caught by
/// `at_most_one_claim_report_per_id` against the candidate state -
/// `authorise_settlement` later relies on this uniqueness through
/// `bind_one ClaimReported(claim_id, policy_id, _)`.
pub fn report_claim() -> Transformation {
    Transformation {
        name: "report_claim".to_string(),
        parameters: params(&["claim_id", "policy_id", "claimed_amount"]),
        body: vec![
            require(claim("Policy", vec![var("policy_id"), wildcard()])),
            assert_(
                "ClaimReported",
                vec![var("claim_id"), var("policy_id"), var("claimed_amount")],
            ),
            emit("ClaimReported", vec![var("claim_id")]),
        ],
    }
}

/// Grant settlement authority to an actor up to `limit`. Unconditional
/// in v0; granting authority itself is treated as out of scope, in
/// the same way `grant_approval_limit` is unconditional in the
/// approval-controls example.
pub fn grant_settlement_authority() -> Transformation {
    Transformation {
        name: "grant_settlement_authority".to_string(),
        parameters: params(&["principal", "limit"]),
        body: vec![
            assert_("SettlementAuthority", vec![var("principal"), var("limit")]),
            emit(
                "SettlementAuthorityGranted",
                vec![var("principal"), var("limit")],
            ),
        ],
    }
}

/// The load-bearing transformation. Pulls the claim's policy_id and
/// the policy's aggregate_limit into bindings via `Stmt::BindOne`,
/// then gates settlement authorisation on:
///
/// 1. The proposing actor has settlement authority covering the
///    proposed amount (`actor_limit` bound by the authority claim,
///    `amount <= actor_limit`).
/// 2. Cumulative paid settlements on this policy plus the proposed
///    amount do not exceed the policy aggregate. Encoded as
///    `Le(Add(Sum(paid), amount), aggregate_limit)` - this is the
///    `Expr::Add` shape the example forces.
///
/// On admission, asserts both `SettlementAuthorised` (who decided
/// what) and `SettlementPaid` (the payment record the cumulative
/// running total reads from) and emits a payment-request intent for
/// the outbox.
/// No actor parameter on the transformation; the actor flows through
/// transition context as `actor()`, persisted to the
/// authorisation record.
///
/// Each `bind_one` looks up a uniquely-matching claim and binds its
/// values into the surrounding context. `at_most_one_claim_report_per_id`
/// and `at_most_one_policy_per_id` are the structural-uniqueness
/// invariants that make these lookups safe: without them a duplicate
/// admission would surface as `bind_one matched 2 candidates`
/// (kernel error) rather than a lawful business rejection.
pub fn authorise_settlement() -> Transformation {
    Transformation {
        name: "authorise_settlement".to_string(),
        parameters: params(&["claim_id", "settlement_id", "amount"]),
        body: vec![
            // Unique-lookup chain: pull `policy_id` from the claim
            // report, then `aggregate_limit` from the policy, then
            // `current_headroom` from the operational counter. Each
            // bind_one rejects if zero matches (no such claim
            // reported / no such policy / no headroom on file) and
            // surfaces a kernel error if multiple matches (programme
            // bug - structural-uniqueness invariants exist precisely
            // to prevent this).
            bind_one(claim(
                "ClaimReported",
                vec![var("claim_id"), var("policy_id"), wildcard()],
            )),
            bind_one(claim(
                "Policy",
                vec![var("policy_id"), var("aggregate_limit")],
            )),
            bind_one(claim(
                "PolicyHeadroom",
                vec![var("policy_id"), var("current_headroom")],
            )),
            require(and(vec![
                claim("SettlementAuthority", vec![actor(), var("actor_limit")]),
                le(term(var("amount")), term(var("actor_limit"))),
            ])),
            // Admission-gate cumulative-cap rule. Kept alongside the
            // post-state `headroom_consumed_by_payment` invariant:
            // one is an authorisation gate ("is there enough headroom
            // for this payment?"), the other is conservation ("did
            // the payment actually consume exactly its amount?"). A
            // buggy transformation that staged SettlementPaid without
            // touching PolicyHeadroom would pass this require and
            // fail the invariant.
            require(le(
                add(
                    sum(
                        var("paid"),
                        "paid",
                        claim(
                            "SettlementPaid",
                            vec![var("policy_id"), wildcard(), wildcard(), var("paid")],
                        ),
                    ),
                    term(var("amount")),
                ),
                term(var("aggregate_limit")),
            )),
            // Compute the new headroom and stage the retract+assert
            // pair that conservation requires. The order matters
            // structurally only: retract before assert keeps the
            // intermediate state coherent under
            // at_most_one_headroom_per_policy if a future trace
            // ever inspects it; the kernel commits the whole staged
            // set atomically.
            let_(
                "new_headroom",
                sub(term(var("current_headroom")), term(var("amount"))),
            ),
            retract(
                "PolicyHeadroom",
                vec![var("policy_id"), var("current_headroom")],
            ),
            assert_(
                "PolicyHeadroom",
                vec![var("policy_id"), var("new_headroom")],
            ),
            assert_(
                "SettlementAuthorised",
                vec![
                    var("claim_id"),
                    var("settlement_id"),
                    var("amount"),
                    actor(),
                ],
            ),
            assert_(
                "SettlementPaid",
                vec![
                    var("policy_id"),
                    var("claim_id"),
                    var("settlement_id"),
                    var("amount"),
                ],
            ),
            emit(
                "ClaimPaymentRequested",
                vec![var("settlement_id"), var("amount"), actor()],
            ),
        ],
    }
}

pub fn all_predicates() -> Vec<morpholog_core::PredicateDecl> {
    vec![
        predicate("Policy")
            .subject("policy_id")
            .decimal("aggregate_limit")
            .build(),
        predicate("ClaimReported")
            .subject("claim_id")
            .subject("policy_id")
            .decimal("claimed_amount")
            .build(),
        predicate("SettlementAuthority")
            .subject("actor")
            .decimal("limit")
            .build(),
        predicate("SettlementAuthorised")
            .subject("claim_id")
            .subject("settlement_id")
            .decimal("amount")
            .subject("actor")
            .build(),
        predicate("SettlementPaid")
            .subject("policy_id")
            .subject("claim_id")
            .subject("settlement_id")
            .decimal("amount")
            .build(),
        predicate("PolicyHeadroom")
            .subject("policy_id")
            .decimal("remaining")
            .build(),
        predicate("PolicyLimitUsage")
            .subject("policy_id")
            .decimal("used")
            .build(),
    ]
}

pub fn all_invariants() -> Vec<Invariant> {
    vec![
        paid_implies_authorised(),
        at_most_one_policy_per_id(),
        at_most_one_claim_report_per_id(),
        at_most_one_headroom_per_policy(),
        settlement_id_uniquely_identifies_payment(),
        headroom_consumed_by_payment(),
    ]
}

/// Read-side reporting projection: cumulative paid per policy as a
/// sum over `SettlementPaid` claims. Enumerated on demand via
/// `enumerate_derived`; not added to admitted state, not visible to
/// invariants or transformations, not persisted.
///
/// Distinct in role from the admitted `PolicyHeadroom(policy_id,
/// remaining)` predicate: `PolicyLimitUsage` is a recomputed-from-
/// history view (how much has been spent), `PolicyHeadroom` is
/// operational admitted state (how much remains). Both are derivable
/// from each other (`used = aggregate_limit - remaining`), but they
/// serve different consumers - one for reporting, one for the
/// conservation invariant a later step will enforce.
pub fn policy_limit_usage() -> morpholog_core::DerivedClaim {
    morpholog_core::DerivedClaim {
        predicate: "PolicyLimitUsage".to_string(),
        keys: vec!["policy_id".to_string()],
        values: vec![morpholog_core::DerivedValue {
            name: "used".to_string(),
            expr: sum(
                var("paid"),
                "paid",
                claim(
                    "SettlementPaid",
                    vec![var("policy_id"), wildcard(), wildcard(), var("paid")],
                ),
            ),
        }],
        domain: claim(
            "SettlementPaid",
            vec![var("policy_id"), wildcard(), wildcard(), wildcard()],
        ),
    }
}

/// The insurance-claim-settlement example as a
/// [`morpholog_core::Program`]. Stable identifier:
/// `"insurance_claim_settlement"`.
pub fn program() -> morpholog_core::Program {
    morpholog_core::Program {
        name: "insurance_claim_settlement".to_string(),
        predicates: all_predicates(),
        invariants: all_invariants(),
        transformations: vec![
            issue_policy(),
            report_claim(),
            grant_settlement_authority(),
            authorise_settlement(),
        ],
        derived_claims: vec![policy_limit_usage()],
    }
}
