//! Constructors for building Morpholog programs in Rust, used as the v0
//! authoring surface until a parser exists.
//!
//! The kernel IR (`Expr`, `Stmt`, `Term`, `Value`, `Claim`, `Intent`,
//! `Invariant`, `Transformation`, `DerivedClaim`, `Program`) is
//! deliberately low-level: every variant is one thing, every field
//! carries one meaning, and there is no syntactic sugar. That keeps the
//! kernel minimal but makes the call site of a transformation body read
//! like data structure construction:
//!
//! ```ignore
//! Stmt::Require(Expr::And(vec![
//!     Expr::Claim {
//!         predicate: "Policy".to_string(),
//!         args: vec![Term::Var("policy_id".to_string()), Term::Wildcard],
//!     },
//!     Expr::Le(
//!         Box::new(Expr::Term(Term::Var("amount".to_string()))),
//!         Box::new(Expr::Term(Term::Var("limit".to_string()))),
//!     ),
//! ]))
//! ```
//!
//! The `dsl` module gives that same construction a readable shape:
//!
//! ```ignore
//! use morpholog_core::dsl::*;
//!
//! require(and(vec![
//!     claim("Policy", vec![var("policy_id"), wildcard()]),
//!     le(term(var("amount")), term(var("limit"))),
//! ]))
//! ```
//!
//! These constructors are thin wrappers; the kernel never sees them.
//! They exist because every worked example, every test, and (when there
//! is no parser yet) every external user assembles IR by hand. Making
//! the construction surface readable is the v0 substitute for surface
//! syntax.
//!
//! When a parser arrives, the parser will emit IR directly (via the
//! same constructors). This module stays as the lower-level
//! programmatic API the parser is built on.
//!
//! Naming conventions:
//! - Term literals are short: `subj`, `dec`, `date`. They return `Term`,
//!   ready to drop into a `vec![...]` argument list.
//! - Statement constructors that share a name with a Rust keyword take
//!   a trailing underscore: `assert_`, `let_`, `let_new_subject`, `for_`.
//!   These are unavoidable Rust constraints, not stylistic choices.
//! - `actor()` and `wildcard()` are nullary constructors, the natural
//!   way to write `Term::Actor` and `Term::Wildcard` in a `vec!` list.
//! - `role()` is a semantic alias for `subj()` introduced when the
//!   clinical-trial-enrolment example needed to name a delegated role
//!   literal at the call site. The runtime representation is identical
//!   to `subj()`; the alias only documents reader intent. Resist
//!   adding more aliases until a worked example forces one - the
//!   subject-as-string-as-everything model is intentional, and
//!   pseudo-types over it would not help.

use crate::{Claim, Expr, Intent, Stmt, Term, Value};

// ============================================================
// Term constructors
// ============================================================

/// A free variable bound somewhere in the surrounding context (a
/// transformation parameter, a `let`, a `for`, an `exists`, or the
/// match positions of an enclosing claim pattern).
pub fn var(name: &str) -> Term {
    Term::Var(name.to_string())
}

/// Match anything at this position. Valid inside `Expr::Claim`,
/// `Stmt::Retract`, and `Expr::ValueOf` patterns. Invalid in `Term`-as-
/// value positions (resolves to `EvalError::TypeMismatch`).
pub fn wildcard() -> Term {
    Term::Wildcard
}

/// The actor proposing the current transition. Only valid inside a
/// transformation body (`require`, `let`, `assert`, `retract`, `emit`,
/// `for`); referencing it from an invariant or derived-claim body
/// raises `EvalError::UnboundActor`.
pub fn actor() -> Term {
    Term::Actor
}

/// Subject literal. Used for named constants (purposes, roles,
/// statuses, fixed authorities) and for embedding specific subject
/// identifiers in IR bodies.
pub fn subj(s: &str) -> Term {
    Term::Literal(Value::Subject(s.to_string()))
}

/// Decimal literal. Stored as the exact source string; the evaluator
/// parses it to `rust_decimal::Decimal` on use.
pub fn dec(s: &str) -> Term {
    Term::Literal(Value::Decimal(s.to_string()))
}

/// ISO-8601 civil-date literal. Stored as the exact source string; the
/// evaluator parses it to `jiff::civil::Date` on use.
pub fn date(s: &str) -> Term {
    Term::Literal(Value::Date(s.to_string()))
}

/// Semantic alias for [`subj`]. Identical runtime representation;
/// documents reader intent at the call site when the subject names
/// a delegated role (e.g. `role("randomise_participant")` in the
/// clinical-trial-enrolment example's `DelegatedInvestigator`
/// pattern).
pub fn role(s: &str) -> Term {
    subj(s)
}

// ============================================================
// Expr constructors
// ============================================================

