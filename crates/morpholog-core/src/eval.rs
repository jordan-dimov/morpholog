//! The in-memory evaluator.
//!
//! `find_matches` walks an [`Expr`] against a [`State`] and a binding
//! context, returning either the set of extended binding contexts that
//! satisfy the expression (predicate-shaped use) or a kernel error.
//! The supporting helpers (`find_claim_matches`, `unify_args`,
//! `find_conjunction`, `find_in_matches`, `eval_value`, `resolve_term`,
//! `parse_date_literal`) are crate-private workhorses called from
//! `find_matches`, [`crate::propose`], and [`crate::derive`].
//!
//! `EvalError` is the structured error type raised when an expression
//! is structurally ill-formed (type mismatches, missing variables,
//! ValueOf cardinality violations, etc.). Distinct from lawful
//! business rejection, which is reported as `Outcome::Rejected`.

use jiff::civil::Date;
use rust_decimal::Decimal;
use std::str::FromStr;

use crate::ir::{Expr, Term, Value};
use crate::state::{Bindings, EvalValue, State};

/// Errors raised by the evaluator and the transformation runner. These
/// are distinct from *lawful business rejection* (which is reported as
/// [`crate::Outcome::Rejected`]); an `EvalError` indicates that an expression
/// or transformation was structurally ill-formed and cannot be run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EvalError {
    /// A variable was referenced before being bound by a parameter,
    /// `let`, `for`, or `exists` binding.
    UnboundVariable(String),
    /// An expression demanded an operand of one kind but received
    /// another (e.g. arithmetic on a subject, membership on a non-
    /// collection, etc.).
    TypeMismatch(String),
    /// An expression that must be predicate-shaped (boolean-valued)
    /// was used in a position that cannot interpret it.
    NotPredicate,
    /// An expression that must be value-producing was used in a
    /// position that requires a value (e.g. as a `let` right-hand side
    /// or a sum target).
    NotValue,
    /// `Expr::ValueOf(predicate, args)` matched zero claims and no
    /// `default` was supplied.
    ValueOfZeroMatches(String),
    /// `Expr::ValueOf(predicate, args)` matched more than one claim;
    /// the functional-lookup contract requires exactly one match.
    ValueOfMultipleMatches(String),
    /// `Term::Actor` was referenced in a context that has no transition
    /// in scope - any path that calls into the evaluator with
    /// `actor = None`. The common cases are invariant bodies and
    /// derived-claim bodies (both evaluate against admitted state, not
    /// against any specific proposing transition). Authority checks
    /// belong in `require`, not in invariants; this error makes that
    /// doctrine enforceable rather than convention.
    UnboundActor,
}

impl std::fmt::Display for EvalError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            EvalError::UnboundVariable(name) => write!(f, "unbound variable: {name}"),
            EvalError::TypeMismatch(msg) => write!(f, "type mismatch: {msg}"),
            EvalError::NotPredicate => write!(f, "expression is not a predicate"),
            EvalError::NotValue => write!(f, "expression is not value-producing"),
            EvalError::ValueOfZeroMatches(p) => {
                write!(f, "value({p}, _): zero matches")
            }
            EvalError::ValueOfMultipleMatches(p) => {
                write!(f, "value({p}, _): multiple matches")
            }
            EvalError::UnboundActor => write!(
                f,
                "Term::Actor referenced with no transition in scope (likely used outside a transformation body - e.g., inside an invariant or derived-claim body; authority checks belong in `require`)"
            ),
        }
    }
}

