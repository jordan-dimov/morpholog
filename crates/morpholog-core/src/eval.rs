//! The in-memory evaluator.
//!
//! `find_matches` walks an [`Expr`] against a [`State`] and a binding
//! context, returning either the set of extended binding contexts that
//! satisfy the expression (predicate-shaped use) or a kernel error. Its
//! crate-private helpers are also called from [`crate::propose`] and
//! [`crate::derive`].
//!
//! `EvalError` is raised when an expression is structurally ill-formed
//! (type mismatches, missing variables, ValueOf cardinality violations).
//! Distinct from lawful business rejection, reported as
//! `Outcome::Rejected`.

use jiff::civil::Date;
use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};
use std::str::FromStr;

use crate::ir::{Expr, Term, Value};
use crate::state::{Bindings, EvalValue, State};

/// Errors raised by the evaluator and the transformation runner: an
/// expression or transformation was structurally ill-formed and cannot
/// be run. Distinct from lawful business rejection
/// ([`crate::Outcome::Rejected`]).
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
    /// `Term::Actor` was referenced with no transition in scope (the
    /// evaluator was called with `actor = None`): invariant or
    /// derived-claim bodies, which evaluate against admitted state, not
    /// a proposing transition. Authority checks belong in `require`, not
    /// invariants; this error makes that doctrine enforceable.
    UnboundActor,
    /// `Expr::Pre` was reached with no pre-state in scope: derived-claim
    /// bodies, transformation `require`s, the inner of nested `pre`, or
    /// an `EvalContext` built with `pre_state: None`. Phrased about
    /// evaluation context, not AST position, so future contexts that
    /// carry both states share the primitive without IR change.
    PreStateUnavailable,
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
            EvalError::PreStateUnavailable => write!(
                f,
                "Expr::Pre evaluated with no pre-state in scope (a derived-claim body, a transformation `require`, the inner of nested `pre`, or an EvalContext built with pre_state: None)"
            ),
        }
    }
}

impl std::error::Error for EvalError {}

/// Evaluator context: state(s), bindings, optional actor. Threaded
/// through `find_matches`, `eval_value`, and the helpers that recurse
/// into expression bodies.
#[derive(Clone, Copy)]
pub(crate) struct EvalContext<'a> {
    /// The state predicate lookups resolve against: the candidate (post)
    /// state during proposal-path invariant evaluation, the only state
    /// in one-state contexts, or the pre-transition state inside
    /// `pre(...)`.
    pub(crate) state: &'a State,
    /// Pre-transition state when both states are in scope; `None`
    /// otherwise. Cleared inside a `Pre` subtree so nested
    /// `pre(pre(...))` surfaces `PreStateUnavailable`.
    pub(crate) pre_state: Option<&'a State>,
    pub(crate) bindings: &'a Bindings,
    /// The proposing transition's actor; `None` in one-state contexts.
    /// `Term::Actor` reached with `actor: None` surfaces `UnboundActor`.
    pub(crate) actor: Option<&'a EvalValue>,
}

impl<'a> EvalContext<'a> {
    pub(crate) fn new(
        state: &'a State,
        pre_state: Option<&'a State>,
        bindings: &'a Bindings,
        actor: Option<&'a EvalValue>,
    ) -> Self {
        Self {
            state,
            pre_state,
            bindings,
            actor,
        }
    }

    /// Swap in extended bindings; used when descending into a
    /// conjunct, an `Implies` right side, or a quantifier body.
    pub(crate) fn with_bindings(&self, bindings: &'a Bindings) -> Self {
        Self {
            state: self.state,
            pre_state: self.pre_state,
            bindings,
            actor: self.actor,
        }
    }

    /// Enter an `Expr::Pre` subtree: state becomes the previous
    /// pre-state, pre-state is cleared. `None` if no pre-state was
    /// in scope; caller surfaces `PreStateUnavailable`.
    pub(crate) fn enter_pre(&self) -> Option<Self> {
        Some(Self {
            state: self.pre_state?,
            pre_state: None,
            bindings: self.bindings,
            actor: self.actor,
        })
    }
}

