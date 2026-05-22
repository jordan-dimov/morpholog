//! Approval controls IR: unconditional `MayApprove` + quantitative
//! `ApprovalLimit`, both consulted via `actor()` in admission-time
//! `require` clauses. No invariants - revocation prevents future
//! approvals but historical approvals stay admitted. See
//! `examples/04_approval_controls/README.md` for the business framing.

use morpholog_core::{Invariant, Transformation};

use morpholog_core::dsl::*;

// ============================================================
// Unconditional authority - MayApprove + approve_document
// ============================================================

pub fn grant_approval_authority() -> Transformation {
    Transformation {
        name: "grant_approval_authority".to_string(),
        parameters: params(&["principal", "doc_type"]),
        body: vec![
            assert_("MayApprove", vec![var("principal"), var("doc_type")]),
            emit(
                "ApprovalAuthorityGranted",
                vec![var("principal"), var("doc_type")],
            ),
        ],
    }
}

pub fn revoke_approval_authority() -> Transformation {
    Transformation {
        name: "revoke_approval_authority".to_string(),
        parameters: params(&["principal", "doc_type"]),
        body: vec![
            require(claim("MayApprove", vec![var("principal"), var("doc_type")])),
            retract("MayApprove", vec![var("principal"), var("doc_type")]),
            emit(
                "ApprovalAuthorityRevoked",
                vec![var("principal"), var("doc_type")],
            ),
        ],
    }
}

/// Unconditional approval. Declares no `actor` parameter; the actor
/// flows through transition context and is consulted via `actor()`.
pub fn approve_document() -> Transformation {
    Transformation {
        name: "approve_document".to_string(),
        parameters: params(&["doc_id", "doc_type"]),
        body: vec![
            require(claim("MayApprove", vec![actor(), var("doc_type")])),
            assert_("Approval", vec![var("doc_id"), var("doc_type"), actor()]),
            emit("DocumentApproved", vec![var("doc_id"), actor()]),
        ],
    }
}

// ============================================================
// Quantitative authority - ApprovalLimit + approve_within_limit
// ============================================================

pub fn grant_approval_limit() -> Transformation {
    Transformation {
        name: "grant_approval_limit".to_string(),
        parameters: params(&["principal", "doc_type", "limit"]),
        body: vec![
            assert_(
                "ApprovalLimit",
                vec![var("principal"), var("doc_type"), var("limit")],
            ),
            emit(
                "ApprovalLimitGranted",
                vec![var("principal"), var("doc_type"), var("limit")],
            ),
        ],
    }
}

pub fn revoke_approval_limit() -> Transformation {
    Transformation {
        name: "revoke_approval_limit".to_string(),
        parameters: params(&["principal", "doc_type", "limit"]),
        body: vec![
            require(claim(
                "ApprovalLimit",
                vec![var("principal"), var("doc_type"), var("limit")],
            )),
            retract(
                "ApprovalLimit",
                vec![var("principal"), var("doc_type"), var("limit")],
            ),
            emit(
                "ApprovalLimitRevoked",
                vec![var("principal"), var("doc_type"), var("limit")],
            ),
        ],
    }
}

/// Quantitative approval. The require is `And(ApprovalLimit($actor,
/// doc_type, limit), Le(amount, limit))` - binds `limit` from the
/// authority claim and compares the proposed amount.
pub fn approve_within_limit() -> Transformation {
    Transformation {
        name: "approve_within_limit".to_string(),
        parameters: params(&["doc_id", "doc_type", "amount"]),
        body: vec![
            require(and(vec![
                claim(
                    "ApprovalLimit",
                    vec![actor(), var("doc_type"), var("limit")],
                ),
                le(term(var("amount")), term(var("limit"))),
            ])),
            assert_(
                "LimitedApproval",
                vec![var("doc_id"), var("doc_type"), var("amount"), actor()],
            ),
            emit(
                "DocumentApprovedWithinLimit",
                vec![var("doc_id"), actor(), var("amount")],
            ),
        ],
    }
}

pub fn all_predicates() -> Vec<morpholog_core::PredicateDecl> {
    vec![
        predicate("MayApprove")
            .subject("actor")
            .subject("doc_type")
            .build(),
        predicate("Approval")
            .subject("doc_id")
            .subject("doc_type")
            .subject("actor")
            .build(),
        predicate("ApprovalLimit")
            .subject("actor")
            .subject("doc_type")
            .decimal("limit")
            .build(),
        predicate("LimitedApproval")
            .subject("doc_id")
            .subject("doc_type")
            .decimal("amount")
            .subject("actor")
            .build(),
    ]
}

pub fn all_invariants() -> Vec<Invariant> {
    vec![]
}

/// The approval-controls example as a [`morpholog_core::Program`]:
/// six transformations across two authority shapes, no invariants.
/// Stable identifier: `"approval_controls"`.
pub fn program() -> morpholog_core::Program {
    morpholog_core::Program {
        name: "approval_controls".to_string(),
        predicates: all_predicates(),
        invariants: all_invariants(),
        transformations: vec![
            grant_approval_authority(),
            revoke_approval_authority(),
            approve_document(),
            grant_approval_limit(),
            revoke_approval_limit(),
            approve_within_limit(),
        ],
        derived_claims: vec![],
    }
}
