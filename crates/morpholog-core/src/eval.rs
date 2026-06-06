//! The in-memory evaluator.
//!
//! `find_matches` walks a [`Prop`] against a [`State`] and a binding
//! context, returning the set of extended binding contexts that satisfy
//! the proposition, or a kernel error. `eval_value` walks a [`ValueExpr`]
//! and returns the single value it computes, or a kernel error. Each is
//! total over its sort - there is no wrong-shape arm, because the IR
//! makes a value expression at a predicate position (or the reverse)
//! unrepresentable. Their crate-private helpers are also called from
//! [`crate::propose`] and [`crate::derive`].
//!
//! `EvalError` is raised when an expression is structurally ill-formed
//! (type mismatches, missing variables, ValueOf cardinality violations).
//! Distinct from lawful business rejection, reported as
//! `Outcome::Rejected`.

use jiff::civil::Date;
use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};
use std::str::FromStr;

use crate::ir::{
    ArithOp, CompareOp, OrderedDomain, PredicateName, Prop, Subject, Term, Value, ValueExpr,
};
use crate::state::{Bindings, ClaimInstance, EvalValue, State};

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
    /// `ValueExpr::ValueOf(predicate, args)` matched zero claims and no
    /// `default` was supplied.
    ValueOfZeroMatches(String),
    /// `ValueExpr::ValueOf(predicate, args)` matched more than one claim;
    /// the functional-lookup contract requires exactly one match.
    ValueOfMultipleMatches(String),
    /// `Term::Actor` was referenced with no transition in scope (the
    /// evaluator was called with `actor = None`): invariant or
    /// derived-claim bodies, which evaluate against admitted state, not
    /// a proposing transition. Authority checks belong in `require`, not
    /// invariants; this error makes that doctrine enforceable.
    UnboundActor,
    /// `Prop::Pre` was reached with no pre-state in scope: derived-claim
    /// bodies, transformation `require`s, the inner of nested `pre`, or
    /// an `EvalContext` built with `pre_state: None`. Phrased about
    /// evaluation context, not AST position, so future contexts that
    /// carry both states share the primitive without IR change.
    PreStateUnavailable,
    /// An `ArithOp::Div` or `ArithOp::Mod` evaluated with a zero divisor.
    /// A rule that divides (or takes a remainder) by zero cannot be
    /// evaluated, so it surfaces here rather than producing a value; the
    /// proposal is rejected (or the derived read errors). Gates avoid this
    /// by cross-multiplying with `Mul`.
    DivisionByZero,
}

impl std::fmt::Display for EvalError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            EvalError::UnboundVariable(name) => write!(f, "unbound variable: {name}"),
            EvalError::TypeMismatch(msg) => write!(f, "type mismatch: {msg}"),
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
                "Prop::Pre evaluated with no pre-state in scope (a derived-claim body, a transformation `require`, the inner of nested `pre`, or an EvalContext built with pre_state: None)"
            ),
            EvalError::DivisionByZero => write!(f, "division by zero"),
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
    pub(crate) actor: Option<&'a Subject>,
}