/// Evaluate a decimal comparator. Both operands must resolve to
/// `EvalValue::Decimal`; `admit` decides whether the comparison holds.
/// Predicate-shaped: the unchanged bindings when it holds, empty
/// otherwise. Shared by `Le`/`Lt`/`Ge`/`Gt`.
fn decimal_comparison(
    lhs: &Expr,
    rhs: &Expr,
    ctx: &EvalContext<'_>,
    op: &str,
    admit: impl Fn(Decimal, Decimal) -> bool,
) -> Result<Vec<Bindings>, EvalError> {
    match (eval_value(lhs, ctx)?, eval_value(rhs, ctx)?) {
        (EvalValue::Decimal(a), EvalValue::Decimal(b)) => Ok(if admit(a, b) {
            vec![ctx.bindings.clone()]
        } else {
            vec![]
        }),
        _ => Err(EvalError::TypeMismatch(format!(
            "{op} expects decimal operands"
        ))),
    }
}

/// Civil-date comparator. The mirror of [`decimal_comparison`] for
/// `EvalValue::Date`; shared by `DateLe`/`DateLt`/`DateGe`/`DateGt`.
fn date_comparison(
    lhs: &Expr,
    rhs: &Expr,
    ctx: &EvalContext<'_>,
    op: &str,
    admit: impl Fn(Date, Date) -> bool,
) -> Result<Vec<Bindings>, EvalError> {
    match (eval_value(lhs, ctx)?, eval_value(rhs, ctx)?) {
        (EvalValue::Date(a), EvalValue::Date(b)) => Ok(if admit(a, b) {
            vec![ctx.bindings.clone()]
        } else {
            vec![]
        }),
        _ => Err(EvalError::TypeMismatch(format!(
            "{op} expects civil-date operands"
        ))),
    }
}

pub(crate) fn find_matches(e: &Expr, ctx: &EvalContext<'_>) -> Result<Vec<Bindings>, EvalError> {
    match e {
        Expr::Claim { predicate, args } => find_claim_matches(predicate, args, ctx),
        Expr::And(exprs) => find_conjunction(exprs, ctx),
        Expr::Or(exprs) => find_disjunction(exprs, ctx),
        Expr::Not(inner) => {
            let m = find_matches(inner, ctx)?;
            Ok(if m.is_empty() {
                vec![ctx.bindings.clone()]
            } else {
                vec![]
            })
        }
        Expr::Pre(inner) => {
            let pre_ctx = ctx.enter_pre().ok_or(EvalError::PreStateUnavailable)?;
            find_matches(inner, &pre_ctx)
        }
        Expr::Implies { left, right } => {
            let lm = find_matches(left, ctx)?;
            for m in lm {
                if find_matches(right, &ctx.with_bindings(&m))?.is_empty() {
                    return Ok(vec![]);
                }
            }
            Ok(vec![ctx.bindings.clone()])
        }
        Expr::Exists { binding: _, body } => {
            let m = find_matches(body, ctx)?;
            Ok(if m.is_empty() {
                vec![]
            } else {
                vec![ctx.bindings.clone()]
            })
        }
        Expr::Forall {
            binding: _,
            source,
            body,
        } => {
            let sm = find_matches(source, ctx)?;
            for m in sm {
                if find_matches(body, &ctx.with_bindings(&m))?.is_empty() {
                    return Ok(vec![]);
                }
            }
            Ok(vec![ctx.bindings.clone()])
        }
        Expr::Eq(lhs, rhs) => {
            let l = eval_value(lhs, ctx)?;
            let r = eval_value(rhs, ctx)?;
            Ok(if l == r {
                vec![ctx.bindings.clone()]
            } else {
                vec![]
            })
        }
        Expr::Le(l, r) => decimal_comparison(l, r, ctx, "Le", |a, b| a <= b),
        Expr::Lt(l, r) => decimal_comparison(l, r, ctx, "Lt", |a, b| a < b),
        Expr::Ge(l, r) => decimal_comparison(l, r, ctx, "Ge", |a, b| a >= b),
        Expr::Gt(l, r) => decimal_comparison(l, r, ctx, "Gt", |a, b| a > b),
        Expr::DateLe(l, r) => date_comparison(l, r, ctx, "DateLe", |a, b| a <= b),
        Expr::DateLt(l, r) => date_comparison(l, r, ctx, "DateLt", |a, b| a < b),
        Expr::DateGe(l, r) => date_comparison(l, r, ctx, "DateGe", |a, b| a >= b),
        Expr::DateGt(l, r) => date_comparison(l, r, ctx, "DateGt", |a, b| a > b),
        Expr::Neq(t1, t2) => {
            let l = resolve_term(t1, ctx.bindings, ctx.actor)?;
            let r = resolve_term(t2, ctx.bindings, ctx.actor)?;
            Ok(if l != r {
                vec![ctx.bindings.clone()]
            } else {
                vec![]
            })
        }
        Expr::In(elem, coll) => find_in_matches(elem, coll, ctx),
        Expr::Term(_)
        | Expr::Sub(_, _)
        | Expr::Add(_, _)
        | Expr::Sum { .. }
        | Expr::ValueOf { .. } => Err(EvalError::NotPredicate),
    }
}

