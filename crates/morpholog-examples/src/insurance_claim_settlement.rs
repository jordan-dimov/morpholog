//! Insurance claim settlement: an insurer authorises payments
//! against a policy's aggregate limit. Authorisation depends on
//! both an actor-authority gate and a per-policy spend cap, and
//! every payment must consume exactly its amount of the policy's
//! remaining headroom (the conservation invariant).
//!
//! See `examples/05_insurance_claim_settlement/README.md` for the
//! business framing.

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

/// Every `SettlementPaid(p, ...)` needs a current
/// `PolicyHeadroom(p, _)`. Closes a gap in
/// `headroom_consumed_by_payment`: the conservation rule is
/// vacuously true when no headroom claim exists for the policy.
pub fn paid_implies_headroom() -> Invariant {
    Invariant {
        name: "paid_implies_headroom".to_string(),
        version: 1,
        body: implies(
            claim(
                "SettlementPaid",
                vec![var("p"), wildcard(), wildcard(), wildcard()],
            ),
            exists("r", claim("PolicyHeadroom", vec![var("p"), var("r")])),
        ),
    }
}

/// At most one `PolicyHeadroom` per policy. `authorise_settlement`'s
/// `bind_one` lookup relies on this; two competing headroom claims
/// would also mean two answers to "how much capacity remains?".
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

/// Per-policy headroom delta conservation: the change in
/// `PolicyHeadroom` must equal the total of newly-admitted
/// `SettlementPaid` amounts for that policy. The `aggregate_limit`
/// require is an admission gate ("is there enough?"); this is the
/// conservation law ("did the delta match?"). A transformation
/// that admitted `SettlementPaid` without retracting and
/// re-asserting `PolicyHeadroom` would pass the require and fail
/// this invariant.
///
/// Sum-based rather than per-row (`after = before - amt` for each
/// new payment) because the per-row form is too weak: a
/// multi-payment transition admitting two same-amount settlements
/// while decrementing headroom once would pass each individual
/// equation while consuming twice the headroom it credits.
pub fn headroom_consumed_by_payment() -> Invariant {
    Invariant {
        name: "headroom_consumed_by_payment".to_string(),
        version: 1,
        body: implies(
            and(vec![
                claim("PolicyHeadroom", vec![var("p"), var("after")]),
                pre(claim("PolicyHeadroom", vec![var("p"), var("before")])),
            ]),
            eq(
                term(var("after")),
                sub(
                    term(var("before")),
                    sum(
                        var("amt"),
                        "amt",
                        and(vec![
                            claim(
                                "SettlementPaid",
                                vec![var("p"), wildcard(), var("s"), var("amt")],
                            ),
                            not(pre(claim(
                                "SettlementPaid",
                                vec![var("p"), wildcard(), var("s"), var("amt")],
                            ))),
                        ]),
                    ),
                ),
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

/// Open a policy. Admits the contractual `Policy` and the initial
/// `PolicyHeadroom` (= aggregate_limit; no settlement has consumed
/// it yet). The structural-uniqueness invariants catch duplicate
/// admissions.
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

/// The main transformation. Looks up the claim, the policy, and
/// the current headroom; checks the actor has enough authority and
/// the policy has enough capacity; then updates the headroom and
/// admits the authorisation and payment claims. The proposing
/// actor flows through transition context, not as a parameter.
pub fn authorise_settlement() -> Transformation {
    Transformation {
        name: "authorise_settlement".to_string(),
        parameters: params(&["claim_id", "settlement_id", "amount"]),
        body: vec![
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
            // Admission gate: is there enough headroom? The
            // `headroom_consumed_by_payment` invariant separately
            // verifies that the post-state delta matches the payment.
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
        paid_implies_headroom(),
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
/// serve different consumers - one for reporting, the other for
/// the conservation invariant `headroom_consumed_by_payment`.
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