impl std::error::Error for EvalError {}
pub(crate) fn find_matches(
    e: &Expr,
    state: &State,
    base: &Bindings,
    actor: Option<&EvalValue>,
) -> Result<Vec<Bindings>, EvalError> {
    match e {
        Expr::Claim { predicate, args } => find_claim_matches(predicate, args, state, base, actor),
        Expr::And(exprs) => find_conjunction(exprs, state, base, actor),
        Expr::Not(inner) => {
            let m = find_matches(inner, state, base, actor)?;
            Ok(if m.is_empty() {
                vec![base.clone()]
            } else {
                vec![]
            })
        }
        Expr::Implies { left, right } => {
            let lm = find_matches(left, state, base, actor)?;
            for m in lm {
                if find_matches(right, state, &m, actor)?.is_empty() {
                    return Ok(vec![]);
                }
            }
            Ok(vec![base.clone()])
        }
        Expr::Exists { binding: _, body } => {
            let m = find_matches(body, state, base, actor)?;
            Ok(if m.is_empty() {
                vec![]
            } else {
                vec![base.clone()]
            })
        }
        Expr::Forall {
            binding: _,
            source,
            body,
        } => {
            let sm = find_matches(source, state, base, actor)?;
            for m in sm {
                if find_matches(body, state, &m, actor)?.is_empty() {
                    return Ok(vec![]);
                }
            }
            Ok(vec![base.clone()])
        }
        Expr::Eq(lhs, rhs) => {
            let l = eval_value(lhs, state, base, actor)?;
            let r = eval_value(rhs, state, base, actor)?;
            Ok(if l == r { vec![base.clone()] } else { vec![] })
        }
        Expr::Le(lhs, rhs) => {
            let l = eval_value(lhs, state, base, actor)?;
            let r = eval_value(rhs, state, base, actor)?;
            match (l, r) {
                (EvalValue::Decimal(a), EvalValue::Decimal(b)) => {
                    Ok(if a <= b { vec![base.clone()] } else { vec![] })
                }
                _ => Err(EvalError::TypeMismatch(
                    "Le expects decimal operands".into(),
                )),
            }
        }
        Expr::DateLe(lhs, rhs) => {
            let l = eval_value(lhs, state, base, actor)?;
            let r = eval_value(rhs, state, base, actor)?;
            match (l, r) {
                (EvalValue::Date(a), EvalValue::Date(b)) => {
                    Ok(if a <= b { vec![base.clone()] } else { vec![] })
                }
                _ => Err(EvalError::TypeMismatch(
                    "DateLe expects civil-date operands".into(),
                )),
            }
        }
        Expr::Neq(t1, t2) => {
            let l = resolve_term(t1, base, actor)?;
            let r = resolve_term(t2, base, actor)?;
            Ok(if l != r { vec![base.clone()] } else { vec![] })
        }
        Expr::In(elem, coll) => find_in_matches(elem, coll, base, actor),
        Expr::Term(_)
        | Expr::Sub(_, _)
        | Expr::Add(_, _)
        | Expr::Sum { .. }
        | Expr::ValueOf { .. } => Err(EvalError::NotPredicate),
    }
}

/// Parse a `Value::Date(String)` literal into a `jiff::civil::Date`.
/// Centralised so the IR-level literal and the runtime value cannot drift
/// in how they interpret `YYYY-MM-DD`. Used by `resolve_term`, by
/// `unify_args` for `Value::Date` literals in claim patterns, and by
/// `find_claim_matches` when narrowing a predicate bucket by a ground
/// date argument.
pub(crate) fn parse_date_literal(s: &str) -> Result<Date, EvalError> {
    s.parse::<Date>()
        .map_err(|e| EvalError::TypeMismatch(format!("invalid civil date `{s}`: {e}")))
}

pub(crate) fn find_claim_matches(
    predicate: &str,
    args: &[Term],
    state: &State,
    base: &Bindings,
    actor: Option<&EvalValue>,
) -> Result<Vec<Bindings>, EvalError> {
    let mut out = vec![];

    // Pre-pass: any occurrence of `Term::Actor` requires an actor in
    // scope. Without this, a selective ground arg appearing *earlier*
    // in the args could short-circuit to `Ok(empty)` (missing bucket)
    // before the loop ever reaches `Term::Actor`. That would leak the
    // doctrine - an invariant referencing `Term::Actor` could silently
    // produce no matches instead of erroring. Make the requirement
    // position-independent.
    if actor.is_none() && args.iter().any(|t| matches!(t, Term::Actor)) {
        return Err(EvalError::UnboundActor);
    }

    // First pass: identify every argument position that is *ground* in
    // the current binding context (Term::Literal in the IR, or
    // Term::Var already bound in `base`). Pick the position whose
    // (predicate, position, value) bucket is smallest; that's the most
    // selective lookup.
    //
    // For a typical invariant body like `JournalLine(entry, _, d, _)`
    // evaluated inside a `forall entry: ...`, `entry` is bound to a
    // specific subject and position 0 has a bucket of exactly the few
    // lines for that entry. That changes the scan from "all
    // JournalLines" to "JournalLines for this entry" - the difference
    // between O(N) and O(lines_per_entry) per lookup, which is where
    // the quadratic in `balanced_posted_entry` lives.
    //
    // If a ground arg's bucket is missing entirely, no claim of this
    // predicate has that value at that position; the result set is
    // empty and we short-circuit.
    //
    // If no argument is ground, fall back to scanning the whole
    // predicate bucket via `state.claims_for(predicate)`.
    let mut best: Option<&[usize]> = None;
    for (pos, term) in args.iter().enumerate() {
        let ground = match term {
            Term::Wildcard => None,
            Term::Var(name) => base.get(name).cloned(),
            Term::Literal(Value::Subject(s)) => Some(EvalValue::Subject(s.clone())),
            Term::Literal(Value::Decimal(s)) => Decimal::from_str(s).ok().map(EvalValue::Decimal),
            Term::Literal(Value::Date(s)) => parse_date_literal(s).ok().map(EvalValue::Date),
            Term::Actor => match actor {
                Some(a) => Some(a.clone()),
                None => return Err(EvalError::UnboundActor),
            },
        };
        let Some(value) = ground else {
            continue;
        };
        match state.claim_indices_for_arg(predicate, pos, &value) {
            None => return Ok(out),
            Some(bucket) => match best {
                Some(prev) if prev.len() <= bucket.len() => {}
                _ => best = Some(bucket),
            },
        }
    }

    if let Some(bucket) = best {
        for &i in bucket {
            let claim = state.claim_at(i);
            if claim.args.len() != args.len() {
                continue;
            }
            if let Some(b) = unify_args(args, &claim.args, base, actor) {
                out.push(b);
            }
        }
    } else {
        for claim in state.claims_for(predicate) {
            if claim.args.len() != args.len() {
                continue;
            }
            if let Some(b) = unify_args(args, &claim.args, base, actor) {
                out.push(b);
            }
        }
    }
    Ok(out)
}