/// Parse a `Value::Date(String)` literal into a `jiff::civil::Date`.
/// Centralised so the IR-level literal and the runtime value cannot
/// drift in how they interpret `YYYY-MM-DD`.
pub(crate) fn parse_date_literal(s: &str) -> Result<Date, EvalError> {
    s.parse::<Date>()
        .map_err(|e| EvalError::TypeMismatch(format!("invalid civil date `{s}`: {e}")))
}

pub(crate) fn find_claim_matches(
    predicate: &str,
    args: &[Term],
    ctx: &EvalContext<'_>,
) -> Result<Vec<Bindings>, EvalError> {
    let EvalContext {
        state,
        bindings: base,
        actor,
        ..
    } = *ctx;
    let mut out = vec![];

    // Position-independent actor check: any `Term::Actor` requires an
    // actor in scope. Without it, a selective ground arg appearing
    // earlier could short-circuit to `Ok(empty)` before the loop
    // reaches `Term::Actor`, letting an invariant that references
    // `Term::Actor` silently produce no matches instead of erroring.
    if actor.is_none() && args.iter().any(|t| matches!(t, Term::Actor)) {
        return Err(EvalError::UnboundActor);
    }

    // Pick the most selective ground argument (literal, or var already
    // bound in `base`) and scan its (predicate, position, value) bucket
    // rather than the whole predicate. For `JournalLine(entry, _, d, _)`
    // inside `forall entry: ...`, the bound `entry` narrows the scan to
    // that entry's lines - O(lines_per_entry) instead of O(all lines).
    // A missing bucket short-circuits to empty; no ground arg falls
    // back to `state.claims_for(predicate)`.
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
    ctx: &EvalContext<'_>,
) -> Result<Vec<Bindings>, EvalError> {
    let mut current = vec![ctx.bindings.clone()];
    for expr in exprs {
        let mut next = vec![];
        for b in &current {
            next.extend(find_matches(expr, &ctx.with_bindings(b))?);
        }
        if next.is_empty() {
            return Ok(vec![]);
        }
        current = next;
    }
    Ok(current)
}

/// Evaluate a disjunction by concatenating the binding sets each
/// branch produces against the same base context. Empty when every
/// branch is empty. No deduplication - if two branches admit the
/// same extension, both copies appear, mirroring `find_conjunction`'s
/// multiplicity-preserving convention.
pub(crate) fn find_disjunction(
    exprs: &[Expr],
    ctx: &EvalContext<'_>,
) -> Result<Vec<Bindings>, EvalError> {
    let mut out = vec![];
    for expr in exprs {
        out.extend(find_matches(expr, ctx)?);
    }
    Ok(out)
}