/// Claim pattern. Each `args` term is either a variable to bind, a
/// wildcard, a literal to match, or `actor()`. Match semantics:
/// every position must unify against the candidate claim.
pub fn claim(predicate: &str, args: Vec<Term>) -> Expr {
    Expr::Claim {
        predicate: predicate.to_string(),
        args,
    }
}

/// Lift a [`Term`] into value position. Used wherever a sub-expression
/// must evaluate to a value (e.g. inside `Le`, `DateLe`, `Add`, `Sub`,
/// `Sum.value`).
pub fn term(t: Term) -> Expr {
    Expr::Term(t)
}

pub fn and(exprs: Vec<Expr>) -> Expr {
    Expr::And(exprs)
}

pub fn not(inner: Expr) -> Expr {
    Expr::Not(Box::new(inner))
}

pub fn implies(left: Expr, right: Expr) -> Expr {
    Expr::Implies {
        left: Box::new(left),
        right: Box::new(right),
    }
}

pub fn exists(binding: &str, body: Expr) -> Expr {
    Expr::Exists {
        binding: binding.to_string(),
        body: Box::new(body),
    }
}

pub fn forall(binding: &str, source: Expr, body: Expr) -> Expr {
    Expr::Forall {
        binding: binding.to_string(),
        source: Box::new(source),
        body: Box::new(body),
    }
}

pub fn eq(lhs: Expr, rhs: Expr) -> Expr {
    Expr::Eq(Box::new(lhs), Box::new(rhs))
}

pub fn neq(t1: Term, t2: Term) -> Expr {
    Expr::Neq(t1, t2)
}

pub fn le(lhs: Expr, rhs: Expr) -> Expr {
    Expr::Le(Box::new(lhs), Box::new(rhs))
}

pub fn date_le(lhs: Expr, rhs: Expr) -> Expr {
    Expr::DateLe(Box::new(lhs), Box::new(rhs))
}

pub fn sub(lhs: Expr, rhs: Expr) -> Expr {
    Expr::Sub(Box::new(lhs), Box::new(rhs))
}

pub fn add(lhs: Expr, rhs: Expr) -> Expr {
    Expr::Add(Box::new(lhs), Box::new(rhs))
}

pub fn sum(value: Term, binding: &str, body: Expr) -> Expr {
    Expr::Sum {
        value,
        binding: binding.to_string(),
        body: Box::new(body),
    }
}

pub fn in_(elem: Term, coll: Term) -> Expr {
    Expr::In(elem, coll)
}

/// Functional lookup: match exactly one claim and yield its
/// wildcard-position value. Zero matches errors unless `default` is
/// supplied; multiple matches always errors. Constructed without a
/// default by this helper; use [`value_of_with_default`] for the
/// fallback form.
pub fn value_of(predicate: &str, args: Vec<Term>) -> Expr {
    Expr::ValueOf {
        predicate: predicate.to_string(),
        args,
        default: None,
    }
}

/// `value_of` with a fallback expression evaluated when zero matches.
/// Multiple matches still error.
pub fn value_of_with_default(predicate: &str, args: Vec<Term>, default: Expr) -> Expr {
    Expr::ValueOf {
        predicate: predicate.to_string(),
        args,
        default: Some(Box::new(default)),
    }
}

// ============================================================
// Stmt constructors
// ============================================================

pub fn require(expr: Expr) -> Stmt {
    Stmt::Require(expr)
}

pub fn assert_(predicate: &str, args: Vec<Term>) -> Stmt {
    Stmt::Assert(Claim {
        predicate: predicate.to_string(),
        args,
    })
}

pub fn retract(predicate: &str, args: Vec<Term>) -> Stmt {
    Stmt::Retract {
        predicate: predicate.to_string(),
        args,
    }
}

pub fn emit(name: &str, args: Vec<Term>) -> Stmt {
    Stmt::Emit(Intent {
        name: name.to_string(),
        args,
    })
}

pub fn let_(name: &str, value: Expr) -> Stmt {
    Stmt::Let {
        name: name.to_string(),
        value,
    }
}

pub fn let_new_subject(name: &str) -> Stmt {
    Stmt::LetNewSubject {
        name: name.to_string(),
    }
}

pub fn for_(binding: &str, collection: Expr, body: Vec<Stmt>) -> Stmt {
    Stmt::For {
        binding: binding.to_string(),
        collection,
        body,
    }
}

// ============================================================
// Parameter-list sugar
// ============================================================

/// Convenience for the parameter list of a [`crate::Transformation`].
/// Equivalent to `names.iter().map(|s| s.to_string()).collect()` but
/// reads as `params(&["claim_id", "amount"])` at the call site.
pub fn params(names: &[&str]) -> Vec<String> {
    names.iter().map(|s| s.to_string()).collect()
}
