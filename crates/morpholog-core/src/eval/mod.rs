//! The in-memory evaluator.
//!
//! `find_matches` walks a [`Prop`] against a [`State`] and returns every
//! extended binding context that satisfies it. `eval_value` walks a
//! [`ValueExpr`] and returns the one value it computes. Both return a
//! kernel error instead when the expression cannot be evaluated. The IR
//! keeps values and propositions apart, so neither has a wrong-shape arm.
//!
//! `EvalError` means an expression is ill-formed (type mismatch, unbound
//! variable, wrong `ValueOf` cardinality). It is not a business
//! rejection; that is `Outcome::Rejected`.

use jiff::civil::Date;
use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};
use std::str::FromStr;

use crate::definitions::DefinitionTable;
use crate::ir::{
    ArithOp, Builtin, CompareOp, Definition, DefinitionName, OrderedDomain, PredicateName, Prop,
    Subject, Term, Value, ValueExpr, Var,
};
use crate::state::{Bindings, CandidateBucket, ClaimInstance, EvalValue, State};

/// Errors raised by the evaluator and the transformation runner: an
/// expression or transformation was structurally ill-formed and cannot
/// be run. Distinct from lawful business rejection
/// ([`crate::Outcome::Rejected`]).
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum EvalError {
    /// A variable was referenced before being bound by a parameter,
    /// `let`, `for`, or `exists` binding.
    #[error("unbound variable: {0}")]
    UnboundVariable(String),
    /// An operand had the wrong kind (e.g. arithmetic on a subject,
    /// membership in a non-collection).
    #[error("type mismatch: {0}")]
    TypeMismatch(String),
    /// `ValueExpr::ValueOf(predicate, args)` matched zero claims and no
    /// `default` was supplied.
    #[error("value({0}, _): zero matches")]
    ValueOfZeroMatches(String),
    /// `ValueExpr::ValueOf(predicate, args)` matched more than one claim;
    /// the functional-lookup contract requires exactly one match.
    #[error("value({0}, _): multiple matches")]
    ValueOfMultipleMatches(String),
    /// `Term::Actor` was used with no transition in scope, as in an
    /// invariant or derived-claim body. Those judge admitted state, not
    /// a proposer, so authority checks belong in `require`.
    #[error(
        "Term::Actor referenced with no transition in scope (likely used outside a transformation body - e.g., inside an invariant or derived-claim body; authority checks belong in `require`)"
    )]
    UnboundActor,
    /// `Prop::Pre` was reached with no pre-state in scope: a derived-claim
    /// body, a transformation `require`, the inner of a nested `pre`, or
    /// an `EvalContext` built with `pre_state: None`.
    #[error(
        "Prop::Pre evaluated with no pre-state in scope (a derived-claim body, a transformation `require`, the inner of nested `pre`, or an EvalContext built with pre_state: None)"
    )]
    PreStateUnavailable,
    /// An `ArithOp::Div` or `ArithOp::Mod` with a zero divisor. The
    /// proposal fails (or the derived read errors). Gates avoid this by
    /// cross-multiplying with `Mul`.
    #[error("division by zero")]
    DivisionByZero,
    /// A `round(x, quantum)` with a zero or negative quantum. Neither has
    /// a meaning. `Program::validate` catches a literal quantum; this
    /// catches one that arrives through a variable.
    #[error("round quantum must be positive, got {0}")]
    RoundQuantumNotPositive(String),
    /// A period builtin (`period_index`, `period_start_of`) whose span is
    /// not positive; otherwise every date would sit in infinitely many
    /// periods. `Program::validate` catches a literal span; this catches
    /// one that arrives through a defined-call parameter.
    #[error("{builtin} needs a positive span; got {span}")]
    PeriodSpanNotPositive { builtin: &'static str, span: String },
    /// A `period_start_of(anchor, span, index)` whose index is not a
    /// whole number. There is no period between periods.
    /// `Program::validate` catches a literal fraction; this catches a
    /// computed one.
    #[error("period_start_of needs a whole-number index; got {0}")]
    PeriodIndexNotWhole(String),
    /// A `round(x, quantum)` whose exact answer is not representable: the
    /// nearest multiple lies outside the decimal range, or the operands'
    /// scales are too large for an exact remainder. The kernel answers
    /// exactly or refuses by name; it never approximates or panics.
    #[error("round out of decimal range: no representable multiple of {quantum} near {value}")]
    RoundOutOfRange { value: String, quantum: String },
    /// An arithmetic result outside the exact decimal (or time) range.
    /// Same contract as [`EvalError::RoundOutOfRange`]: exact or refused,
    /// never approximated, never a panic.
    ///
    /// Like every `EvalError`, this is a kernel error (`PgError::Kernel`
    /// at the adapter, the error envelope at the CLI), not a business
    /// rejection: the rule cannot be evaluated over this state. A domain
    /// where extreme values are possible should guard them in its own
    /// rules.
    #[error("arithmetic out of range: {0}")]
    ArithOutOfRange(String),
    /// `max`/`min` over a body that matched nothing. An empty sum has a
    /// typed zero; an empty extremum has no answer, so it refuses rather
    /// than inventing one. Guard with a `require` when "none in force"
    /// should be a lawful rejection instead of an error.
    #[error(
        "{op} over `{body}` matched nothing; an empty {op} has no value (guard it with a require)"
    )]
    EmptyExtremum { op: &'static str, body: String },
    /// A `Prop::Defined` call named a definition the evaluation context
    /// does not carry, so there is no body to expand. `Program::validate`
    /// catches this; the runtime error covers unvalidated IR.
    #[error(
        "call to definition `{0}` but the evaluation context carries no such definition; validate the programme before proposing"
    )]
    UnknownDefinition(String),
}

impl EvalError {
    /// A sum of decimals whose exact total is not a representable
    /// decimal. Shared so the interpreter and the compiled checks report
    /// the same error.
    pub fn sum_out_of_decimal_range() -> Self {
        EvalError::ArithOutOfRange("sum of decimals exceeds the exact decimal range".to_string())
    }
}

/// Evaluator context: state(s), bindings, optional actor. Threaded
/// through `find_matches`, `eval_value`, and the helpers that recurse
/// into expression bodies.
#[derive(Clone, Copy)]
pub(crate) struct EvalContext<'a> {
    /// The state lookups read: the candidate (post) state when checking a
    /// proposal's invariants, the pre-transition state inside `pre(...)`,
    /// otherwise the only state there is.
    pub(crate) state: &'a State,
    /// Pre-transition state when both states are in scope. Cleared inside
    /// a `Pre` subtree, so `pre(pre(...))` surfaces `PreStateUnavailable`.
    pub(crate) pre_state: Option<&'a State>,
    pub(crate) bindings: &'a Bindings,
    /// The proposing transition's actor; `None` in one-state contexts.
    /// `Term::Actor` reached with `actor: None` surfaces `UnboundActor`.
    pub(crate) actor: Option<&'a Subject>,
    /// The programme's definitions, for resolving `Prop::Defined` calls.
    pub(crate) definitions: DefinitionTable<'a>,
}

impl<'a> EvalContext<'a> {
    pub(crate) fn new(
        state: &'a State,
        pre_state: Option<&'a State>,
        bindings: &'a Bindings,
        actor: Option<&'a Subject>,
        definitions: DefinitionTable<'a>,
    ) -> Self {
        Self {
            state,
            pre_state,
            bindings,
            actor,
            definitions,
        }
    }

    /// Swap in extended bindings; used when descending into a
    /// conjunct, an `Implies` right side, or a quantifier body.
    pub(crate) fn with_bindings(&self, bindings: &'a Bindings) -> Self {
        Self { bindings, ..*self }
    }

    /// Enter a `Prop::Pre` subtree: state becomes the previous
    /// pre-state, pre-state is cleared. `None` if no pre-state was
    /// in scope; caller surfaces `PreStateUnavailable`.
    pub(crate) fn enter_pre(&self) -> Option<Self> {
        Some(Self {
            state: self.pre_state?,
            pre_state: None,
            ..*self
        })
    }

    /// Enter a definition body: a fresh frame with only the call's
    /// parameter bindings, no pre-state and no actor. Bodies are
    /// context-free, so `pre(...)` or `actor` in unvalidated IR raise
    /// their usual context errors here.
    fn enter_definition(&self, frame: &'a Bindings) -> Self {
        Self {
            state: self.state,
            pre_state: None,
            bindings: frame,
            actor: None,
            definitions: self.definitions,
        }
    }
}

