//! IR-construction helpers, used internally by the example modules
//! to make the verbose Rust IR construction read closer to the
//! surface syntax those examples illustrate.
//!
//! Not a stable public API. A future parser will produce `morpholog_core`
//! IR types directly; these helpers will then go.

use morpholog_core::{Claim, Expr, Intent, Stmt, Term, Value};

// ---------- Term constructors ----------

pub(crate) fn var(name: &str) -> Term {
    Term::Var(name.to_string())
}

pub(crate) fn lit_subj(s: &str) -> Term {
    Term::Literal(Value::Subject(s.to_string()))
}

pub(crate) fn lit_dec(s: &str) -> Term {
    Term::Literal(Value::Decimal(s.to_string()))
}

// ---------- Expr constructors ----------

pub(crate) fn claim(predicate: &str, args: Vec<Term>) -> Expr {
    Expr::Claim {
        predicate: predicate.to_string(),
        args,
    }
}

pub(crate) fn term(t: Term) -> Expr {
    Expr::Term(t)
}

pub(crate) fn and(exprs: Vec<Expr>) -> Expr {
    Expr::And(exprs)
}

pub(crate) fn not(inner: Expr) -> Expr {
    Expr::Not(Box::new(inner))
}

pub(crate) fn implies(left: Expr, right: Expr) -> Expr {
    Expr::Implies {
        left: Box::new(left),
        right: Box::new(right),
    }
}

pub(crate) fn exists(binding: &str, body: Expr) -> Expr {
    Expr::Exists {
        binding: binding.to_string(),
        body: Box::new(body),
    }
}

pub(crate) fn forall(binding: &str, source: Expr, body: Expr) -> Expr {
    Expr::Forall {
        binding: binding.to_string(),
        source: Box::new(source),
        body: Box::new(body),
    }
}

pub(crate) fn eq(lhs: Expr, rhs: Expr) -> Expr {
    Expr::Eq(Box::new(lhs), Box::new(rhs))
}

pub(crate) fn neq(t1: Term, t2: Term) -> Expr {
    Expr::Neq(t1, t2)
}

pub(crate) fn le(lhs: Expr, rhs: Expr) -> Expr {
    Expr::Le(Box::new(lhs), Box::new(rhs))
}

pub(crate) fn sub(lhs: Expr, rhs: Expr) -> Expr {
    Expr::Sub(Box::new(lhs), Box::new(rhs))
}

pub(crate) fn add(lhs: Expr, rhs: Expr) -> Expr {
    Expr::Add(Box::new(lhs), Box::new(rhs))
}

pub(crate) fn sum(value: Term, binding: &str, body: Expr) -> Expr {
    Expr::Sum {
        value,
        binding: binding.to_string(),
        body: Box::new(body),
    }
}

pub(crate) fn in_(elem: Term, coll: Term) -> Expr {
    Expr::In(elem, coll)
}

pub(crate) fn value_of(predicate: &str, args: Vec<Term>) -> Expr {
    Expr::ValueOf {
        predicate: predicate.to_string(),
        args,
        default: None,
    }
}

// ---------- Stmt constructors ----------

pub(crate) fn require(expr: Expr) -> Stmt {
    Stmt::Require(expr)
}

pub(crate) fn assert_(predicate: &str, args: Vec<Term>) -> Stmt {
    Stmt::Assert(Claim {
        predicate: predicate.to_string(),
        args,
    })
}

pub(crate) fn retract(predicate: &str, args: Vec<Term>) -> Stmt {
    Stmt::Retract {
        predicate: predicate.to_string(),
        args,
    }
}

pub(crate) fn emit(name: &str, args: Vec<Term>) -> Stmt {
    Stmt::Emit(Intent {
        name: name.to_string(),
        args,
    })
}

pub(crate) fn let_(name: &str, value: Expr) -> Stmt {
    Stmt::Let {
        name: name.to_string(),
        value,
    }
}

pub(crate) fn let_new_subject(name: &str) -> Stmt {
    Stmt::LetNewSubject {
        name: name.to_string(),
    }
}

pub(crate) fn for_(binding: &str, collection: Expr, body: Vec<Stmt>) -> Stmt {
    Stmt::For {
        binding: binding.to_string(),
        collection,
        body,
    }
}

// ---------- Parameter-list sugar ----------

pub(crate) fn params(names: &[&str]) -> Vec<String> {
    names.iter().map(|s| s.to_string()).collect()
}
