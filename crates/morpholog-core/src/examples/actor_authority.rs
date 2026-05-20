//! Actor authority - the smallest worked example exercising
//! `Term::Actor`.
//!
//! Surface-syntax form: `examples/06_actor_authority/actor_authority.morph`.
//!
//! Proves that an admission gate can consult the actor of the proposed
//! transition without the transformation declaring an actor parameter.
//! The actor flows through transition context (see [`crate::Transition`])
//! and is read inside `require` and `assert` clauses via `Term::Actor`.
//!
//! No invariants. The doctrine here is **require-based authority** -
//! authority is a precondition at admission time, not a continuous
//! property of admitted state. Tying decisions to live authority via
//! an invariant would force cascade-retraction of historical approvals
//! when authority is revoked, which contradicts the rule that history
//! is preserved (see Example 3's require-vs-invariant lesson). This
//! example deliberately keeps the model on the safe side of that line.

use crate::{Claim, Expr, Intent, Invariant, Stmt, Term, Transformation};

fn var(name: &str) -> Term {
    Term::Var(name.to_string())
}

// ============================================================
// Transformations
// ============================================================

/// Grant `actor` the authority to approve documents of `doc_type`.
///
/// In v0 this transformation is ungated: any caller can grant
/// authority to any subject. A real system would gate this on an
/// administrative authority claim about the *proposing* actor; that
/// is the natural follow-on once approval limits and predicate-pattern
/// matching arrive.
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

/// Revoke `actor`'s authority to approve documents of `doc_type`.
///
/// Requires the authority to be currently held. The retraction
/// removes future authority; *historical* approvals admitted under
/// the authority are preserved (no invariant ties Approval to live
/// MayApprove). That asymmetry is the require-vs-invariant lesson.
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

/// Approve `doc_id` of `doc_type` under the authority of the actor
/// proposing the transition.
///
/// The transformation declares no `actor` parameter; the actor flows
/// through transition context and is read via `Term::Actor`. Admission
/// requires that the *proposing actor* hold `MayApprove($actor, doc_type)`
/// in the pre-state. On success, the recorded `Approval` carries the
/// proposing actor as its third argument, providing the durable
/// answer to "who approved this document?"
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

pub fn all_invariants() -> Vec<Invariant> {
    vec![]
}

/// The actor-authority example as a [`crate::Program`]: three
/// transformations, no invariants. Stable identifier:
/// `"actor_authority"`.
///
/// The absence of invariants is intentional and load-bearing. The
/// require-vs-invariant lesson from Example 3 applies: an invariant
/// tying `Approval` to live `MayApprove` would force the runtime to
/// either reject authority revocations or cascade-retract historical
/// approvals. Neither matches the real-world semantics.
pub fn program() -> crate::Program {
    crate::Program {
        name: "actor_authority".to_string(),
        invariants: all_invariants(),
        transformations: vec![
            grant_approval_authority(),
            revoke_approval_authority(),
            approve_document(),
        ],
        derived_claims: vec![],
    }
}