/// Why two runtime values cannot be ordered under `domain`, or `None`
/// when they can. One authority for the interpreter and the compiled
/// checks, so both report the same error for the same operands.
pub fn ordered_compare_error(
    domain: OrderedDomain,
    left: &EvalValue,
    right: &EvalValue,
) -> Option<EvalError> {
    match (domain, left, right) {
        (OrderedDomain::Decimal, EvalValue::Decimal(_), EvalValue::Decimal(_))
        | (OrderedDomain::Date, EvalValue::Date(_), EvalValue::Date(_))
        | (OrderedDomain::Timestamp, EvalValue::Timestamp(_), EvalValue::Timestamp(_))
        | (OrderedDomain::Duration, EvalValue::Duration(_), EvalValue::Duration(_)) => None,
        // A `Decimal[U]` is an exact decimal with a unit label; two
        // quantities compare only under the same unit.
        (
            OrderedDomain::Decimal,
            EvalValue::Quantity { unit: u, .. },
            EvalValue::Quantity { unit: v, .. },
        ) => (u != v).then(|| {
            EvalError::TypeMismatch(format!(
                "cannot compare Decimal[{u}] with Decimal[{v}]: \
                 comparison requires the same unit"
            ))
        }),
        (OrderedDomain::Decimal, l, r) => Some(EvalError::TypeMismatch(format!(
            "comparison expects two decimal-domain operands of one flavour \
             (bare decimals, or quantities of the same unit); got {} vs {}",
            runtime_kind_label(l),
            runtime_kind_label(r),
        ))),
        (OrderedDomain::Date, _, _) => Some(EvalError::TypeMismatch(
            "comparison expects civil-date operands".to_string(),
        )),
        (OrderedDomain::Timestamp, _, _) => Some(EvalError::TypeMismatch(
            "comparison expects timestamp operands".to_string(),
        )),
        (OrderedDomain::Duration, _, _) => Some(EvalError::TypeMismatch(
            "comparison expects duration operands".to_string(),
        )),
    }
}