pub(crate) fn unify_args(
    patterns: &[Term],
    values: &[EvalValue],
    base: &Bindings,
    actor: Option<&EvalValue>,
) -> Option<Bindings> {
    let mut b = base.clone();
    for (p, v) in patterns.iter().zip(values.iter()) {
        match p {
            Term::Wildcard => {}
            Term::Var(name) => {
                if let Some(existing) = b.get(name) {
                    if existing != v {
                        return None;
                    }
                } else {
                    b.insert(name.clone(), v.clone());
                }
            }
            Term::Literal(Value::Decimal(s)) => {
                let parsed = Decimal::from_str(s).ok()?;
                match v {
                    EvalValue::Decimal(d) if *d == parsed => {}
                    _ => return None,
                }
            }
            Term::Literal(Value::Subject(s)) => match v {
                EvalValue::Subject(id) if id == s => {}
                _ => return None,
            },
            Term::Literal(Value::Date(s)) => {
                let parsed = parse_date_literal(s).ok()?;
                match v {
                    EvalValue::Date(d) if *d == parsed => {}
                    _ => return None,
                }
            }
            Term::Actor => match actor {
                Some(a) if a == v => {}
                _ => return None,
            },
        }
    }
    Some(b)
}

pub(crate) fn find_conjunction(
    exprs: &[Expr],
    state: &State,
    base: &Bindings,
    actor: Option<&EvalValue>,
) -> Result<Vec<Bindings>, EvalError> {
    let mut current = vec![base.clone()];
    for expr in exprs {
        let mut next = vec![];
        for b in &current {
            next.extend(find_matches(expr, state, b, actor)?);
        }
        if next.is_empty() {
            return Ok(vec![]);
        }
        current = next;
    }
    Ok(current)
}

pub(crate) fn find_in_matches(
    elem: &Term,
    coll: &Term,
    base: &Bindings,
    actor: Option<&EvalValue>,
) -> Result<Vec<Bindings>, EvalError> {
    let coll_val = resolve_term(coll, base, actor)?;
    let items = match coll_val {
        EvalValue::Collection(v) => v,
        _ => return Err(EvalError::TypeMismatch("In expects a collection".into())),
    };
    match elem {
        Term::Wildcard => Err(EvalError::TypeMismatch("wildcard not valid in In".into())),
        Term::Literal(_) | Term::Actor => {
            let e = resolve_term(elem, base, actor)?;
            Ok(if items.contains(&e) {
                vec![base.clone()]
            } else {
                vec![]
            })
        }
        Term::Var(name) => {
            if let Some(existing) = base.get(name) {
                Ok(if items.contains(existing) {
                    vec![base.clone()]
                } else {
                    vec![]
                })
            } else {
                Ok(items
                    .into_iter()
                    .map(|v| {
                        let mut b = base.clone();
                        b.insert(name.clone(), v);
                        b
                    })
                    .collect())
            }
        }
    }
}