pub(crate) fn find_in_matches(
    elem: &Term,
    coll: &Term,
    ctx: &EvalContext<'_>,
) -> Result<Vec<Bindings>, EvalError> {
    let base = ctx.bindings;
    let actor = ctx.actor;
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

pub(crate) fn eval_value(e: &Expr, ctx: &EvalContext<'_>) -> Result<EvalValue, EvalError> {
    match e {
        Expr::Term(t) => resolve_term(t, ctx.bindings, ctx.actor),
        Expr::Sub(lhs, rhs) => {
            let l = eval_value(lhs, ctx)?;
            let r = eval_value(rhs, ctx)?;
            match (l, r) {
                (EvalValue::Decimal(a), EvalValue::Decimal(b)) => Ok(EvalValue::Decimal(a - b)),
                _ => Err(EvalError::TypeMismatch(
                    "Sub expects decimal operands".into(),
                )),
            }
        }
        Expr::Add(lhs, rhs) => {
            let l = eval_value(lhs, ctx)?;
            let r = eval_value(rhs, ctx)?;
            match (l, r) {
                (EvalValue::Decimal(a), EvalValue::Decimal(b)) => Ok(EvalValue::Decimal(a + b)),
                _ => Err(EvalError::TypeMismatch(
                    "Add expects decimal operands".into(),
                )),
            }
        }
        Expr::Sum { value, body } => {
            let matches = find_matches(body, ctx)?;
            let mut total = Decimal::ZERO;
            for m in matches {
                match resolve_term(value, &m, ctx.actor)? {
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
            let matches = find_claim_matches(predicate, args, ctx)?;
            match matches.len() {
                1 => {
                    let pos = args
                        .iter()
                        .position(|t| matches!(t, Term::Wildcard))
                        .ok_or_else(|| {
                            EvalError::TypeMismatch("ValueOf requires a wildcard arg".into())
                        })?;
                    let claim = ctx
                        .state
                        .claims_for(predicate)
                        .find(|f| {
                            f.args.len() == args.len()
                                && unify_args(args, &f.args, ctx.bindings, ctx.actor).is_some()
                        })
                        .ok_or_else(|| EvalError::ValueOfZeroMatches(predicate.clone()))?;
                    Ok(claim.args[pos].clone())
                }
                0 => match default {
                    Some(d) => eval_value(d, ctx),
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
/// branches of `Require` and `BindOne`. Never on the success path, so
/// success-path cost is unchanged.
///
/// Drill-down rules (statement-level plus one layer):
///
/// - `And(conjuncts)`: recurse into the first conjunct whose
///   `find_matches` is empty under the same bindings; render it as-is
///   if the recursion yields nothing more specific.
/// - `Implies { left, right }`: if `left` held, recurse into `right`.
///   If `left` failed, the implies is vacuously true - return `None`.
/// - `Forall { binding, source, body }`: recurse into `body` under the
///   first source-match where it fails. Binding values are **not**
///   substituted into the rendered string in v0.
/// - `Not`, `Exists`, `Or`: return `None`. No single sub-expression is
///   "the one responsible": `Not` describes what *held*; `Exists`
///   failure means no member satisfied; `Or` failure means every
///   branch failed.
/// - Leaf expressions: return `None`, already as specific as possible.
pub(crate) fn find_failing_subexpr(expr: &Expr, ctx: &EvalContext<'_>) -> Option<String> {
    match expr {
        Expr::And(conjuncts) => {
            // Thread bindings through conjuncts as `find_conjunction`
            // does: each runs against the contexts the previous produced.
            // Evaluating each against the original `bindings` would miss
            // failures that only appear after a prior conjunct narrowed
            // the context (e.g. `And(A(x), B(x))` where `A(a1)` and
            // `B(b2)` each hold but no `x` satisfies both).
            let mut current: Vec<Bindings> = vec![ctx.bindings.clone()];
            for c in conjuncts {
                let mut next: Vec<Bindings> = Vec::new();
                for b in &current {
                    next.extend(find_matches(c, &ctx.with_bindings(b)).ok()?);
                }
                if next.is_empty() {
                    // This conjunct kills the chain. Diagnose under one
                    // of the surviving binding contexts; the first is fine.
                    let failing_bindings = current.first().unwrap_or(ctx.bindings);
                    return Some(
                        find_failing_subexpr(c, &ctx.with_bindings(failing_bindings))
                            .unwrap_or_else(|| crate::format::format_expr_inline(c)),
                    );
                }
                current = next;
            }
            None
        }
        Expr::Implies { left, right } => {
            let left_matches = find_matches(left, ctx).ok()?;
            if left_matches.is_empty() {
                // Vacuously true when left fails; return None as safety.
                return None;
            }
            // Recurse into right under the first of left's satisfying
            // bindings, so the drill-down sees the evaluator's context.
            for ext in &left_matches {
                let ext_ctx = ctx.with_bindings(ext);
                let right_matches = find_matches(right, &ext_ctx).ok()?;
                if right_matches.is_empty() {
                    return Some(
                        find_failing_subexpr(right, &ext_ctx)
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
            // extension and test the body. No `contains_key` filter -
            // diverging from the evaluator's iteration order could blame
            // a "failing" iteration the evaluator never tried.
            let source_matches = find_matches(source, ctx).ok()?;
            for ext in &source_matches {
                let ext_ctx = ctx.with_bindings(ext);
                let body_matches = find_matches(body, &ext_ctx).ok()?;
                if body_matches.is_empty() {
                    return Some(
                        find_failing_subexpr(body, &ext_ctx)
                            .unwrap_or_else(|| crate::format::format_expr_inline(body)),
                    );
                }
            }
            None
        }
        // No useful drill-down for these:
        Expr::Not(_)
        | Expr::Or(_)
        | Expr::Pre(_)
        | Expr::Exists { .. }
        | Expr::Claim { .. }
        | Expr::Le(..)
        | Expr::Lt(..)
        | Expr::Ge(..)
        | Expr::Gt(..)
        | Expr::DateLe(..)
        | Expr::DateLt(..)
        | Expr::DateGe(..)
        | Expr::DateGt(..)
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

/// A claim-shaped gate conjunct that did not match, rendered with its
/// arguments resolved under the binding context live at the rejection.
///
/// Carried structurally on the rejection trace (see
/// [`crate::RequireOutcome`]) so the explanation engine can attach
/// candidate suppliers by predicate without re-deriving bindings.
/// `predicate` is kept separate from `rendered` precisely so supplier
/// lookup is by predicate name, not by parsing the rendered string.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RenderedClaim {
    pub predicate: String,
    pub rendered: String,
}

/// On a failing predicate-shaped gate expression, return the positive
/// claim conjuncts directly responsible for the failure - the
/// "directly missing claims" the explanation engine reports. Mirrors
/// [`find_failing_subexpr`]'s conjunction threading so the binding flow
/// matches the kernel's `And` semantics exactly (a conjunct is evaluated
/// against the contexts the previous conjuncts produced, not the entry
/// bindings).
///
/// Scope is deliberately narrow in v0:
///
/// - a top-level [`Expr::Claim`] that did not match -> that claim;
/// - a top-level [`Expr::And`] -> the first conjunct that kills the
///   chain, *if and only if* it is itself a positive `Claim`.
///
/// Everything else returns empty: `Or`, `Not`, `Exists`, `Implies`,
/// `Forall`, the comparators, `ValueOf`, `Pre`, `Term`, and an `And`
/// whose chain-killing conjunct is not a positive claim. Those are
/// faithful rejections without a directly-missing claim. Present
/// blockers (`not X` where `X` holds), comparator failures, and
/// bounded abduction are deliberately out of scope - surfacing them
/// would mean explaining the *semantics* of failure, a later tier.
///
/// Called only on the rejection branch, like `find_failing_subexpr`, so
/// the success path pays nothing.
pub(crate) fn unsatisfied_positive_claims(
    expr: &Expr,
    ctx: &EvalContext<'_>,
) -> Vec<RenderedClaim> {
    match expr {
        Expr::Claim { .. } => match find_matches(expr, ctx) {
            // Reached only because the gate failed; guard anyway so this
            // never reports a claim that actually matched.
            Ok(m) if m.is_empty() => vec![render_claim(expr, ctx)],
            _ => vec![],
        },
        Expr::And(conjuncts) => {
            let mut current: Vec<Bindings> = vec![ctx.bindings.clone()];
            for c in conjuncts {
                let mut next: Vec<Bindings> = Vec::new();
                for b in &current {
                    match find_matches(c, &ctx.with_bindings(b)) {
                        Ok(ms) => next.extend(ms),
                        // An evaluator error mid-chain is a kernel error,
                        // not a missing claim; leave it to the error path.
                        Err(_) => return vec![],
                    }
                }
                if next.is_empty() {
                    // `c` killed the chain. Report it only when it is
                    // itself a positive claim; a comparator/`not`/etc.
                    // failure has no directly-missing claim in v0.
                    let failing_bindings = current.first().unwrap_or(ctx.bindings);
                    if matches!(c, Expr::Claim { .. }) {
                        return vec![render_claim(c, &ctx.with_bindings(failing_bindings))];
                    }
                    return vec![];
                }
                current = next;
            }
            vec![]
        }
        _ => vec![],
    }
}

/// Render an `Expr::Claim` with its arguments resolved under `ctx`'s
/// live bindings - `MayApprove(alice, contract)`, not
/// `MayApprove(actor, doc_type)`. A term that does not resolve (an
/// unbound variable) falls back to its symbolic form. Panics if handed a
/// non-`Claim`; the only callers pass a `Claim`.
fn render_claim(expr: &Expr, ctx: &EvalContext<'_>) -> RenderedClaim {
    let Expr::Claim { predicate, args } = expr else {
        unreachable!("render_claim is only called on Expr::Claim")
    };
    let rendered_args: Vec<String> = args.iter().map(|t| render_term(t, ctx)).collect();
    RenderedClaim {
        predicate: predicate.clone(),
        rendered: format!("{}({})", predicate, rendered_args.join(", ")),
    }
}

/// Resolve a term to its value under `ctx` and render it; fall back to
/// the term's symbolic form when it cannot be resolved.
fn render_term(t: &Term, ctx: &EvalContext<'_>) -> String {
    match resolve_term(t, ctx.bindings, ctx.actor) {
        Ok(v) => render_eval_value(&v),
        Err(_) => match t {
            Term::Var(name) => name.clone(),
            Term::Wildcard => "_".to_string(),
            Term::Actor => "actor".to_string(),
            // Literals always resolve, so this arm is unreachable in
            // practice; render defensively rather than panic.
            Term::Literal(_) => "?".to_string(),
        },
    }
}

/// Render a runtime value to a short human string for explanations and
/// trace prose. Subjects and decimals render as their bare text; dates
/// as ISO-8601; collections bracketed.
pub(crate) fn render_eval_value(v: &EvalValue) -> String {
    match v {
        EvalValue::Subject(s) => s.clone(),
        EvalValue::Decimal(d) => d.to_string(),
        EvalValue::Bool(b) => b.to_string(),
        EvalValue::Date(d) => d.to_string(),
        EvalValue::Collection(items) => {
            let inner: Vec<String> = items.iter().map(render_eval_value).collect();
            format!("[{}]", inner.join(", "))
        }
    }
}
