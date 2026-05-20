//! Insurance claim settlement IR: actor authority + cumulative
//! aggregate policy limit. The load-bearing transformation gates
//! authorisation on `Le(Add(running_paid, proposed), aggregate_limit)`,
//! which is the IR shape Expr::Add was added for. See
//! `examples/05_insurance_claim_settlement/README.md` for the business
//! framing.

use morpholog_core::{Invariant, Term, Transformation};

use crate::helpers::*;

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
                vec![Term::Wildcard, var("c"), var("s"), var("a")],
            ),
            claim(
                "SettlementAuthorised",
                vec![var("c"), var("s"), var("a"), Term::Wildcard],
            ),
        ),
    }
}

// ============================================================
// Transformations
// ============================================================

/// Open a policy with an aggregate limit. The aggregate limit is the
/// cumulative cap across every settlement on this policy. Unconditional
/// in v0; policy administration (issuance authority, endorsements,
/// cancellation) is out of scope for this example.
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
/// claimed amount.
pub fn report_claim() -> Transformation {
    Transformation {
        name: "report_claim".to_string(),
        parameters: params(&["claim_id", "policy_id", "claimed_amount"]),
        body: vec![
            require(claim("Policy", vec![var("policy_id"), Term::Wildcard])),
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
/// the policy's aggregate_limit into bindings via `ValueOf`, then
/// gates settlement authorisation on:
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
/// what) and `SettlementPaid` (the payment fact for the cumulative
/// running total) and emits a payment-request intent for the outbox.
/// No actor parameter on the transformation; the actor flows through
/// transition context as `Term::Actor`, persisted to the
/// authorisation record.
///
/// A `require` gates on existence of the reported claim before
/// `ValueOf` extracts `policy_id`. `Stmt::Require` is a yes/no
/// predicate gate that does not propagate its match's bindings back
/// into the active scope, so the value extraction has to happen
/// separately via `Let` + `ValueOf` (same pattern settlement_netting
/// uses for `LineAmount`). The Policy ValueOf does not need its own
/// existence-require because `report_claim` already requires the
/// policy to exist; a `ClaimReported` implies its `Policy`.
pub fn authorise_settlement() -> Transformation {
    Transformation {
        name: "authorise_settlement".to_string(),
        parameters: params(&["claim_id", "settlement_id", "amount"]),
        body: vec![
            require(claim(
                "ClaimReported",
                vec![var("claim_id"), Term::Wildcard, Term::Wildcard],
            )),
            let_(
                "policy_id",
                value_of(
                    "ClaimReported",
                    vec![var("claim_id"), Term::Wildcard, Term::Wildcard],
                ),
            ),
            let_(
                "aggregate_limit",
                value_of("Policy", vec![var("policy_id"), Term::Wildcard]),
            ),
            require(and(vec![
                claim("SettlementAuthority", vec![Term::Actor, var("actor_limit")]),
                le(term(var("amount")), term(var("actor_limit"))),
            ])),
            require(le(
                add(
                    sum(
                        var("paid"),
                        "paid",
                        claim(
                            "SettlementPaid",
                            vec![
                                var("policy_id"),
                                Term::Wildcard,
                                Term::Wildcard,
                                var("paid"),
                            ],
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
                    Term::Actor,
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
                vec![var("settlement_id"), var("amount"), Term::Actor],
            ),
        ],
    }
}

pub fn all_invariants() -> Vec<Invariant> {
    vec![paid_implies_authorised()]
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
                    vec![
                        var("policy_id"),
                        Term::Wildcard,
                        Term::Wildcard,
                        var("paid"),
                    ],
                ),
            ),
        }],
        domain: claim(
            "SettlementPaid",
            vec![
                var("policy_id"),
                Term::Wildcard,
                Term::Wildcard,
                Term::Wildcard,
            ],
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