pub(crate) fn eval_value(
    e: &Expr,
    state: &State,
    bindings: &Bindings,
    actor: Option<&EvalValue>,
) -> Result<EvalValue, EvalError> {
    match e {
        Expr::Term(t) => resolve_term(t, bindings, actor),
        Expr::Sub(lhs, rhs) => {
            let l = eval_value(lhs, state, bindings, actor)?;
            let r = eval_value(rhs, state, bindings, actor)?;
            match (l, r) {
                (EvalValue::Decimal(a), EvalValue::Decimal(b)) => Ok(EvalValue::Decimal(a - b)),
                _ => Err(EvalError::TypeMismatch(
                    "Sub expects decimal operands".into(),
                )),
            }
        }
        Expr::Add(lhs, rhs) => {
            let l = eval_value(lhs, state, bindings, actor)?;
            let r = eval_value(rhs, state, bindings, actor)?;
            match (l, r) {
                (EvalValue::Decimal(a), EvalValue::Decimal(b)) => Ok(EvalValue::Decimal(a + b)),
                _ => Err(EvalError::TypeMismatch(
                    "Add expects decimal operands".into(),
                )),
            }
        }
        Expr::Sum {
            value,
            binding: _,
            body,
        } => {
            let matches = find_matches(body, state, bindings, actor)?;
            let mut total = Decimal::ZERO;
            for m in matches {
                match resolve_term(value, &m, actor)? {
                    EvalValue::Decimal(d) => total += d,
                    _ => return Err(EvalError::TypeMismatch("Sum expects decimal".into())),
                }
            }
            Ok(EvalValue::Decimal(total))
        }
        Expr::ValueOf {
            predicate,
            args,
            default,
        } => {
            let matches = find_claim_matches(predicate, args, state, bindings, actor)?;
            match matches.len() {
                1 => {
                    let pos = args
                        .iter()
                        .position(|t| matches!(t, Term::Wildcard))
                        .ok_or_else(|| {
                            EvalError::TypeMismatch("ValueOf requires a wildcard arg".into())
                        })?;
                    let claim = state
                        .claims_for(predicate)
                        .find(|f| {
                            f.args.len() == args.len()
                                && unify_args(args, &f.args, bindings, actor).is_some()
                        })
                        .ok_or_else(|| EvalError::ValueOfZeroMatches(predicate.clone()))?;
                    Ok(claim.args[pos].clone())
                }
                0 => match default {
                    Some(d) => eval_value(d, state, bindings, actor),
                    None => Err(EvalError::ValueOfZeroMatches(predicate.clone())),
                },
                _ => Err(EvalError::ValueOfMultipleMatches(predicate.clone())),
            }
        }
        _ => Err(EvalError::NotValue),
    }
}

pub(crate) fn resolve_term(
    t: &Term,
    bindings: &Bindings,
    actor: Option<&EvalValue>,
) -> Result<EvalValue, EvalError> {
    match t {
        Term::Var(name) => bindings
            .get(name)
            .cloned()
            .ok_or_else(|| EvalError::UnboundVariable(name.clone())),
        Term::Wildcard => Err(EvalError::TypeMismatch(
            "wildcard cannot be resolved as a value".into(),
        )),
        Term::Literal(Value::Decimal(s)) => {
            let d = Decimal::from_str(s)
                .map_err(|_| EvalError::TypeMismatch(format!("invalid decimal: {s}")))?;
            Ok(EvalValue::Decimal(d))
        }
        Term::Literal(Value::Subject(s)) => Ok(EvalValue::Subject(s.clone())),
        Term::Literal(Value::Date(s)) => Ok(EvalValue::Date(parse_date_literal(s)?)),
        Term::Actor => actor.cloned().ok_or(EvalError::UnboundActor),
    }
}

