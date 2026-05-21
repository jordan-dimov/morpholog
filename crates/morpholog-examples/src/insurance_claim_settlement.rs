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

/// Open a policy with an aggregate limit. The aggregate limit is the
/// cumulative cap across every settlement on this policy. A
/// duplicate `policy_id` admission is caught by
/// `at_most_one_policy_per_id` against the candidate state -
/// `authorise_settlement` later relies on this uniqueness through
/// `bind_one Policy(policy_id, aggregate_limit)`.
pub fn issue_policy() -> Transformation {
    Transformation {
        name: "issue_policy".to_string(),
        parameters: params(&["policy_id", "aggregate_limit"]),
        body: vec![
            assert_("Policy", vec![var("policy_id"), var("aggregate_limit")]),
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
        parameters: params(&["actor", "limit"]),
        body: vec![
            assert_("SettlementAuthority", vec![var("actor"), var("limit")]),
            emit(
                "SettlementAuthorityGranted",
                vec![var("actor"), var("limit")],
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
            // Unique-lookup pair: pull `policy_id` from the claim
            // report, then `aggregate_limit` from the policy. Each
            // bind_one rejects if zero matches (no such claim
            // reported / no such policy) and surfaces a kernel
            // error if multiple matches (programme bug -
            // structural-uniqueness invariants exist precisely to
            // prevent this).
            bind_one(claim(
                "ClaimReported",
                vec![var("claim_id"), var("policy_id"), wildcard()],
            )),
            bind_one(claim(
                "Policy",
                vec![var("policy_id"), var("aggregate_limit")],
            )),
            require(and(vec![
                claim("SettlementAuthority", vec![actor(), var("actor_limit")]),
                le(term(var("amount")), term(var("actor_limit"))),
            ])),
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

pub fn all_invariants() -> Vec<Invariant> {
    vec![
        paid_implies_authorised(),
        at_most_one_policy_per_id(),
        at_most_one_claim_report_per_id(),
        settlement_id_uniquely_identifies_payment(),
    ]
}

/// Read-side projection: cumulative paid per policy. Enumerated on
/// demand via `enumerate_derived`; not added to admitted state, not
/// visible to invariants or transformations, not persisted.
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