impl<'a> EvalContext<'a> {
    pub(crate) fn new(
        state: &'a State,
        pre_state: Option<&'a State>,
        bindings: &'a Bindings,
        actor: Option<&'a Subject>,
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

    /// Enter a `Prop::Pre` subtree: state becomes the previous
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

/// Evaluate an ordered comparison. Both operands must resolve to the
/// `domain`'s runtime kind (`EvalValue::Decimal` or `EvalValue::Date`);
/// `op` decides whether the comparison holds. Predicate-shaped: the
/// unchanged bindings when it holds, empty otherwise.
fn ordered_comparison(
    left: &ValueExpr,
    right: &ValueExpr,
    op: CompareOp,
    domain: OrderedDomain,
    ctx: &EvalContext<'_>,
) -> Result<Vec<Bindings>, EvalError> {
    let holds = match (domain, eval_value(left, ctx)?, eval_value(right, ctx)?) {
        (OrderedDomain::Decimal, EvalValue::Decimal(a), EvalValue::Decimal(b)) => {
            apply_cmp(op, a, b)
        }
        (OrderedDomain::Date, EvalValue::Date(a), EvalValue::Date(b)) => apply_cmp(op, a, b),
        (OrderedDomain::Decimal, _, _) => {
            return Err(EvalError::TypeMismatch(
                "comparison expects decimal operands".to_string(),
            ));
        }
        (OrderedDomain::Date, _, _) => {
            return Err(EvalError::TypeMismatch(
                "comparison expects civil-date operands".to_string(),
            ));
        }
    };
    Ok(if holds {
        vec![ctx.bindings.clone()]
    } else {
        vec![]
    })
}

/// Apply a [`CompareOp`] to two operands of an ordered domain.
fn apply_cmp<T: PartialOrd>(op: CompareOp, a: T, b: T) -> bool {
    match op {
        CompareOp::Le => a <= b,
        CompareOp::Lt => a < b,
        CompareOp::Ge => a >= b,
        CompareOp::Gt => a > b,
    }
}

/// `a xor b` is defined as exactly-one: `(a or b) and not (a and b)`.
/// Lowering to that combination keeps XOR's binding semantics identical
/// to the hand-written form - it is a spelling, not new evaluation. The
/// single definition of what xor expands to: `find_matches` evaluates it,
/// and the validator measures *this* shape's depth (not the one binary
/// node) so a deep xor cannot pass the depth guard yet overflow eval.
pub(crate) fn lower_xor(left: &Prop, right: &Prop) -> Prop {
    Prop::And(vec![
        Prop::Or(vec![left.clone(), right.clone()]),
        Prop::Not(Box::new(Prop::And(vec![left.clone(), right.clone()]))),
    ])
}

pub(crate) fn find_matches(p: &Prop, ctx: &EvalContext<'_>) -> Result<Vec<Bindings>, EvalError> {
    match p {
        Prop::Claim { predicate, args } => find_claim_matches(predicate, args, ctx),
        Prop::And(props) => find_conjunction(props, ctx),
        Prop::Or(props) => find_disjunction(props, ctx),
        Prop::Xor(left, right) => find_matches(&lower_xor(left, right), ctx),
        Prop::Not(inner) => {
            let m = find_matches(inner, ctx)?;
            Ok(if m.is_empty() {
                vec![ctx.bindings.clone()]
            } else {
                vec![]
            })
        }
        Prop::Pre(inner) => {
            let pre_ctx = ctx.enter_pre().ok_or(EvalError::PreStateUnavailable)?;
            find_matches(inner, &pre_ctx)
        }
        Prop::Implies { left, right } => {
            let lm = find_matches(left, ctx)?;
            for m in lm {
                if find_matches(right, &ctx.with_bindings(&m))?.is_empty() {
                    return Ok(vec![]);
                }
            }
            Ok(vec![ctx.bindings.clone()])
        }
        Prop::Exists { binding: _, body } => {
            let m = find_matches(body, ctx)?;
            Ok(if m.is_empty() {
                vec![]
            } else {
                vec![ctx.bindings.clone()]
            })
        }
        Prop::Forall {
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
        Prop::Eq(lhs, rhs) => {
            let l = eval_value(lhs, ctx)?;
            let r = eval_value(rhs, ctx)?;
            Ok(if l == r {
                vec![ctx.bindings.clone()]
            } else {
                vec![]
            })
        }
        Prop::Compare {
            op,
            domain,
            left,
            right,
        } => ordered_comparison(left, right, *op, *domain, ctx),
        Prop::Neq(lhs, rhs) => {
            let l = eval_value(lhs, ctx)?;
            let r = eval_value(rhs, ctx)?;
            Ok(if l != r {
                vec![ctx.bindings.clone()]
            } else {
                vec![]
            })
        }
        Prop::In(elem, coll) => find_in_matches(elem, coll, ctx),
    }
}

/// Parse a `Value::Date(String)` literal into a `jiff::civil::Date`.
/// Centralised so the IR-level literal and the runtime value cannot
/// drift in how they interpret `YYYY-MM-DD`.
pub(crate) fn parse_date_literal(s: &str) -> Result<Date, EvalError> {
    s.parse::<Date>()
        .map_err(|e| EvalError::TypeMismatch(format!("invalid civil date `{s}`: {e}")))
}

/// The claims worth checking when matching `predicate(args)` against
/// state, after narrowing by the most selective ground argument.
/// Computed once and reused by both [`find_claim_matches`] (which
/// collects the satisfying bindings) and the `ValueOf` value-lookup
/// (which keeps the matched claim), so the ground-argument narrowing
/// lives in exactly one place.
enum Candidates<'a> {
    /// A ground argument named a `(predicate, position, value)` bucket
    /// that does not exist, so no admitted claim can match.
    None,
    /// The narrowed bucket of `State::claims()` indices to check.
    Indexed(&'a [usize]),
    /// No ground argument to narrow on; every claim of this predicate
    /// is a candidate.
    All,
}

/// Narrow `predicate(args)` to its candidate claims by the most
/// selective ground argument (a literal, a variable already bound in
/// `base`, or `actor`). For `JournalLine(entry, _, d, _)` inside
/// `forall entry: ...`, the bound `entry` narrows the scan to that
/// entry's lines - O(lines_per_entry) instead of O(all lines). A
/// missing bucket short-circuits to [`Candidates::None`]; no ground
/// argument falls back to [`Candidates::All`].
///
/// Raises `UnboundActor` position-independently if any `Term::Actor`
/// appears with no actor in scope: without the up-front check a
/// selective earlier arg could short-circuit before the loop reached
/// `Term::Actor`, letting a body that references it silently produce
/// no matches instead of erroring.
fn select_candidates<'a>(
    predicate: &PredicateName,
    args: &[Term],
    ctx: &EvalContext<'a>,
) -> Result<Candidates<'a>, EvalError> {
    let EvalContext {
        state,
        bindings: base,
        actor,
        ..
    } = *ctx;

    if actor.is_none() && args.iter().any(|t| matches!(t, Term::Actor)) {
        return Err(EvalError::UnboundActor);
    }

    let mut best: Option<&[usize]> = None;
    for (pos, term) in args.iter().enumerate() {
        let ground = match term {
            Term::Wildcard => None,
            Term::Var(name) => base.get(name).cloned(),
            Term::Literal(Value::Subject(s)) => Some(EvalValue::Subject(s.clone())),
            Term::Literal(Value::Decimal(s)) => Decimal::from_str(s).ok().map(EvalValue::Decimal),
            Term::Literal(Value::Date(s)) => parse_date_literal(s).ok().map(EvalValue::Date),
            Term::Actor => match actor {
                Some(a) => Some(EvalValue::Subject(a.clone())),
                None => return Err(EvalError::UnboundActor),
            },
        };
        let Some(value) = ground else {
            continue;
        };
        match state.claim_indices_for_arg(predicate, pos, &value) {
            None => return Ok(Candidates::None),
            Some(bucket) => match best {
                Some(prev) if prev.len() <= bucket.len() => {}
                _ => best = Some(bucket),
            },
        }
    }

    Ok(best.map_or(Candidates::All, Candidates::Indexed))
}

pub(crate) fn find_claim_matches(
    predicate: &PredicateName,
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
    match select_candidates(predicate, args, ctx)? {
        Candidates::None => {}
        Candidates::Indexed(bucket) => {
            for &i in bucket {
                let claim = state.claim_at(i);
                if claim.args.len() != args.len() {
                    continue;
                }
                if let Some(b) = unify_args(args, &claim.args, base, actor) {
                    out.push(b);
                }
            }
        }
        Candidates::All => {
            for claim in state.claims_for_name(predicate) {
                if claim.args.len() != args.len() {
                    continue;
                }
                if let Some(b) = unify_args(args, &claim.args, base, actor) {
                    out.push(b);
                }
            }
        }
    }
    Ok(out)
}

/// The admitted claims of `predicate` whose args unify with `args`
/// under `ctx`, cloned. Shares the same ground-argument narrowing as
/// [`find_claim_matches`] via [`select_candidates`], but returns the
/// matched claims themselves rather than the bindings they would
/// extend - what the retract path needs to record what it removed.
pub(crate) fn matching_claims(
    predicate: &PredicateName,
    args: &[Term],
    ctx: &EvalContext<'_>,
) -> Result<Vec<ClaimInstance>, EvalError> {
    let EvalContext {
        state,
        bindings: base,
        actor,
        ..
    } = *ctx;
    let mut out = vec![];
    match select_candidates(predicate, args, ctx)? {
        Candidates::None => {}
        Candidates::Indexed(bucket) => {
            for &i in bucket {
                let claim = state.claim_at(i);
                if claim.args.len() == args.len()
                    && unify_args(args, &claim.args, base, actor).is_some()
                {
                    out.push(claim.clone());
                }
            }
        }
        Candidates::All => {
            for claim in state.claims_for_name(predicate) {
                if claim.args.len() == args.len()
                    && unify_args(args, &claim.args, base, actor).is_some()
                {
                    out.push(claim.clone());
                }
            }
        }
    }
    Ok(out)
}

pub(crate) fn unify_args(
    patterns: &[Term],
    values: &[EvalValue],
    base: &Bindings,
    actor: Option<&Subject>,
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
                Some(a) if matches!(v, EvalValue::Subject(s) if s == a) => {}
                _ => return None,
            },
        }
    }
    Some(b)
}