/// On a failing predicate-shaped expression, return the most specific
/// sub-expression responsible, rendered via
/// [`crate::format::format_expr_inline`]. Returns `None` when no
/// drill-down meaningfully applies.
///
/// Called from [`crate::propose::execute_stmt`] on the rejection
/// branches of `Require` and `BindOne`. Never called on the success
/// path; the kernel's success-path cost is unchanged.
///
/// Drill-down rules (failure-walk, statement-level + one layer):
///
/// - `And(conjuncts)`: find the first conjunct whose `find_matches`
///   is empty under the same bindings; recurse into it. If the
///   recursion yields a more specific answer, use it; otherwise
///   render the conjunct as-is.
/// - `Implies { left, right }`: if `left` held (non-empty matches),
///   the failure is in `right`. Recurse into `right` and fall back
///   to rendering it. If `left` failed, the implies is vacuously
///   true - caller should not have invoked us; return `None` as
///   safety.
/// - `Forall { binding, source, body }`: find the first source-match
///   under which `body` fails, recurse into `body` under that
///   binding context, fall back to rendering `body` as-is. Binding
///   values are **not substituted** into the rendered string in v0
///   (per the PR-G review constraint); the caller correlates the
///   failing iteration separately if needed.
/// - `Not`, `Exists`: return `None`. Structurally these have
///   no single sub-expression that's "the one responsible": `Not`
///   describes what *held* rather than what failed; `Exists` and
///   `Exists` failures mean "no member of the set satisfied", which is
///   the whole expression, not any sub-part.
/// - Leaf expressions (`Claim`, `Le`, `DateLe`, `Eq`, `Neq`, `In`,
///   `Term`, `Sub`, `Add`, `Sum`, `ValueOf`): return `None`. Already
///   as specific as the kernel can be.
///
/// Distinct from a structural ExprTrace mirror of the evaluator
/// (Option B in the PR-G design discussion). This is the smallest
/// useful failure-walk that closes the "which conjunct failed?"
/// diagnostic gap without growing the trace surface beyond
/// statement-level.
pub(crate) fn find_failing_subexpr(
    expr: &Expr,
    state: &State,
    bindings: &Bindings,
    actor: Option<&EvalValue>,
) -> Option<String> {
    match expr {
        Expr::And(conjuncts) => {
            // Thread bindings through conjuncts the same way
            // `find_conjunction` does in the evaluator: each conjunct
            // runs against the contexts produced by the previous one.
            // Evaluating each conjunct against the *original*
            // `bindings` would miss failures that only show up after
            // a prior conjunct narrowed the binding context (e.g.
            // `And(A(x), B(x))` where some `A(a1)` holds and some
            // `B(b2)` holds but no `x` exists where both hold).
            let mut current: Vec<Bindings> = vec![bindings.clone()];
            for c in conjuncts {
                let mut next: Vec<Bindings> = Vec::new();
                for ctx in &current {
                    next.extend(find_matches(c, state, ctx, actor).ok()?);
                }
                if next.is_empty() {
                    // This conjunct kills the chain. Diagnose under
                    // one of the binding contexts that survived to
                    // this point; the first is fine.
                    let failing_ctx = current.first().unwrap_or(bindings);
                    return Some(
                        find_failing_subexpr(c, state, failing_ctx, actor)
                            .unwrap_or_else(|| crate::format::format_expr_inline(c)),
                    );
                }
                current = next;
            }
            None
        }
        Expr::Implies { left, right } => {
            let left_matches = find_matches(left, state, bindings, actor).ok()?;
            if left_matches.is_empty() {
                // Implies is vacuously true when left fails; caller
                // shouldn't have invoked us. Safety: return None.
                return None;
            }
            // The right side rejected under at least one of left's
            // satisfying bindings. Recurse with the first such
            // extension so the drill-down sees the same context the
            // evaluator did.
            for ext in &left_matches {
                let right_matches = find_matches(right, state, ext, actor).ok()?;
                if right_matches.is_empty() {
                    return Some(
                        find_failing_subexpr(right, state, ext, actor)
                            .unwrap_or_else(|| crate::format::format_expr_inline(right)),
                    );
                }
            }
            None
        }
        Expr::Forall {
            binding: _,
            source,
            body,
        } => {
            // Mirror find_matches's Forall: iterate every source
            // binding extension and test the body against it. No
            // `contains_key` filter here - the evaluator does not
            // filter source matches that way, and a walker that
            // diverged from the evaluator's iteration order could
            // identify a "failing" iteration the evaluator never
            // tried.
            let source_matches = find_matches(source, state, bindings, actor).ok()?;
            for ext in &source_matches {
                let body_matches = find_matches(body, state, ext, actor).ok()?;
                if body_matches.is_empty() {
                    return Some(
                        find_failing_subexpr(body, state, ext, actor)
                            .unwrap_or_else(|| crate::format::format_expr_inline(body)),
                    );
                }
            }
            None
        }
        // No useful drill-down for these:
        Expr::Not(_)
        | Expr::Exists { .. }
        | Expr::Claim { .. }
        | Expr::Le(..)
        | Expr::DateLe(..)
        | Expr::Eq(..)
        | Expr::Neq(..)
        | Expr::In(..)
        | Expr::Term(..)
        | Expr::Sub(..)
        | Expr::Add(..)
        | Expr::Sum { .. }
        | Expr::ValueOf { .. } => None,
    }
}