/// Evaluate an ordered comparison. Both operands must resolve to the
/// `domain`'s runtime kind. Returns the unchanged bindings when it
/// holds, empty otherwise.
fn ordered_comparison(
    left: &ValueExpr,
    right: &ValueExpr,
    op: CompareOp,
    domain: OrderedDomain,
    ctx: &EvalContext<'_>,
) -> Result<Vec<Bindings>, EvalError> {
    let l = eval_value(left, ctx)?;
    let r = eval_value(right, ctx)?;
    if let Some(e) = ordered_compare_error(domain, &l, &r) {
        return Err(e);
    }
    let holds = match (l, r) {
        (EvalValue::Decimal(a), EvalValue::Decimal(b)) => apply_cmp(op, a, b),
        (EvalValue::Quantity { amount: a, .. }, EvalValue::Quantity { amount: b, .. }) => {
            apply_cmp(op, a, b)
        }
        (EvalValue::Date(a), EvalValue::Date(b)) => apply_cmp(op, a, b),
        (EvalValue::Timestamp(a), EvalValue::Timestamp(b)) => apply_cmp(op, a, b),
        (EvalValue::Duration(a), EvalValue::Duration(b)) => apply_cmp(op, a, b),
        (l, r) => {
            return Err(EvalError::TypeMismatch(format!(
                "ordered_compare_error admitted {} vs {} under {domain:?}",
                runtime_kind_label(&l),
                runtime_kind_label(&r)
            )));
        }
    };
    Ok(verdict(ctx.bindings, holds))
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

/// `a xor b` means exactly one: `(a or b) and not (a and b)`, with the
/// same bindings as that hand-written form. `find_matches` evaluates this
/// expansion, and the validator measures its depth, so a deep xor cannot
/// pass the depth guard and then overflow evaluation.
pub(crate) fn lower_xor(left: &Prop, right: &Prop) -> Prop {
    Prop::And(vec![
        Prop::Or(vec![left.clone(), right.clone()]),
        Prop::Not(Box::new(Prop::And(vec![left.clone(), right.clone()]))),
    ])
}

/// Build a definition call's frame: the bindings its body runs under.
/// A ground argument (a literal, `actor`, or a bound variable) pre-binds
/// its parameter; an unbound variable or wildcard leaves it free, so the
/// body generates values for it. Nothing else from the caller enters, so
/// a body cannot capture surrounding scope.
///
/// The evaluator, the failure walk and the missing-claims walk all build
/// their body context here.
pub(crate) fn definition_call_frame(
    def: &Definition,
    args: &[Term],
    ctx: &EvalContext<'_>,
) -> Result<Bindings, EvalError> {
    if args.len() != def.parameters.len() {
        return Err(EvalError::TypeMismatch(format!(
            "definition {} takes {} argument(s) but the call passes {}",
            def.name,
            def.parameters.len(),
            args.len()
        )));
    }
    let mut frame = Bindings::new();
    for (param, arg) in def.parameters.iter().zip(args) {
        match arg {
            Term::Wildcard => {}
            Term::Var(v) => {
                if let Some(value) = ctx.bindings.get(v) {
                    frame.insert(param.clone(), value.clone());
                }
            }
            // Literals and `actor` resolve at the call site; the body sees
            // the actor only as an ordinary subject value.
            other => {
                frame.insert(param.clone(), resolve_term(other, ctx.bindings, ctx.actor)?);
            }
        }
    }
    Ok(frame)
}

/// Evaluate a call to a named definition. The body runs under the call
/// frame (see [`definition_call_frame`]); each body match projects its
/// parameter values back onto the call's arguments, extending the
/// caller's bindings. Each distinct projection is yielded once, so two
/// internal witnesses for the same arguments cannot double-count in a
/// `Sum`.
fn find_defined_matches(
    name: &DefinitionName,
    args: &[Term],
    ctx: &EvalContext<'_>,
) -> Result<Vec<Bindings>, EvalError> {
    let def = ctx
        .definitions
        .get(name)
        .ok_or_else(|| EvalError::UnknownDefinition(name.to_string()))?;
    let frame = definition_call_frame(def, args, ctx)?;
    let body_matches = find_matches(&def.body, &ctx.enter_definition(&frame))?;

    let mut out: Vec<Bindings> = Vec::new();
    for m in body_matches {
        let mut extended = ctx.bindings.clone();
        let mut admit = true;
        for (param, arg) in def.parameters.iter().zip(args) {
            // Validation guarantees the body binds every parameter;
            // unvalidated IR gets the usual unbound-variable error.
            let Some(value) = m.get(param) else {
                return Err(EvalError::UnboundVariable(param.to_string()));
            };
            if let Term::Var(v) = arg {
                match extended.get(v) {
                    // A pre-bound variable already agrees. A repeated
                    // unbound one (`f(x, x)`) must project the same value
                    // everywhere, or the match is dropped.
                    Some(prev) if prev != value => {
                        admit = false;
                        break;
                    }
                    Some(_) => {}
                    None => {
                        extended.insert(v.clone(), value.clone());
                    }
                }
            }
        }
        if admit && !out.contains(&extended) {
            out.push(extended);
        }
    }
    Ok(out)
}

/// A predicate-shaped truth: the unchanged binding set when `holds`,
/// no bindings otherwise.
fn verdict(base: &Bindings, holds: bool) -> Vec<Bindings> {
    if holds { vec![base.clone()] } else { vec![] }
}

pub(crate) fn find_matches(p: &Prop, ctx: &EvalContext<'_>) -> Result<Vec<Bindings>, EvalError> {
    match p {
        Prop::Claim { predicate, args } => find_claim_matches(predicate, args, ctx),
        Prop::Defined { name, args } => find_defined_matches(name, args, ctx),
        Prop::And(props) => find_conjunction(props, ctx),
        Prop::Or(props) => find_disjunction(props, ctx),
        Prop::Xor(left, right) => find_matches(&lower_xor(left, right), ctx),
        Prop::Not(inner) => {
            let m = find_matches(inner, ctx)?;
            Ok(verdict(ctx.bindings, m.is_empty()))
        }
        Prop::Pre(inner) => {
            let pre_ctx = ctx.enter_pre().ok_or(EvalError::PreStateUnavailable)?;
            find_matches(inner, &pre_ctx)
        }
        // Implication and forall evaluate every binding before answering
        // false, so an error at any binding wins over false whatever the
        // match order. Which of two errors wins is not decided here.
        Prop::Implies { left, right } => {
            let lm = find_matches(left, ctx)?;
            let mut holds = true;
            for m in lm {
                if find_matches(right, &ctx.with_bindings(&m))?.is_empty() {
                    holds = false;
                }
            }
            Ok(verdict(ctx.bindings, holds))
        }
        Prop::Exists { binding: _, body } => {
            let m = find_matches(body, ctx)?;
            Ok(verdict(ctx.bindings, !m.is_empty()))
        }
        Prop::Forall {
            binding: _,
            source,
            body,
        } => {
            let sm = find_matches(source, ctx)?;
            let mut holds = true;
            for m in sm {
                if find_matches(body, &ctx.with_bindings(&m))?.is_empty() {
                    holds = false;
                }
            }
            Ok(verdict(ctx.bindings, holds))
        }
        Prop::Eq(lhs, rhs) => {
            let l = eval_value(lhs, ctx)?;
            let r = eval_value(rhs, ctx)?;
            Ok(verdict(ctx.bindings, l == r))
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
            Ok(verdict(ctx.bindings, l != r))
        }
        Prop::In(elem, coll) => find_in_matches(elem, coll, ctx),
    }
}

/// Parse a `Value::Date(String)` literal into a `jiff::civil::Date`.
/// One parser, so every path reads `YYYY-MM-DD` the same way.
pub(crate) fn parse_date_literal(s: &str) -> Result<Date, EvalError> {
    s.parse::<Date>()
        .map_err(|e| EvalError::TypeMismatch(format!("invalid civil date `{s}`: {e}")))
}

/// Parse a `Value::Timestamp(String)` literal into a [`jiff::Timestamp`]
/// (RFC 3339).
pub(crate) fn parse_timestamp_literal(s: &str) -> Result<jiff::Timestamp, EvalError> {
    s.parse::<jiff::Timestamp>()
        .map_err(|e| EvalError::TypeMismatch(format!("invalid timestamp `{s}`: {e}")))
}

/// An exact span as a decimal count of nanoseconds, so two durations
/// divide exactly. Always fits: |i64 seconds| * 1e9 + nanos < 2^96.
fn duration_nanos_decimal(d: jiff::SignedDuration) -> Decimal {
    let nanos = i128::from(d.as_secs()) * 1_000_000_000 + i128::from(d.subsec_nanos());
    Decimal::try_from_i128_with_scale(nanos, 0)
        .unwrap_or_else(|_| unreachable!("duration nanoseconds always fit a Decimal"))
}

/// A runtime value's kind as authors write it, for error messages.
/// Quantities show their unit (`Decimal[USD]`).
fn runtime_kind_label(v: &EvalValue) -> String {
    match v {
        EvalValue::Decimal(_) => "decimal".to_string(),
        EvalValue::Subject(_) => "subject".to_string(),
        EvalValue::Bool(_) => "bool".to_string(),
        EvalValue::Date(_) => "date".to_string(),
        EvalValue::Timestamp(_) => "timestamp".to_string(),
        EvalValue::Duration(_) => "duration".to_string(),
        EvalValue::CalendarSpan(_) => "calendar span".to_string(),
        EvalValue::Quantity { unit, .. } => format!("Decimal[{unit}]"),
        EvalValue::Collection(_) => "collection".to_string(),
    }
}

/// Parse a `Value::Quantity` literal's amount into the runtime
/// quantity value, keeping the unit label.
pub(crate) fn parse_quantity_literal(
    amount: &str,
    unit: &crate::ir::Unit,
) -> Result<EvalValue, EvalError> {
    let d = Decimal::from_str(amount).map_err(|_| {
        EvalError::TypeMismatch(format!("invalid quantity amount `{amount} {unit}`"))
    })?;
    Ok(EvalValue::Quantity {
        amount: d,
        unit: unit.clone(),
    })
}

/// Parse a `Value::Duration(String)` literal into a
/// [`jiff::SignedDuration`] (ISO 8601, e.g. `PT6H`). Exact seconds
/// only: calendar units are rejected by the type itself.
pub(crate) fn parse_duration_literal(s: &str) -> Result<jiff::SignedDuration, EvalError> {
    s.parse::<jiff::SignedDuration>()
        .map_err(|e| EvalError::TypeMismatch(format!("invalid duration `{s}`: {e}")))
}

/// Parse a `Value::CalendarSpan(String)` literal with the kernel's own
/// grammar ([`crate::calendar::parse_calendar_span`]), the same one the
/// surface diagnostics use.
pub(crate) fn parse_calendar_span_literal(
    s: &str,
) -> Result<crate::calendar::CalendarSpan, EvalError> {
    crate::calendar::parse_calendar_span(s)
        .map_err(|e| EvalError::TypeMismatch(format!("invalid calendar span `{s}`: {e}")))
}

/// Shift a civil date by whole months, then whole days. Months move the
/// (year, month) and clamp the day to the destination month's length;
/// days then step one civil day at a time. `None` when the result leaves
/// the representable calendar.
fn shift_date(d: Date, months: i64, days: i64) -> Option<Date> {
    let total = i64::from(d.year()) * 12 + i64::from(d.month()) - 1 + months;
    let year = i16::try_from(total.div_euclid(12)).ok()?;
    #[allow(clippy::cast_possible_truncation)] // rem_euclid(12) is 0..=11
    let month = (total.rem_euclid(12) + 1) as i8;
    let first = Date::new(year, month, 1).ok()?;
    let day = d.day().min(first.days_in_month());
    let landed = Date::new(year, month, day).ok()?;
    let day_span = jiff::Span::new().try_days(days).ok()?;
    landed.checked_add(day_span).ok()
}

/// The anniversary-anchored period index: the greatest integer n with
/// `period_boundary(n) <= at`. Periods are half-open, `[B(n), B(n+1))`,
/// so an exact anniversary starts the new period and dates before the
/// anchor get negative indexes. A boundary outside the calendar counts
/// as minus or plus infinity, so the outermost periods are clipped and
/// every representable (anchor, at) pair has an answer. Found by binary
/// search over the monotone boundaries.
fn period_index_of(
    anchor: Date,
    span: crate::calendar::CalendarSpan,
    at: Date,
) -> Result<i64, EvalError> {
    require_positive_span("period_index", span)?;
    let boundary = |n: i64| period_boundary(anchor, span, n);
    let at_or_before = |n: i64| -> bool {
        match boundary(n) {
            Some(b) => b <= at,
            // Off the calendar. A positive span can only leave upward for
            // n > 0 and downward for n < 0: below counts as minus
            // infinity (always at-or-before), above as plus infinity.
            None => n < 0,
        }
    };
    // Each positive span moves a boundary by at least one day, so no
    // index can exceed the calendar's day range, plus room for the
    // clipped outer periods.
    let cap: i64 = days_between(Date::MIN, Date::MAX) + 2;
    let (mut lo, mut hi) = (-cap, cap);
    // Cannot fire given the cap above. A release-build assert, because a
    // broken bracket would otherwise return a silently wrong index.
    assert!(
        at_or_before(lo) && !at_or_before(hi),
        "period_index bracket must hold by construction"
    );
    while hi - lo > 1 {
        let mid = lo + (hi - lo) / 2;
        if at_or_before(mid) {
            lo = mid;
        } else {
            hi = mid;
        }
    }
    Ok(lo)
}

/// Boundary n of an anniversary-anchored schedule: the anchor shifted
/// once by n times the span. Never n repeated hops, because month-end
/// clamping would make those drift. `None` when the boundary leaves the
/// calendar. Shared by `period_index` (which clips) and
/// `period_start_of` (which refuses).
fn period_boundary(anchor: Date, span: crate::calendar::CalendarSpan, n: i64) -> Option<Date> {
    shift_date(
        anchor,
        n.checked_mul(i64::from(span.months))?,
        n.checked_mul(i64::from(span.days))?,
    )
}

/// The period builtins need a positive span, or every date would sit in
/// infinitely many periods.
fn require_positive_span(
    builtin: &'static str,
    span: crate::calendar::CalendarSpan,
) -> Result<(), EvalError> {
    if span.months < 0 || span.days < 0 || (span.months == 0 && span.days == 0) {
        return Err(EvalError::PeriodSpanNotPositive {
            builtin,
            span: span.to_string(),
        });
    }
    Ok(())
}

/// The first day of period `index`: the inverse of [`period_index_of`]
/// wherever the boundary is representable. `period_index` clips its
/// outer periods, but an index whose boundary leaves the calendar has no
/// date to return, so this refuses, as `date + span` does.
fn period_start_of_date(
    anchor: Date,
    span: crate::calendar::CalendarSpan,
    index: Decimal,
) -> Result<Date, EvalError> {
    require_positive_span("period_start_of", span)?;
    if !index.is_integer() {
        return Err(EvalError::PeriodIndexNotWhole(index.to_string()));
    }
    i64::try_from(index)
        .ok()
        .and_then(|n| period_boundary(anchor, span, n))
        .ok_or_else(|| {
            EvalError::ArithOutOfRange(format!(
                "period_start_of: period {index}'s boundary is outside the representable calendar"
            ))
        })
}

/// The signed count of civil days from `from` to `to` (positive when
/// `to` is later). Total for in-range dates: the difference of two
/// representable dates always fits the day unit.
fn days_between(from: Date, to: Date) -> i64 {
    from.until(to)
        .map(|span| i64::from(span.get_days()))
        .unwrap_or_else(|_| unreachable!("the gap between two civil dates always fits in days"))
}

/// The claims worth checking when matching `predicate(args)`, narrowed
/// by the most selective ground argument. Shared by
/// [`find_claim_matches`], [`matching_claims`] and the `ValueOf` lookup.
enum Candidates<'a> {
    /// A ground argument named a `(predicate, position, value)` bucket
    /// that does not exist, so no admitted claim can match.
    None,
    /// The narrowed bucket of claims to check.
    Indexed(CandidateBucket<'a>),
    /// No ground argument to narrow on; every claim of this predicate
    /// is a candidate.
    All,
}

/// Narrow `predicate(args)` to its candidate claims by the most
/// selective ground argument (a literal, a bound variable, or `actor`).
/// For `JournalLine(entry, _, d, _)` inside `forall entry: ...`, the
/// bound `entry` limits the scan to that entry's lines. A missing bucket
/// gives [`Candidates::None`]; no ground argument gives
/// [`Candidates::All`].
///
/// Any `Term::Actor` with no actor in scope raises `UnboundActor` up
/// front. Otherwise an earlier empty bucket could return no matches
/// before the loop ever reached the actor.
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

    let mut best: Option<CandidateBucket<'a>> = None;
    for (pos, term) in args.iter().enumerate() {
        let ground = match term {
            Term::Wildcard => None,
            Term::Var(name) => base.get(name).cloned(),
            Term::Literal(Value::Subject(s)) => Some(EvalValue::Subject(s.clone())),
            Term::Literal(Value::Decimal(s)) => Decimal::from_str(s).ok().map(EvalValue::Decimal),
            Term::Literal(Value::Date(s)) => parse_date_literal(s).ok().map(EvalValue::Date),
            Term::Literal(Value::Timestamp(s)) => {
                parse_timestamp_literal(s).ok().map(EvalValue::Timestamp)
            }
            Term::Literal(Value::Duration(s)) => {
                parse_duration_literal(s).ok().map(EvalValue::Duration)
            }
            // No admitted claim can carry a calendar span, so as a
            // ground argument it matches nothing.
            Term::Literal(Value::CalendarSpan(_)) => None,
            Term::Literal(Value::Quantity { amount, unit }) => {
                parse_quantity_literal(amount, unit).ok()
            }
            Term::Actor => match actor {
                Some(a) => Some(EvalValue::Subject(a.clone())),
                None => return Err(EvalError::UnboundActor),
            },
        };
        let Some(value) = ground else {
            continue;
        };
        match state.claim_candidates(predicate, pos, &value) {
            None => return Ok(Candidates::None),
            Some(bucket) => match &best {
                Some(prev) if prev.estimate_len() <= bucket.estimate_len() => {}
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
            for claim in bucket.iter() {
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
/// under `ctx`, cloned. Like [`find_claim_matches`] but returns the
/// claims rather than bindings, so retract can record what it removed.
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
            for claim in bucket.iter() {
                if claim.args.len() == args.len() && claim_matches(args, &claim.args, base, actor) {
                    out.push(claim.clone());
                }
            }
        }
        Candidates::All => {
            for claim in state.claims_for_name(predicate) {
                if claim.args.len() == args.len() && claim_matches(args, &claim.args, base, actor) {
                    out.push(claim.clone());
                }
            }
        }
    }
    Ok(out)
}

/// Match a claim pattern against values without cloning `base`. Returns
/// the new variable bindings as borrowed pairs, or `None` on a mismatch.
/// A variable repeated in one pattern must bind the same value each
/// time. Shared by [`unify_args`] and [`claim_matches`].
///
/// Lists of different lengths never match; checked here because the
/// `zip` below would otherwise match a prefix.
fn match_args<'a>(
    patterns: &'a [Term],
    values: &'a [EvalValue],
    base: &Bindings,
    actor: Option<&Subject>,
) -> Option<Vec<(&'a Var, &'a EvalValue)>> {
    if patterns.len() != values.len() {
        return None;
    }
    let mut new: Vec<(&Var, &EvalValue)> = Vec::new();
    for (p, v) in patterns.iter().zip(values.iter()) {
        match p {
            Term::Wildcard => {}
            Term::Var(name) => {
                if let Some(existing) = base.get(name) {
                    if existing != v {
                        return None;
                    }
                } else if let Some((_, existing)) = new.iter().find(|(k, _)| *k == name) {
                    if *existing != v {
                        return None;
                    }
                } else {
                    new.push((name, v));
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
            Term::Literal(Value::Timestamp(s)) => {
                let parsed = parse_timestamp_literal(s).ok()?;
                match v {
                    EvalValue::Timestamp(t) if *t == parsed => {}
                    _ => return None,
                }
            }
            Term::Literal(Value::Duration(s)) => {
                let parsed = parse_duration_literal(s).ok()?;
                match v {
                    EvalValue::Duration(d) if *d == parsed => {}
                    _ => return None,
                }
            }
            // A stored value is never a calendar span, so the pattern
            // cannot match one.
            Term::Literal(Value::CalendarSpan(_)) => return None,
            Term::Literal(Value::Quantity { amount, unit }) => {
                let parsed = parse_quantity_literal(amount, unit).ok()?;
                if *v != parsed {
                    return None;
                }
            }
            Term::Actor => match actor {
                Some(a) if matches!(v, EvalValue::Subject(s) if s == a) => {}
                _ => return None,
            },
        }
    }
    Some(new)
}

/// Unify `patterns` against `values`, extending `base` with the new
/// bindings. `base` is cloned only on a match.
pub(crate) fn unify_args(
    patterns: &[Term],
    values: &[EvalValue],
    base: &Bindings,
    actor: Option<&Subject>,
) -> Option<Bindings> {
    let new = match_args(patterns, values, base, actor)?;
    let mut b = base.clone();
    for (name, v) in new {
        b.insert(name.clone(), v.clone());
    }
    Some(b)
}

/// Whether `patterns` unify against `values` under `base`, without
/// building bindings. For lookups that need only a yes or no
/// (`matching_claims`, `ValueOf`).
pub(crate) fn claim_matches(
    patterns: &[Term],
    values: &[EvalValue],
    base: &Bindings,
    actor: Option<&Subject>,
) -> bool {
    match_args(patterns, values, base, actor).is_some()
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

/// Evaluate a disjunction: the binding sets of every branch, each run
/// against the same base context, concatenated. No deduplication, like
/// `find_conjunction`.
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
            Ok(verdict(base, items.contains(&e)))
        }
        Term::Var(name) => {
            if let Some(existing) = base.get(name) {
                Ok(verdict(base, items.contains(existing)))
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
        // Every builtin is strict: arguments evaluate left to right,
        // once each, and the first failure is the error reported.
        // `eval_builtin` sees finished values only, never state,
        // bindings, or the actor.
        ValueExpr::Call { builtin, args } => {
            // Arity before any argument, so a misshapen call reports
            // "abs takes 1 argument", not "y is unbound". Only hand-built
            // IR gets here; validation refuses it earlier.
            check_builtin_arity(*builtin, args.len())?;
            let mut values = Vec::with_capacity(args.len());
            for a in args {
                values.push(eval_value(a, ctx)?);
            }
            eval_builtin(*builtin, &values)
        }
        ValueExpr::Cond {
            when,
            then,
            otherwise,
        } => {
            // An exists-style test: any witness selects `then`, none
            // selects `otherwise`, and the witnesses' bindings are
            // discarded, as with `require`. Only the selected branch
            // evaluates. An error in the condition propagates rather
            // than silently picking a branch.
            let matches = find_matches(when, ctx)?;
            if matches.is_empty() {
                eval_value(otherwise, ctx)
            } else {
                eval_value(then, ctx)
            }
        }
        ValueExpr::Arith { op, left, right } => {
            let l = eval_value(left, ctx)?;
            let r = eval_value(right, ctx)?;
            match (l, r) {
                (EvalValue::Decimal(a), EvalValue::Decimal(b)) => {
                    Ok(EvalValue::Decimal(checked_decimal_op(*op, a, b)?))
                }
                // Time arithmetic: an instant plus or minus a span is an
                // instant; two instants subtract to a span; spans add,
                // subtract, and divide. Everything else is a type error.
                (EvalValue::Timestamp(t), EvalValue::Duration(d)) => {
                    let shifted = match op {
                        ArithOp::Add => t.checked_add(d),
                        ArithOp::Sub => t.checked_sub(d),
                        _ => {
                            return Err(EvalError::TypeMismatch(format!(
                                "{op:?} is not defined for timestamp and duration"
                            )));
                        }
                    };
                    shifted.map(EvalValue::Timestamp).map_err(|e| {
                        EvalError::ArithOutOfRange(format!("timestamp {op:?} duration: {e}"))
                    })
                }
                (EvalValue::Timestamp(a), EvalValue::Timestamp(b)) => match op {
                    ArithOp::Sub => Ok(EvalValue::Duration(a.duration_since(b))),
                    _ => Err(EvalError::TypeMismatch(format!(
                        "{op:?} is not defined for two timestamps (only Sub: the gap between them)"
                    ))),
                },
                // A calendar span shifts a date: months first, clamping
                // the day (Jan 31 + P1M is Feb 28 or 29), then days.
                // Subtraction negates both parts. Clamping makes the
                // shift neither reversible nor associative at month ends;
                // that is how the calendar behaves.
                (EvalValue::Date(d), EvalValue::CalendarSpan(s)) => {
                    let (months, days) = (i64::from(s.months), i64::from(s.days));
                    let shifted = match op {
                        ArithOp::Add => shift_date(d, months, days),
                        ArithOp::Sub => shift_date(d, -months, -days),
                        _ => {
                            return Err(EvalError::TypeMismatch(format!(
                                "{op:?} is not defined for date and calendar span"
                            )));
                        }
                    };
                    shifted.map(EvalValue::Date).ok_or_else(|| {
                        EvalError::ArithOutOfRange(format!("date {op:?} {s} leaves the calendar"))
                    })
                }
                // Two dates subtract to the signed count of actual days,
                // as a decimal.
                (EvalValue::Date(a), EvalValue::Date(b)) => match op {
                    ArithOp::Sub => Ok(EvalValue::Decimal(Decimal::from(days_between(b, a)))),
                    _ => Err(EvalError::TypeMismatch(format!(
                        "{op:?} is not defined for two dates (only Sub: the days between them)"
                    ))),
                },
                (EvalValue::Duration(a), EvalValue::Duration(b)) => match op {
                    ArithOp::Add | ArithOp::Sub => {
                        let result = match op {
                            ArithOp::Add => a.checked_add(b),
                            ArithOp::Sub => a.checked_sub(b),
                            _ => unreachable!("outer match restricts the op"),
                        };
                        result.map(EvalValue::Duration).ok_or_else(|| {
                            EvalError::ArithOutOfRange(format!("duration {op:?} duration"))
                        })
                    }
                    // The ratio of two spans is a plain decimal (132h/24h
                    // = 5.5). Terminating ratios are exact; a repeating
                    // one carries Decimal's 28 digits, and any rounding
                    // is the rule's job.
                    ArithOp::Div => {
                        let divisor = duration_nanos_decimal(b);
                        if divisor == Decimal::ZERO {
                            return Err(EvalError::DivisionByZero);
                        }
                        Ok(EvalValue::Decimal(checked_decimal_op(
                            ArithOp::Div,
                            duration_nanos_decimal(a),
                            divisor,
                        )?))
                    }
                    _ => Err(EvalError::TypeMismatch(format!(
                        "{op:?} is not defined for durations"
                    ))),
                },
                // Units: amounts combine only under the same unit; a bare
                // decimal scales a quantity; two same-unit quantities
                // divide to a bare decimal. No arithmetic makes a new
                // unit.
                (
                    EvalValue::Quantity { amount: a, unit: u },
                    EvalValue::Quantity { amount: b, unit: v },
                ) => {
                    if u != v {
                        return Err(EvalError::TypeMismatch(format!(
                            "no arithmetic rule for Decimal[{u}] {op:?} Decimal[{v}]: \
                             quantity arithmetic requires the same unit"
                        )));
                    }
                    match op {
                        ArithOp::Add | ArithOp::Sub => Ok(EvalValue::Quantity {
                            amount: checked_decimal_op(*op, a, b)?,
                            unit: u,
                        }),
                        ArithOp::Div => Ok(EvalValue::Decimal(checked_decimal_op(*op, a, b)?)),
                        ArithOp::Mul | ArithOp::Mod => Err(EvalError::TypeMismatch(format!(
                            "{op:?} is not defined for Decimal[{u}] and Decimal[{u}]: \
                             two amounts of one unit multiply into no meaningful unit"
                        ))),
                    }
                }
                (EvalValue::Quantity { amount: a, unit }, EvalValue::Decimal(b)) => match op {
                    ArithOp::Mul | ArithOp::Div => Ok(EvalValue::Quantity {
                        amount: checked_decimal_op(*op, a, b)?,
                        unit,
                    }),
                    _ => Err(EvalError::TypeMismatch(format!(
                        "{op:?} is not defined for Decimal[{unit}] and a bare decimal \
                         (only Mul/Div: a bare decimal scales a quantity)"
                    ))),
                },
                (EvalValue::Decimal(a), EvalValue::Quantity { amount: b, unit }) => match op {
                    ArithOp::Mul => Ok(EvalValue::Quantity {
                        amount: checked_decimal_op(*op, a, b)?,
                        unit,
                    }),
                    _ => Err(EvalError::TypeMismatch(format!(
                        "{op:?} is not defined for a bare decimal and Decimal[{unit}] \
                         (only Mul: a bare decimal scales a quantity)"
                    ))),
                },
                (l, r) => Err(EvalError::TypeMismatch(format!(
                    "no arithmetic rule for {} {op:?} {}",
                    runtime_kind_label(&l),
                    runtime_kind_label(&r),
                ))),
            }
        }
        ValueExpr::Extremum { op, value, body } => {
            // Every candidate is compared against the running best, and
            // mixing kinds is an error, so match order cannot pick the
            // winner.
            let matches = find_matches(body, ctx)?;
            let mut best: Option<EvalValue> = None;
            for m in matches {
                let next = resolve_term(value, &m, ctx.actor)?;
                best = Some(match best {
                    None => {
                        // Check the first one too, or a single unordered
                        // match would succeed where two would fail.
                        ensure_ordered(&next, *op)?;
                        next
                    }
                    Some(current) => {
                        let ordering = compare_ordered(&current, &next, *op)?;
                        if ordering { next } else { current }
                    }
                });
            }
            // Unlike a sum, an empty extremum has no value to return.
            best.ok_or_else(|| EvalError::EmptyExtremum {
                op: op.as_str(),
                body: crate::format::format_prop_inline(body),
            })
        }
        ValueExpr::Sum { value, body, seed } => {
            // A sum keeps the kind of what it sums: decimals, durations,
            // or quantities of one unit. Mixing kinds or units is an
            // error. An empty sum is `seed`, the typed zero of the
            // declared kind (`0 t`, `PT0S`).
            //
            // The accumulators are wider than the values, so only the
            // final total can overflow. Otherwise [MAX, 1, -MAX] could
            // pass in one match order and fail in another.
            enum SumTotal {
                Empty,
                Decimal(BigSum),
                Duration(i128),
                Quantity(BigSum, crate::ir::Unit),
            }
            let matches = find_matches(body, ctx)?;
            let mut total = SumTotal::Empty;
            for m in matches {
                let next = eval_value(value, &ctx.with_bindings(&m))?;
                total = match (total, next) {
                    (SumTotal::Empty, EvalValue::Decimal(d)) => SumTotal::Decimal(BigSum::new(d)),
                    (SumTotal::Empty, EvalValue::Duration(d)) => SumTotal::Duration(d.as_nanos()),
                    (SumTotal::Empty, EvalValue::Quantity { amount, unit }) => {
                        SumTotal::Quantity(BigSum::new(amount), unit)
                    }
                    (SumTotal::Decimal(mut t), EvalValue::Decimal(d)) => {
                        t.add(d);
                        SumTotal::Decimal(t)
                    }
                    (SumTotal::Duration(t), EvalValue::Duration(d)) => {
                        // Cannot overflow: each value is within
                        // +-9.3e27ns, so passing i128 needs more claims
                        // than memory can hold.
                        SumTotal::Duration(t + d.as_nanos())
                    }
                    (SumTotal::Quantity(mut t, u), EvalValue::Quantity { amount, unit }) => {
                        if u != unit {
                            return Err(EvalError::TypeMismatch(format!(
                                "Sum cannot mix Decimal[{u}] and Decimal[{unit}] values"
                            )));
                        }
                        t.add(amount);
                        SumTotal::Quantity(t, u)
                    }
                    (
                        SumTotal::Decimal(_) | SumTotal::Duration(_) | SumTotal::Quantity(..),
                        other,
                    ) => {
                        return Err(EvalError::TypeMismatch(format!(
                            "Sum cannot mix value kinds (next value is {})",
                            runtime_kind_label(&other)
                        )));
                    }
                    (SumTotal::Empty, other) => {
                        return Err(EvalError::TypeMismatch(format!(
                            "Sum expects decimal, duration, or quantity values, got {}",
                            runtime_kind_label(&other)
                        )));
                    }
                };
            }
            Ok(match total {
                SumTotal::Empty => match seed {
                    crate::ir::SumSeed::Decimal => EvalValue::Decimal(Decimal::ZERO),
                    crate::ir::SumSeed::Duration => EvalValue::Duration(jiff::SignedDuration::ZERO),
                    crate::ir::SumSeed::Quantity(unit) => EvalValue::Quantity {
                        amount: Decimal::ZERO,
                        unit: unit.clone(),
                    },
                },
                SumTotal::Decimal(t) => EvalValue::Decimal(
                    t.into_decimal()
                        .ok_or_else(EvalError::sum_out_of_decimal_range)?,
                ),
                SumTotal::Duration(t) => {
                    EvalValue::Duration(nanos_to_duration(t).ok_or_else(|| {
                        EvalError::ArithOutOfRange("sum of durations out of range".to_string())
                    })?)
                }
                SumTotal::Quantity(t, unit) => EvalValue::Quantity {
                    amount: t.into_decimal().ok_or_else(|| {
                        EvalError::ArithOutOfRange(format!(
                            "sum of Decimal[{unit}] values exceeds the exact decimal range"
                        ))
                    })?,
                    unit,
                },
            })
        }
        ValueExpr::ValueOf {
            predicate,
            args,
            extract,
            default,
        } => {
            // `extract` must index a wildcard. Validation ensures it;
            // unvalidated IR is refused here rather than misread. One
            // pass over the narrowed candidates finds the claim and
            // reads that position.
            let pos = *extract;
            if !matches!(args.get(pos), Some(Term::Wildcard)) {
                return Err(EvalError::TypeMismatch(
                    "ValueOf extraction position must be a wildcard arg".into(),
                ));
            }

            let mut matched: Option<&EvalValue> = None;
            let mut multiple = false;
            match select_candidates(predicate, args, ctx)? {
                Candidates::None => {}
                Candidates::Indexed(bucket) => {
                    for claim in bucket.iter() {
                        if claim.args.len() == args.len()
                            && claim_matches(args, &claim.args, ctx.bindings, ctx.actor)
                        {
                            multiple |= matched.is_some();
                            matched = Some(&claim.args[pos]);
                        }
                    }
                }
                Candidates::All => {
                    for claim in ctx.state.claims_for_name(predicate) {
                        if claim.args.len() == args.len()
                            && claim_matches(args, &claim.args, ctx.bindings, ctx.actor)
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

/// Refuse a builtin call with the wrong number of arguments. Checked
/// before the arguments are evaluated, and again in [`eval_builtin`].
fn check_builtin_arity(builtin: Builtin, found: usize) -> Result<(), EvalError> {
    if found == builtin.arity() {
        return Ok(());
    }
    Err(EvalError::TypeMismatch(format!(
        "{} takes {} argument(s), got {}",
        builtin.name(),
        builtin.arity(),
        found
    )))
}

/// Apply a builtin to its evaluated arguments.
///
/// No wildcard arm, so a new builtin must decide what it computes. The
/// arity check guards hand-built IR that skipped validation.
pub(crate) fn eval_builtin(builtin: Builtin, args: &[EvalValue]) -> Result<EvalValue, EvalError> {
    check_builtin_arity(builtin, args.len())?;
    match builtin {
        Builtin::Abs => match &args[0] {
            EvalValue::Decimal(d) => Ok(EvalValue::Decimal(d.abs())),
            EvalValue::Quantity { amount, unit } => Ok(EvalValue::Quantity {
                amount: amount.abs(),
                unit: unit.clone(),
            }),
            EvalValue::Duration(d) => Ok(EvalValue::Duration(d.abs())),
            other => Err(EvalError::TypeMismatch(format!(
                "abs is defined on decimals, quantities, and durations, not {}",
                runtime_kind_label(other)
            ))),
        },
        Builtin::Round => match (&args[0], &args[1]) {
            (EvalValue::Decimal(v), EvalValue::Decimal(q)) => {
                if *q <= Decimal::ZERO {
                    return Err(EvalError::RoundQuantumNotPositive(q.to_string()));
                }
                round_decimal(*v, *q).map(EvalValue::Decimal)
            }
            (v, q) => Err(EvalError::TypeMismatch(format!(
                "round is defined on decimals (value and quantum), not {} and {}",
                runtime_kind_label(v),
                runtime_kind_label(q)
            ))),
        },
        Builtin::PeriodIndex => {
            let EvalValue::Date(anchor) = &args[0] else {
                return Err(EvalError::TypeMismatch(format!(
                    "period_index anchor must be a date, got {}",
                    runtime_kind_label(&args[0])
                )));
            };
            let EvalValue::CalendarSpan(span) = &args[1] else {
                return Err(EvalError::TypeMismatch(format!(
                    "period_index span must be a calendar span, got {}",
                    runtime_kind_label(&args[1])
                )));
            };
            let EvalValue::Date(at) = &args[2] else {
                return Err(EvalError::TypeMismatch(format!(
                    "period_index position must be a date, got {}",
                    runtime_kind_label(&args[2])
                )));
            };
            Ok(EvalValue::Decimal(Decimal::from(period_index_of(
                *anchor, *span, *at,
            )?)))
        }
        Builtin::PeriodStartOf => {
            let EvalValue::Date(anchor) = &args[0] else {
                return Err(EvalError::TypeMismatch(format!(
                    "period_start_of anchor must be a date, got {}",
                    runtime_kind_label(&args[0])
                )));
            };
            let EvalValue::CalendarSpan(span) = &args[1] else {
                return Err(EvalError::TypeMismatch(format!(
                    "period_start_of span must be a calendar span, got {}",
                    runtime_kind_label(&args[1])
                )));
            };
            let EvalValue::Decimal(index) = &args[2] else {
                return Err(EvalError::TypeMismatch(format!(
                    "period_start_of index must be a decimal, got {}",
                    runtime_kind_label(&args[2])
                )));
            };
            period_start_of_date(*anchor, *span, *index).map(EvalValue::Date)
        }
        Builtin::Min | Builtin::Max => extremum_of(builtin, &args[0], &args[1]),
    }
}

/// Exact running total of decimals: a big-integer count of units at the
/// largest scale seen. Adding never overflows, so only the final total
/// decides whether the sum fits, whatever the order.
struct BigSum {
    units: num_bigint::BigInt,
    scale: u32,
}

impl BigSum {
    fn new(d: Decimal) -> Self {
        Self {
            units: num_bigint::BigInt::from(d.mantissa()),
            scale: d.scale(),
        }
    }

    fn add(&mut self, d: Decimal) {
        let scale = d.scale();
        if scale > self.scale {
            self.units *= num_bigint::BigInt::from(10u32).pow(scale - self.scale);
            self.scale = scale;
        }
        let mut mantissa = num_bigint::BigInt::from(d.mantissa());
        if self.scale > scale {
            mantissa *= num_bigint::BigInt::from(10u32).pow(self.scale - scale);
        }
        self.units += mantissa;
    }

    /// Back to an exact decimal. Trailing zeros are stripped first, so a
    /// total fails only if its normalised form does not fit.
    fn into_decimal(mut self) -> Option<Decimal> {
        let ten = num_bigint::BigInt::from(10);
        let zero = num_bigint::BigInt::from(0);
        while self.scale > 0 && (&self.units % &ten) == zero {
            self.units /= &ten;
            self.scale -= 1;
        }
        let mantissa: i128 = self.units.try_into().ok()?;
        Decimal::try_from_i128_with_scale(mantissa, self.scale).ok()
    }
}

/// An exact i128 nanosecond total back to a span; `None` when it does
/// not fit a signed duration. Floor division can put a total at the
/// negative extreme one second below `i64::MIN`; carrying the remainder
/// back keeps that boundary representable.
fn nanos_to_duration(total: i128) -> Option<jiff::SignedDuration> {
    const NANOS: i128 = 1_000_000_000;
    let secs = total.div_euclid(NANOS);
    let nanos = total.rem_euclid(NANOS);
    if let Ok(s) = i64::try_from(secs) {
        return Some(jiff::SignedDuration::new(s, nanos as i32));
    }
    let s = i64::try_from(secs + 1).ok()?;
    Some(jiff::SignedDuration::new(s, (nanos - NANOS) as i32))
}

/// `min(a, b)` / `max(a, b)` over two finished values.
///
/// Defined on ordered kinds, same kind only, and quantities only under
/// the same unit. The aggregate forms over a proposition are
/// [`ValueExpr::Extremum`].
fn extremum_of(builtin: Builtin, a: &EvalValue, b: &EvalValue) -> Result<EvalValue, EvalError> {
    let take_min = builtin == Builtin::Min;
    match (a, b) {
        (EvalValue::Decimal(x), EvalValue::Decimal(y)) => Ok(EvalValue::Decimal(if take_min {
            *x.min(y)
        } else {
            *x.max(y)
        })),
        (EvalValue::Duration(x), EvalValue::Duration(y)) => Ok(EvalValue::Duration(if take_min {
            *x.min(y)
        } else {
            *x.max(y)
        })),
        (EvalValue::Date(x), EvalValue::Date(y)) => Ok(EvalValue::Date(if take_min {
            *x.min(y)
        } else {
            *x.max(y)
        })),
        (EvalValue::Timestamp(x), EvalValue::Timestamp(y)) => {
            Ok(EvalValue::Timestamp(if take_min {
                *x.min(y)
            } else {
                *x.max(y)
            }))
        }
        (
            EvalValue::Quantity {
                amount: x,
                unit: ux,
            },
            EvalValue::Quantity {
                amount: y,
                unit: uy,
            },
        ) if ux == uy => Ok(EvalValue::Quantity {
            amount: if take_min { *x.min(y) } else { *x.max(y) },
            unit: ux.clone(),
        }),
        (x, y) => Err(EvalError::TypeMismatch(format!(
            "{} is defined on two ordered values of the same kind, not {} and {}",
            builtin.name(),
            runtime_kind_label(x),
            runtime_kind_label(y)
        ))),
    }
}

/// All raw decimal arithmetic goes through here, checked, because the
/// plain rust_decimal operators panic on overflow (division too, with a
/// tiny divisor). A zero divisor gets its own error.
fn checked_decimal_op(op: ArithOp, a: Decimal, b: Decimal) -> Result<Decimal, EvalError> {
    let result = match op {
        ArithOp::Add => a.checked_add(b),
        ArithOp::Sub => a.checked_sub(b),
        ArithOp::Mul => a.checked_mul(b),
        ArithOp::Div => {
            if b == Decimal::ZERO {
                return Err(EvalError::DivisionByZero);
            }
            a.checked_div(b)
        }
        ArithOp::Mod => {
            if b == Decimal::ZERO {
                return Err(EvalError::DivisionByZero);
            }
            a.checked_rem(b)
        }
    };
    result.ok_or_else(|| {
        EvalError::ArithOutOfRange(format!("{a} {op:?} {b} exceeds the exact decimal range"))
    })
}

/// The multiple of `quantum` nearest to `v`, halves away from zero.
/// Works from the remainder, not the quotient: `round(8, 1e-28)` is
/// exactly 8, but the quotient 8e28 overflows. Fails only when the
/// nearest multiple itself is out of range.
fn round_decimal(v: Decimal, q: Decimal) -> Result<Decimal, EvalError> {
    let out_of_range = || EvalError::RoundOutOfRange {
        value: v.to_string(),
        quantum: q.to_string(),
    };
    let rem = v.checked_rem(q).ok_or_else(out_of_range)?;
    if rem.is_zero() {
        return Ok(v.normalize());
    }
    let toward_zero = v.checked_sub(rem).ok_or_else(out_of_range)?;
    let away = if v.is_sign_negative() {
        toward_zero.checked_sub(q)
    } else {
        toward_zero.checked_add(q)
    };
    let abs_rem = rem.abs();
    // Away wins when twice the remainder >= quantum; a doubling that
    // overflows is certainly larger.
    let away_wins = match abs_rem.checked_add(abs_rem) {
        Some(doubled) => doubled >= q,
        None => true,
    };
    let result = if away_wins {
        away.ok_or_else(out_of_range)?
    } else {
        toward_zero
    };
    Ok(result.normalize())
}

/// The runtime value of a literal, parsed as the evaluator parses it.
pub fn literal_value(value: &Value) -> Result<EvalValue, EvalError> {
    resolve_term(&Term::Literal(value.clone()), &Bindings::new(), None)
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
        Term::Literal(Value::Timestamp(s)) => Ok(EvalValue::Timestamp(parse_timestamp_literal(s)?)),
        Term::Literal(Value::Duration(s)) => Ok(EvalValue::Duration(parse_duration_literal(s)?)),
        Term::Literal(Value::CalendarSpan(s)) => {
            Ok(EvalValue::CalendarSpan(parse_calendar_span_literal(s)?))
        }
        Term::Literal(Value::Quantity { amount, unit }) => {
            Ok(parse_quantity_literal(amount, unit)?)
        }
        Term::Actor => actor
            .map(|a| EvalValue::Subject(a.clone()))
            .ok_or(EvalError::UnboundActor),
    }
}

/// On a failing proposition, return the most specific sub-proposition
/// responsible, rendered via [`crate::format::format_prop_inline`], or
/// `None` when no drill-down applies.
///
/// Called from [`crate::propose::execute_stmt`] only when a `Require` or
/// `BindOne` fails, so the success path pays nothing.
///
/// Drill-down rules:
///
/// - `And(conjuncts)`: recurse into the first conjunct that kills the
///   chain; render it as-is if nothing more specific is found.
/// - `Implies { left, right }`: if `left` held, recurse into `right`;
///   if not, the implication holds - `None`.
/// - `Forall { binding, source, body }`: recurse into `body` under the
///   first source match where it fails. Binding values are not
///   substituted into the rendering.
/// - `Defined`: recurse into the body under the call's frame, rendered
///   as `inside <call>: <body part>`.
/// - `Not`, `Exists`, `Or`, `Xor`, `Pre`: `None`. No single part is to
///   blame.
/// - Leaf expressions: `None`; already as specific as possible.
pub(crate) fn find_failing_subexpr(prop: &Prop, ctx: &EvalContext<'_>) -> Option<String> {
    find_failure(prop, ctx).map(|failure| failure.rendered)
}

/// The failing sub-expression with the bindings live where it failed.
/// Found in one descent, so the rendering and the values always blame
/// the same iteration.
pub(crate) struct Failure {
    pub(crate) rendered: String,
    /// Bindings in scope at the failure. Inside a definition body these
    /// are its parameters, matching the names in the rendering.
    pub(crate) bindings: Bindings,
}

impl Failure {
    pub(crate) fn here(prop: &Prop, ctx: &EvalContext<'_>) -> Self {
        Self {
            rendered: crate::format::format_prop_inline(prop),
            bindings: ctx.bindings.clone(),
        }
    }
}

/// Descend to the most specific failing sub-expression, carrying the
/// binding context. `find_failing_subexpr` is the rendering-only view.
pub(crate) fn find_failure(prop: &Prop, ctx: &EvalContext<'_>) -> Option<Failure> {
    match prop {
        Prop::And(conjuncts) => {
            // Thread bindings as `find_conjunction` does. Checking each
            // conjunct against the original bindings would miss
            // `And(A(x), B(x))` failing when `A(a1)` and `B(b2)` each hold
            // but no `x` satisfies both.
            let mut current: Vec<Bindings> = vec![ctx.bindings.clone()];
            for c in conjuncts {
                let mut next: Vec<Bindings> = Vec::new();
                for b in &current {
                    next.extend(find_matches(c, &ctx.with_bindings(b)).ok()?);
                }
                if next.is_empty() {
                    // This conjunct kills the chain. Diagnose under the
                    // first surviving binding context.
                    let failing_bindings = current.first().unwrap_or(ctx.bindings);
                    let failing_ctx = ctx.with_bindings(failing_bindings);
                    return Some(
                        find_failure(c, &failing_ctx)
                            .unwrap_or_else(|| Failure::here(c, &failing_ctx)),
                    );
                }
                current = next;
            }
            None
        }
        Prop::Implies { left, right } => {
            let left_matches = find_matches(left, ctx).ok()?;
            if left_matches.is_empty() {
                // Vacuously true when left fails.
                return None;
            }
            // Recurse into right under the first left match where it
            // fails.
            for ext in &left_matches {
                let ext_ctx = ctx.with_bindings(ext);
                let right_matches = find_matches(right, &ext_ctx).ok()?;
                if right_matches.is_empty() {
                    return Some(
                        find_failure(right, &ext_ctx)
                            .unwrap_or_else(|| Failure::here(right, &ext_ctx)),
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
            // Iterate exactly as `find_matches` does, so the blamed
            // iteration is one the evaluator actually tried.
            let source_matches = find_matches(source, ctx).ok()?;
            for ext in &source_matches {
                let ext_ctx = ctx.with_bindings(ext);
                let body_matches = find_matches(body, &ext_ctx).ok()?;
                if body_matches.is_empty() {
                    return Some(
                        find_failure(body, &ext_ctx)
                            .unwrap_or_else(|| Failure::here(body, &ext_ctx)),
                    );
                }
            }
            None
        }
        // Drill into the body under the call's frame. The rendering
        // keeps both levels: the named condition, then the failing body
        // part, with parameter names rather than values.
        Prop::Defined { name, args } => {
            let def = ctx.definitions.get(name)?;
            let frame = definition_call_frame(def, args, ctx).ok()?;
            let body_ctx = ctx.enter_definition(&frame);
            let inner = find_failure(&def.body, &body_ctx)
                .unwrap_or_else(|| Failure::here(&def.body, &body_ctx));
            Some(Failure {
                rendered: format!(
                    "inside {}: {}",
                    crate::format::format_prop_inline(prop),
                    inner.rendered
                ),
                bindings: inner.bindings,
            })
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
/// arguments resolved under the bindings live at the rejection.
///
/// Carried on the rejection trace (see [`crate::RequireOutcome`]) so the
/// explanation engine can find suppliers by `predicate` name, never by
/// parsing `rendered`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RenderedClaim {
    pub predicate: String,
    pub rendered: String,
}

/// On a failing gate, return the positive claims directly responsible:
/// the "directly missing claims" the explanation engine reports.
/// Conjunctions thread bindings as the kernel's `And` does.
///
/// Deliberately narrow:
///
/// - a top-level [`Prop::Claim`] that did not match -> that claim;
/// - a top-level [`Prop::Defined`] call -> the same, found in its body
///   under the call's frame;
/// - a top-level [`Prop::And`] -> the first conjunct that kills the
///   chain, if it is a positive `Claim` or a `Defined` call.
///
/// Everything else returns empty: `Or`, `Xor`, `Not`, `Exists`,
/// `Implies`, `Forall`, `Pre`, the comparisons, and an `And` killed by
/// any other conjunct. Those rejections have no directly missing claim.
///
/// Called only on the rejection branch, so the success path pays
/// nothing.
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
        // A gate factored into a named condition reports the same
        // missing claims its inline form would.
        Prop::Defined { name, args } => {
            let Some(def) = ctx.definitions.get(name) else {
                return vec![];
            };
            let Ok(frame) = definition_call_frame(def, args, ctx) else {
                return vec![];
            };
            unsatisfied_positive_claims(&def.body, &ctx.enter_definition(&frame))
        }
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
                    // `c` killed the chain. Only a positive claim or a
                    // defined call has a directly missing claim.
                    let failing_bindings = current.first().unwrap_or(ctx.bindings);
                    let fctx = ctx.with_bindings(failing_bindings);
                    return match c {
                        Prop::Claim { .. } => vec![render_claim(c, &fctx)],
                        Prop::Defined { .. } => unsatisfied_positive_claims(c, &fctx),
                        _ => vec![],
                    };
                }
                current = next;
            }
            vec![]
        }
        _ => vec![],
    }
}

/// Render a `Prop::Claim` with its arguments resolved under `ctx`:
/// `MayApprove(alice, contract)`, not `MayApprove(actor, doc_type)`. An
/// unresolvable term keeps its symbolic form. Panics on a non-`Claim`.
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
            // A malformed literal fails to parse; render it rather than
            // panic.
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
        EvalValue::Timestamp(t) => t.to_string(),
        EvalValue::Duration(d) => d.to_string(),
        EvalValue::CalendarSpan(s) => s.to_string(),
        EvalValue::Quantity { amount, unit } => format!("{amount} {unit}"),
        EvalValue::Collection(items) => {
            let inner: Vec<String> = items.iter().map(render_eval_value).collect();
            format!("[{}]", inner.join(", "))
        }
    }
}

/// Does this value belong to a kind with an order at all? Subjects and
/// booleans do not.
///
/// The static checker refuses unordered kinds; this covers hand-built IR
/// and fields the checker could not narrow.
fn ensure_ordered(value: &EvalValue, op: crate::ir::ExtremumOp) -> Result<(), EvalError> {
    match value {
        EvalValue::Decimal(_)
        | EvalValue::Date(_)
        | EvalValue::Timestamp(_)
        | EvalValue::Duration(_)
        | EvalValue::Quantity { .. } => Ok(()),
        other => Err(EvalError::TypeMismatch(format!(
            "{} needs an ordered kind, got {other:?}",
            op.as_str()
        ))),
    }
}

/// Should an extremum keep `candidate` over `current`? Ordered kinds
/// only; quantities compare only within one unit.
fn compare_ordered(
    current: &EvalValue,
    candidate: &EvalValue,
    op: crate::ir::ExtremumOp,
) -> Result<bool, EvalError> {
    use std::cmp::Ordering;
    let ordering = match (current, candidate) {
        (EvalValue::Decimal(a), EvalValue::Decimal(b)) => a.cmp(b),
        (EvalValue::Date(a), EvalValue::Date(b)) => a.cmp(b),
        (EvalValue::Timestamp(a), EvalValue::Timestamp(b)) => a.cmp(b),
        (EvalValue::Duration(a), EvalValue::Duration(b)) => a.cmp(b),
        (
            EvalValue::Quantity {
                amount: a,
                unit: ua,
            },
            EvalValue::Quantity {
                amount: b,
                unit: ub,
            },
        ) => {
            if ua != ub {
                return Err(EvalError::TypeMismatch(format!(
                    "{} cannot compare {ua} with {ub}: quantities order only within one unit",
                    op.as_str()
                )));
            }
            a.cmp(b)
        }
        (a, b) => {
            return Err(EvalError::TypeMismatch(format!(
                "{} needs an ordered kind, got {a:?} and {b:?}",
                op.as_str()
            )));
        }
    };
    Ok(match op {
        crate::ir::ExtremumOp::Max => ordering == Ordering::Less,
        crate::ir::ExtremumOp::Min => ordering == Ordering::Greater,
    })
}

#[cfg(test)]
mod tests;
