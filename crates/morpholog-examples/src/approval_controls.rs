//! Approval controls IR: unconditional `MayApprove` + quantitative
//! `ApprovalLimit`, both consulted via `Term::Actor` in admission-time
//! `require` clauses. No invariants - revocation prevents future
//! approvals but historical approvals stay admitted. See
//! `examples/04_approval_controls/README.md` for the business framing.

use morpholog_core::{Claim, Expr, Intent, Invariant, Stmt, Term, Transformation};

fn var(name: &str) -> Term {
    Term::Var(name.to_string())
}

// ============================================================
// Unconditional authority - MayApprove + approve_document
// ============================================================

/// Grant `actor` the unconditional authority to approve documents of
/// `doc_type`.
///
/// In v0 this transformation is ungated: any caller can grant
/// authority to any subject. A real system would gate this on an
/// administrative-authority claim about the *proposing* actor; the
/// natural follow-on once predicate-pattern matching arrives.
pub fn grant_approval_authority() -> Transformation {
    Transformation {
        name: "grant_approval_authority".to_string(),
        parameters: vec!["actor".to_string(), "doc_type".to_string()],
        body: vec![
            Stmt::Assert(Claim {
                predicate: "MayApprove".to_string(),
                args: vec![var("actor"), var("doc_type")],
            }),
            Stmt::Emit(Intent {
                name: "ApprovalAuthorityGranted".to_string(),
                args: vec![var("actor"), var("doc_type")],
            }),
        ],
    }
}

/// Revoke `actor`'s unconditional authority to approve documents of
/// `doc_type`.
///
/// Removes future authority; *historical* `Approval` claims admitted
/// under the authority are preserved (no invariant ties Approval to
/// live MayApprove). That asymmetry is the require-vs-invariant
/// lesson.
pub fn revoke_approval_authority() -> Transformation {
    Transformation {
        name: "revoke_approval_authority".to_string(),
        parameters: vec!["actor".to_string(), "doc_type".to_string()],
        body: vec![
            Stmt::Require(Expr::Claim {
                predicate: "MayApprove".to_string(),
                args: vec![var("actor"), var("doc_type")],
            }),
            Stmt::Retract {
                predicate: "MayApprove".to_string(),
                args: vec![var("actor"), var("doc_type")],
            },
            Stmt::Emit(Intent {
                name: "ApprovalAuthorityRevoked".to_string(),
                args: vec![var("actor"), var("doc_type")],
            }),
        ],
    }
}

/// Approve `doc_id` of `doc_type` under the proposing actor's
/// unconditional authority.
///
/// The transformation declares no `actor` parameter; the actor flows
/// through transition context and is consulted via `Term::Actor`.
/// Admission requires `MayApprove($actor, doc_type)` in pre-state.
/// On success, the asserted `Approval` carries the proposing actor.
pub fn approve_document() -> Transformation {
    Transformation {
        name: "approve_document".to_string(),
        parameters: vec!["doc_id".to_string(), "doc_type".to_string()],
        body: vec![
            Stmt::Require(Expr::Claim {
                predicate: "MayApprove".to_string(),
                args: vec![Term::Actor, var("doc_type")],
            }),
            Stmt::Assert(Claim {
                predicate: "Approval".to_string(),
                args: vec![var("doc_id"), var("doc_type"), Term::Actor],
            }),
            Stmt::Emit(Intent {
                name: "DocumentApproved".to_string(),
                args: vec![var("doc_id"), Term::Actor],
            }),
        ],
    }
}

// ============================================================
// Quantitative authority - ApprovalLimit + approve_within_limit
// ============================================================

/// Grant `actor` authority to approve documents of `doc_type` up to
/// `limit`. Multiple grants for the same (actor, doc_type) with
/// different limits are allowed; the effective ceiling for any
/// proposed amount is whichever grant satisfies it.
pub fn grant_approval_limit() -> Transformation {
    Transformation {
        name: "grant_approval_limit".to_string(),
        parameters: vec![
            "actor".to_string(),
            "doc_type".to_string(),
            "limit".to_string(),
        ],
        body: vec![
            Stmt::Assert(Claim {
                predicate: "ApprovalLimit".to_string(),
                args: vec![var("actor"), var("doc_type"), var("limit")],
            }),
            Stmt::Emit(Intent {
                name: "ApprovalLimitGranted".to_string(),
                args: vec![var("actor"), var("doc_type"), var("limit")],
            }),
        ],
    }
}

/// Revoke a specific `(actor, doc_type, limit)` quantitative grant.
/// Historical `LimitedApproval` claims admitted under it are
/// preserved.
pub fn revoke_approval_limit() -> Transformation {
    Transformation {
        name: "revoke_approval_limit".to_string(),
        parameters: vec![
            "actor".to_string(),
            "doc_type".to_string(),
            "limit".to_string(),
        ],
        body: vec![
            Stmt::Require(Expr::Claim {
                predicate: "ApprovalLimit".to_string(),
                args: vec![var("actor"), var("doc_type"), var("limit")],
            }),
            Stmt::Retract {
                predicate: "ApprovalLimit".to_string(),
                args: vec![var("actor"), var("doc_type"), var("limit")],
            },
            Stmt::Emit(Intent {
                name: "ApprovalLimitRevoked".to_string(),
                args: vec![var("actor"), var("doc_type"), var("limit")],
            }),
        ],
    }
}

/// Approve `doc_id` of `doc_type` for `amount`, under the proposing
/// actor's quantitative authority.
///
/// The require is `And(ApprovalLimit($actor, doc_type, limit),
/// Le(amount, limit))` - it binds `limit` from the authority claim
/// and compares the proposed amount against it. Boundary equality
/// (amount == limit) is inclusive.
pub fn approve_within_limit() -> Transformation {
    Transformation {
        name: "approve_within_limit".to_string(),
        parameters: vec![
            "doc_id".to_string(),
            "doc_type".to_string(),
            "amount".to_string(),
        ],
        body: vec![
            Stmt::Require(Expr::And(vec![
                Expr::Claim {
                    predicate: "ApprovalLimit".to_string(),
                    args: vec![Term::Actor, var("doc_type"), var("limit")],
                },
                Expr::Le(
                    Box::new(Expr::Term(var("amount"))),
                    Box::new(Expr::Term(var("limit"))),
                ),
            ])),
            Stmt::Assert(Claim {
                predicate: "LimitedApproval".to_string(),
                args: vec![var("doc_id"), var("doc_type"), var("amount"), Term::Actor],
            }),
            Stmt::Emit(Intent {
                name: "DocumentApprovedWithinLimit".to_string(),
                args: vec![var("doc_id"), Term::Actor, var("amount")],
            }),
        ],
    }
}

pub fn all_invariants() -> Vec<Invariant> {
    vec![]
}

/// The approval-controls example as a [`morpholog_core::Program`]: six
/// transformations split across two authority shapes (unconditional
/// and quantitative), no invariants. Stable identifier:
/// `"approval_controls"`.
pub fn program() -> morpholog_core::Program {
    morpholog_core::Program {
        name: "approval_controls".to_string(),
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
