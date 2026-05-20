//! Approval limits - the smallest worked example consulting
//! `Expr::Le`.
//!
//! Surface-syntax form: `examples/07_approval_limits/approval_limits.morph`.
//!
//! Extends the actor-authority shape (Example 6) with *quantitative*
//! authority: an actor may approve documents of a kind *up to a limit*.
//! The forcing function for `Expr::Le` - decimal less-than-or-equal -
//! lives here. Without it, "amount <= limit" could not be expressed.
//!
//! No invariants. Same require-vs-invariant doctrine as Example 6:
//! authority is a precondition at admission time, not a continuous
//! property of admitted state. Revoking a limit prevents future
//! approvals while preserving historical ones.

use crate::{Claim, Expr, Intent, Invariant, Stmt, Term, Transformation};

fn var(name: &str) -> Term {
    Term::Var(name.to_string())
}

// ============================================================
// Transformations
// ============================================================

/// Grant `actor` authority to approve documents of `doc_type` up to
/// `limit`. Multiple grants for the same (actor, doc_type) with
/// different limits are allowed; the effective ceiling is whichever
/// one satisfies the `approve_within_limit` require.
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

/// Revoke a specific `(actor, doc_type, limit)` authority grant.
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

/// Approve a document of `doc_type` for `amount`, under the
/// proposing actor's authority.
///
/// Admission requires *some* `ApprovalLimit($actor, doc_type, limit)`
/// to exist in pre-state where `amount <= limit`. The And expression
/// in the require binds `limit` from the ApprovalLimit claim, then
/// the `Le` checks the bound `amount` parameter against it. If no
/// such combination exists, the require fails and the proposal is
/// rejected with no audit row, no claim mutation, no intent.
///
/// On commit, `LimitedApproval(doc_id, doc_type, amount, $actor)`
/// is asserted with the proposing actor stamped onto the durable
/// record. The audit row independently carries the actor in its
/// `actor` column (see `audit.actor`).
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

/// The approval-limits example as a [`crate::Program`]: three
/// transformations, no invariants. Stable identifier:
/// `"approval_limits"`.
///
/// The absence of invariants is intentional and follows Example 6's
/// reasoning: a `LimitedApproval` admitted under a now-revoked
/// `ApprovalLimit` is not retroactively invalidated. The legitimacy
/// of a past decision was established when it was made.
pub fn program() -> crate::Program {
    crate::Program {
        name: "approval_limits".to_string(),
        invariants: all_invariants(),
        transformations: vec![
            grant_approval_limit(),
            revoke_approval_limit(),
            approve_within_limit(),
        ],
        derived_claims: vec![],
    }
}