pub(crate) fn find_conjunction(
    props: &[Prop],
    ctx: &EvalContext<'_>,
) -> Result<Vec<Bindings>, EvalError> {
    let mut current = vec![ctx.bindings.clone()];
    for prop in props {
        let mut next = vec![];
        for b in &current {
            next.extend(find_matches(prop, &ctx.with_bindings(b))?);
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
    props: &[Prop],
    ctx: &EvalContext<'_>,
) -> Result<Vec<Bindings>, EvalError> {
    let mut out = vec![];
    for prop in props {
        out.extend(find_matches(prop, ctx)?);
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
    let EvalValue::Collection(items) = coll_val else {
        return Err(EvalError::TypeMismatch("In expects a collection".into()));
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

pub(crate) fn eval_value(e: &ValueExpr, ctx: &EvalContext<'_>) -> Result<EvalValue, EvalError> {
    match e {
        ValueExpr::Term(t) => resolve_term(t, ctx.bindings, ctx.actor),
        ValueExpr::Arith { op, left, right } => {
            let l = eval_value(left, ctx)?;
            let r = eval_value(right, ctx)?;
            match (l, r) {
                (EvalValue::Decimal(a), EvalValue::Decimal(b)) => {
                    let result = match op {
                        ArithOp::Add => a + b,
                        ArithOp::Sub => a - b,
                        ArithOp::Mul => a * b,
                        ArithOp::Div => {
                            if b == Decimal::ZERO {
                                return Err(EvalError::DivisionByZero);
                            }
                            a / b
                        }
                        ArithOp::Mod => {
                            if b == Decimal::ZERO {
                                return Err(EvalError::DivisionByZero);
                            }
                            a % b
                        }
                        ArithOp::Min => a.min(b),
                        ArithOp::Max => a.max(b),
                    };
                    Ok(EvalValue::Decimal(result))
                }
                _ => Err(EvalError::TypeMismatch(format!(
                    "{op:?} expects decimal operands"
                ))),
            }
        }
        ValueExpr::Sum { value, body } => {
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
        ValueExpr::ValueOf {
            predicate,
            args,
            default,
        } => {
            // The wildcard position is the value to extract. A single
            // indexed pass over the narrowed candidates finds the
            // matching claim and reads that position - the same match
            // semantics as `find_claim_matches`, but keeping the claim
            // instead of re-locating it with a second, unindexed scan.
            let pos = args
                .iter()
                .position(|t| matches!(t, Term::Wildcard))
                .ok_or_else(|| EvalError::TypeMismatch("ValueOf requires a wildcard arg".into()))?;

            let mut matched: Option<&EvalValue> = None;
            let mut multiple = false;
            match select_candidates(predicate, args, ctx)? {
                Candidates::None => {}
                Candidates::Indexed(bucket) => {
                    for &i in bucket {
                        let claim = ctx.state.claim_at(i);
                        if claim.args.len() == args.len()
                            && unify_args(args, &claim.args, ctx.bindings, ctx.actor).is_some()
                        {
                            multiple |= matched.is_some();
                            matched = Some(&claim.args[pos]);
                        }
                    }
                }
                Candidates::All => {
                    for claim in ctx.state.claims_for_name(predicate) {
                        if claim.args.len() == args.len()
                            && unify_args(args, &claim.args, ctx.bindings, ctx.actor).is_some()
                        {
                            multiple |= matched.is_some();
                            matched = Some(&claim.args[pos]);
                        }
                    }
                }
            }

            if multiple {
                return Err(EvalError::ValueOfMultipleMatches(predicate.to_string()));
            }
            match matched {
                Some(value) => Ok(value.clone()),
                None => match default {
                    Some(d) => eval_value(d, ctx),
                    None => Err(EvalError::ValueOfZeroMatches(predicate.to_string())),
                },
            }
        }
    }
}

pub(crate) fn resolve_term(
    t: &Term,
    bindings: &Bindings,
    actor: Option<&Subject>,
) -> Result<EvalValue, EvalError> {
    match t {
        Term::Var(name) => bindings
            .get(name)
            .cloned()
            .ok_or_else(|| EvalError::UnboundVariable(name.to_string())),
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
        Term::Actor => actor
            .map(|a| EvalValue::Subject(a.clone()))
            .ok_or(EvalError::UnboundActor),
    }
}

/// On a failing proposition, return the most specific sub-proposition
/// responsible, rendered via
/// [`crate::format::format_prop_inline`]. Returns `None` when no
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
pub(crate) fn find_failing_subexpr(prop: &Prop, ctx: &EvalContext<'_>) -> Option<String> {
    match prop {
        Prop::And(conjuncts) => {
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
                            .unwrap_or_else(|| crate::format::format_prop_inline(c)),
                    );
                }
                current = next;
            }
            None
        }
        Prop::Implies { left, right } => {
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
                            .unwrap_or_else(|| crate::format::format_prop_inline(right)),
                    );
                }
            }
            None
        }
        Prop::Forall {
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
                            .unwrap_or_else(|| crate::format::format_prop_inline(body)),
                    );
                }
            }
            None
        }
        // No useful drill-down for these:
        Prop::Not(_)
        | Prop::Or(_)
        | Prop::Xor(..)
        | Prop::Pre(_)
        | Prop::Exists { .. }
        | Prop::Claim { .. }
        | Prop::Compare { .. }
        | Prop::Eq(..)
        | Prop::Neq(..)
        | Prop::In(..) => None,
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
/// - a top-level [`Prop::Claim`] that did not match -> that claim;
/// - a top-level [`Prop::And`] -> the first conjunct that kills the
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
    prop: &Prop,
    ctx: &EvalContext<'_>,
) -> Vec<RenderedClaim> {
    match prop {
        Prop::Claim { .. } => match find_matches(prop, ctx) {
            // Reached only because the gate failed; guard anyway so this
            // never reports a claim that actually matched.
            Ok(m) if m.is_empty() => vec![render_claim(prop, ctx)],
            _ => vec![],
        },
        Prop::And(conjuncts) => {
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
                    if matches!(c, Prop::Claim { .. }) {
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

/// Render a `Prop::Claim` with its arguments resolved under `ctx`'s
/// live bindings - `MayApprove(alice, contract)`, not
/// `MayApprove(actor, doc_type)`. A term that does not resolve (an
/// unbound variable) falls back to its symbolic form. Panics if handed a
/// non-`Claim`; the only callers pass a `Claim`.
fn render_claim(prop: &Prop, ctx: &EvalContext<'_>) -> RenderedClaim {
    let Prop::Claim { predicate, args } = prop else {
        unreachable!("render_claim is only called on Prop::Claim")
    };
    let rendered_args: Vec<String> = args.iter().map(|t| render_term(t, ctx)).collect();
    RenderedClaim {
        predicate: predicate.to_string(),
        rendered: format!("{}({})", predicate, rendered_args.join(", ")),
    }
}

/// Resolve a term to its value under `ctx` and render it; fall back to
/// the term's symbolic form when it cannot be resolved.
fn render_term(t: &Term, ctx: &EvalContext<'_>) -> String {
    match resolve_term(t, ctx.bindings, ctx.actor) {
        Ok(v) => render_eval_value(&v),
        Err(_) => match t {
            Term::Var(name) => name.to_string(),
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
        EvalValue::Subject(s) => s.to_string(),
        EvalValue::Decimal(d) => d.to_string(),
        EvalValue::Bool(b) => b.to_string(),
        EvalValue::Date(d) => d.to_string(),
        EvalValue::Collection(items) => {
            let inner: Vec<String> = items.iter().map(render_eval_value).collect();
            format!("[{}]", inner.join(", "))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ir_builder::{
        dec, div, max, min, modulo, mul, subj, term, value_of, value_of_with_default, wildcard,
    };
    use crate::state::{ClaimInstance, State};

    // Evaluate a literal-only value expression against empty state/bindings.
    fn eval_lit(e: &ValueExpr) -> Result<EvalValue, EvalError> {
        let state = State::default();
        let bindings = Bindings::new();
        let ctx = EvalContext::new(&state, None, &bindings, None);
        eval_value(e, &ctx)
    }

    #[test]
    fn mul_multiplies_decimal_operands_exactly() {
        assert_eq!(
            eval_lit(&mul(term(dec("3")), term(dec("4")))).unwrap(),
            eval_lit(&term(dec("12"))).unwrap(),
        );
    }

    #[test]
    fn div_divides_decimal_operands() {
        assert_eq!(
            eval_lit(&div(term(dec("12")), term(dec("4")))).unwrap(),
            eval_lit(&term(dec("3"))).unwrap(),
        );
    }

    #[test]
    fn div_by_zero_surfaces_division_by_zero() {
        assert!(matches!(
            eval_lit(&div(term(dec("10")), term(dec("0")))),
            Err(EvalError::DivisionByZero)
        ));
    }

    #[test]
    fn modulo_takes_the_decimal_remainder() {
        // 7 % 2 = 1 - the parity case the chess example relies on.
        assert_eq!(
            eval_lit(&modulo(term(dec("7")), term(dec("2")))).unwrap(),
            eval_lit(&term(dec("1"))).unwrap(),
        );
    }

    #[test]
    fn modulo_by_zero_surfaces_division_by_zero() {
        assert!(matches!(
            eval_lit(&modulo(term(dec("10")), term(dec("0")))),
            Err(EvalError::DivisionByZero)
        ));
    }

    #[test]
    fn modulo_rejects_non_decimal_operands() {
        assert!(matches!(
            eval_lit(&modulo(term(subj("x")), term(dec("2")))),
            Err(EvalError::TypeMismatch(_))
        ));
    }

    #[test]
    fn mul_rejects_non_decimal_operands() {
        assert!(matches!(
            eval_lit(&mul(term(subj("x")), term(dec("2")))),
            Err(EvalError::TypeMismatch(_))
        ));
    }

    #[test]
    fn min_takes_the_lesser_operand() {
        assert_eq!(
            eval_lit(&min(term(dec("3")), term(dec("4")))).unwrap(),
            eval_lit(&term(dec("3"))).unwrap(),
        );
    }

    #[test]
    fn max_takes_the_greater_operand() {
        assert_eq!(
            eval_lit(&max(term(dec("3")), term(dec("4")))).unwrap(),
            eval_lit(&term(dec("4"))).unwrap(),
        );
    }

    #[test]
    fn min_rejects_non_decimal_operands() {
        assert!(matches!(
            eval_lit(&min(term(subj("x")), term(dec("2")))),
            Err(EvalError::TypeMismatch(_))
        ));
    }

    // ValueOf: pins the single-indexed-pass behaviour that the
    // `select_candidates` extraction shares with `find_claim_matches`.
    // The double-entry example the bench uses has no ValueOf, so these
    // are where the changed path's semantics are nailed down.

    /// `Price(trade, amount)` claims for the given (trade, amount) rows.
    fn price_state(rows: &[(&str, i64)]) -> State {
        State::from_claims(
            rows.iter()
                .map(|(t, amt)| ClaimInstance {
                    predicate: "Price".into(),
                    args: vec![
                        EvalValue::Subject(Subject::from(*t)),
                        EvalValue::Decimal(Decimal::new(*amt, 0)),
                    ],
                })
                .collect(),
        )
    }

    fn eval_in(
        e: &ValueExpr,
        state: &State,
        actor: Option<&Subject>,
    ) -> Result<EvalValue, EvalError> {
        let bindings = Bindings::new();
        let ctx = EvalContext::new(state, None, &bindings, actor);
        eval_value(e, &ctx)
    }

    #[test]
    fn value_of_single_match_returns_wildcard_value() {
        // Grounded arg 0 (`t1`) narrows via the argument-position index
        // (the `Indexed` candidate branch); the wildcard at arg 1 is the
        // value read back.
        let state = price_state(&[("t1", 100), ("t2", 200)]);
        assert_eq!(
            eval_in(
                &value_of("Price", vec![subj("t1"), wildcard()]),
                &state,
                None
            ),
            Ok(EvalValue::Decimal(Decimal::new(100, 0))),
        );
    }

    #[test]
    fn value_of_full_scan_branch_single_match() {
        // No grounded arg, so `select_candidates` takes the `All` branch
        // (full predicate scan). A single claim resolves uniquely.
        let state = State::from_claims(vec![ClaimInstance {
            predicate: "Singleton".into(),
            args: vec![EvalValue::Decimal(Decimal::new(7, 0))],
        }]);
        assert_eq!(
            eval_in(&value_of("Singleton", vec![wildcard()]), &state, None),
            Ok(EvalValue::Decimal(Decimal::new(7, 0))),
        );
    }

    #[test]
    fn value_of_zero_matches_uses_default() {
        let state = price_state(&[("t1", 100)]);
        assert_eq!(
            eval_in(
                &value_of_with_default("Price", vec![subj("absent"), wildcard()], term(dec("42")),),
                &state,
                None,
            ),
            Ok(EvalValue::Decimal(Decimal::new(42, 0))),
        );
    }

    #[test]
    fn value_of_zero_matches_without_default_errors() {
        let state = price_state(&[("t1", 100)]);
        assert_eq!(
            eval_in(
                &value_of("Price", vec![subj("absent"), wildcard()]),
                &state,
                None
            ),
            Err(EvalError::ValueOfZeroMatches("Price".to_string())),
        );
    }

    #[test]
    fn value_of_multiple_matches_errors() {
        // Two Price claims share arg 0 `t1`, so the wildcard at arg 1
        // matches both - the functional-lookup contract is violated.
        let state = price_state(&[("t1", 100), ("t1", 200)]);
        assert_eq!(
            eval_in(
                &value_of("Price", vec![subj("t1"), wildcard()]),
                &state,
                None
            ),
            Err(EvalError::ValueOfMultipleMatches("Price".to_string())),
        );
    }

    #[test]
    fn value_of_unbound_actor_errors_position_independently() {
        // A selective ground arg before `actor` would short-circuit to
        // "no matches" first; the up-front actor check in
        // `select_candidates` must still surface `UnboundActor` when no
        // actor is in scope.
        let state = price_state(&[("t1", 100)]);
        let e = value_of("Triple", vec![subj("absent"), Term::Actor, wildcard()]);
        assert_eq!(eval_in(&e, &state, None), Err(EvalError::UnboundActor));
    }

    // matching_claims: the retract path's claim lookup. Same indexed
    // narrowing as find_claim_matches, returning the matched claims.

    fn matched_for(state: &State, args: Vec<Term>) -> Vec<ClaimInstance> {
        let bindings = Bindings::new();
        let ctx = EvalContext::new(state, None, &bindings, None);
        matching_claims(&"Price".into(), &args, &ctx).expect("matching_claims")
    }

    #[test]
    fn matching_claims_narrows_by_ground_arg() {
        // Ground arg 0 selects only that subject's claims (the Indexed
        // branch); the wildcard at arg 1 does not constrain.
        let state = price_state(&[("t1", 100), ("t1", 150), ("t2", 200)]);
        let matched = matched_for(&state, vec![subj("t1"), wildcard()]);
        assert_eq!(matched.len(), 2);
        assert!(
            matched
                .iter()
                .all(|c| c.args[0] == EvalValue::Subject(Subject::from("t1")))
        );
    }

    #[test]
    fn matching_claims_full_scan_all_wildcards() {
        // No ground arg: the All branch returns every claim of the
        // predicate.
        let state = price_state(&[("t1", 100), ("t2", 200)]);
        assert_eq!(matched_for(&state, vec![wildcard(), wildcard()]).len(), 2);
    }

    #[test]
    fn matching_claims_no_match_is_empty() {
        let state = price_state(&[("t1", 100)]);
        assert!(matched_for(&state, vec![subj("absent"), wildcard()]).is_empty());
    }

    #[test]
    fn matching_claims_unbound_actor_errors() {
        // `select_candidates`' up-front actor check applies on the
        // retract path too: a `Term::Actor` arg with no actor in scope
        // is an error, not a silent no-match.
        let state = price_state(&[("t1", 100)]);
        let bindings = Bindings::new();
        let ctx = EvalContext::new(&state, None, &bindings, None);
        assert_eq!(
            matching_claims(&"Price".into(), &[Term::Actor, wildcard()], &ctx),
            Err(EvalError::UnboundActor),
        );
    }
}
