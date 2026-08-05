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

use crate::definitions::DefinitionTable;
use crate::ir::{
    ArithOp, Builtin, CompareOp, Definition, DefinitionName, OrderedDomain, PredicateName, Prop,
    Subject, Term, Value, ValueExpr, Var,
};
use crate::state::{Bindings, ClaimInstance, EvalValue, State};

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
    /// An expression demanded an operand of one kind but received
    /// another (e.g. arithmetic on a subject, membership on a non-
    /// collection, etc.).
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
    /// `Term::Actor` was referenced with no transition in scope (the
    /// evaluator was called with `actor = None`): invariant or
    /// derived-claim bodies, which evaluate against admitted state, not
    /// a proposing transition. Authority checks belong in `require`, not
    /// invariants; this error makes that doctrine enforceable.
    #[error(
        "Term::Actor referenced with no transition in scope (likely used outside a transformation body - e.g., inside an invariant or derived-claim body; authority checks belong in `require`)"
    )]
    UnboundActor,
    /// `Prop::Pre` was reached with no pre-state in scope: derived-claim
    /// bodies, transformation `require`s, the inner of nested `pre`, or
    /// an `EvalContext` built with `pre_state: None`. Phrased about
    /// evaluation context, not AST position, so future contexts that
    /// carry both states share the primitive without IR change.
    #[error(
        "Prop::Pre evaluated with no pre-state in scope (a derived-claim body, a transformation `require`, the inner of nested `pre`, or an EvalContext built with pre_state: None)"
    )]
    PreStateUnavailable,
    /// An `ArithOp::Div` or `ArithOp::Mod` evaluated with a zero divisor.
    /// A rule that divides (or takes a remainder) by zero cannot be
    /// evaluated, so it surfaces here rather than producing a value; the
    /// proposal is rejected (or the derived read errors). Gates avoid this
    /// by cross-multiplying with `Mul`.
    #[error("division by zero")]
    DivisionByZero,
    /// A `round(x, quantum)` evaluated with a zero or negative quantum.
    /// "Round to the nearest multiple of nothing" has no meaning, and a
    /// negative quantum is a sign error in the rule, not a policy - both
    /// are refused by name. `Program::validate` catches a literal
    /// non-positive quantum at authoring time; this is the runtime
    /// backstop for a quantum that arrives through a variable.
    #[error("round quantum must be positive, got {0}")]
    RoundQuantumNotPositive(String),
    /// A `period_index(anchor, span, at)` whose span is zero: a
    /// period needs a positive span, or every date would sit in
    /// infinitely many periods at once. `Program::validate` catches a
    /// literal zero span at authoring time; this is the runtime
    /// backstop for a span that arrives through a defined-call
    /// parameter.
    #[error("period_index needs a positive span; got {0}")]
    PeriodSpanNotPositive(String),
    /// A `round(x, quantum)` whose exact answer cannot be represented:
    /// the nearest multiple of the quantum lies outside the decimal
    /// range (or the operands' scales exceed what exact remainder
    /// arithmetic can hold). The kernel's contract is exactness or a
    /// named refusal - never an approximation, never a panic.
    #[error("round out of decimal range: no representable multiple of {quantum} near {value}")]
    RoundOutOfRange { value: String, quantum: String },
    /// An arithmetic result outside the exact decimal (or time) range.
    /// Same contract as [`EvalError::RoundOutOfRange`]: exactness or a
    /// named refusal - never an approximation, never a panic. The plain
    /// rust_decimal operators panic on overflow, so every arithmetic
    /// site goes through checked variants.
    ///
    /// Classification, like every `EvalError`: a KERNEL evaluation
    /// error (`PgError::Kernel` at the adapter, the error envelope at
    /// the CLI) - operational, with no audit standing - never a
    /// business rejection. The same line `DivisionByZero` draws: the
    /// rule as written cannot be evaluated over this state. A domain
    /// where extremes are business-possible range-guards them in its
    /// own rules, as the metered-billing example does for negatives.
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
    /// does not carry. Unlike an unmatched predicate (which lawfully
    /// matches nothing), a call without its definition is a programme
    /// integrity error: there is no body to expand. `Program::validate`
    /// catches this at authoring time; the runtime error is the backstop
    /// for unvalidated IR.
    #[error(
        "call to definition `{0}` but the evaluation context carries no such definition; validate the programme before proposing"
    )]
    UnknownDefinition(String),
}

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

    /// Enter a definition body: a fresh frame carrying only the call's
    /// parameter bindings, with no pre-state and no actor. Bodies are
    /// context-free by doctrine; the statically-banned `pre(...)` and
    /// `actor` surface their usual context errors here if unvalidated
    /// IR carries them.
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
        // Quantities ride the decimal ordered domain - a `Decimal[U]`
        // IS an exact decimal, under a contractual label the comparison
        // must respect: same unit or no verdict at all.
        (
            OrderedDomain::Decimal,
            EvalValue::Quantity { amount: a, unit: u },
            EvalValue::Quantity { amount: b, unit: v },
        ) => {
            if u != v {
                return Err(EvalError::TypeMismatch(format!(
                    "cannot compare Decimal[{u}] with Decimal[{v}]: \
                     comparison requires the same unit"
                )));
            }
            apply_cmp(op, a, b)
        }
        (OrderedDomain::Date, EvalValue::Date(a), EvalValue::Date(b)) => apply_cmp(op, a, b),
        (OrderedDomain::Timestamp, EvalValue::Timestamp(a), EvalValue::Timestamp(b)) => {
            apply_cmp(op, a, b)
        }
        (OrderedDomain::Duration, EvalValue::Duration(a), EvalValue::Duration(b)) => {
            apply_cmp(op, a, b)
        }
        (OrderedDomain::Decimal, l, r) => {
            return Err(EvalError::TypeMismatch(format!(
                "comparison expects two decimal-domain operands of one flavour \
                 (bare decimals, or quantities of the same unit); got {} vs {}",
                runtime_kind_label(&l),
                runtime_kind_label(&r),
            )));
        }
        (OrderedDomain::Date, _, _) => {
            return Err(EvalError::TypeMismatch(
                "comparison expects civil-date operands".to_string(),
            ));
        }
        (OrderedDomain::Timestamp, _, _) => {
            return Err(EvalError::TypeMismatch(
                "comparison expects timestamp operands".to_string(),
            ));
        }
        (OrderedDomain::Duration, _, _) => {
            return Err(EvalError::TypeMismatch(
                "comparison expects duration operands".to_string(),
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

/// Build a definition call's frame: the bindings the body evaluates
/// under. Only the parameters appear - a ground argument (a literal,
/// `actor`, or a bound variable) pre-binds its parameter; an unbound
/// variable or wildcard leaves it free, so the body acts as a generator
/// for that position. The caller's other bindings never enter the frame:
/// a definition body cannot capture surrounding scope.
///
/// This is the one place a call's argument-to-parameter translation is
/// computed; the evaluator, the failure walk, and the missing-claims
/// walk all build their body context from it.
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
            // Literals and `actor` resolve at the call site: the actor
            // stays a caller-context concern, never visible to the body
            // as anything but an ordinary subject value.
            other => {
                frame.insert(param.clone(), resolve_term(other, ctx.bindings, ctx.actor)?);
            }
        }
    }
    Ok(frame)
}

/// Evaluate a call to a named definition: relational substitution with
/// projection. The body runs under the call frame (see
/// [`definition_call_frame`]); each body match projects its parameter
/// values back onto the call's argument terms, extending the caller's
/// bindings. Projection deduplicates - a call yields each distinct
/// argument-binding witness once, so internal multiplicity (two
/// different internal witnesses for the same projected arguments) is
/// not observable and cannot double-count inside a `Sum`.
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
            // Validation guarantees the body binds every parameter; for
            // unvalidated IR the absence surfaces as the same error the
            // kernel raises for any unbound name.
            let Some(value) = m.get(param) else {
                return Err(EvalError::UnboundVariable(param.to_string()));
            };
            if let Term::Var(v) = arg {
                match extended.get(v) {
                    // A pre-bound variable already agrees (the frame
                    // pinned it); a repeated unbound variable in the call
                    // (`f(x, x)`) must project consistently or the match
                    // is discarded.
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

pub(crate) fn find_matches(p: &Prop, ctx: &EvalContext<'_>) -> Result<Vec<Bindings>, EvalError> {
    match p {
        Prop::Claim { predicate, args } => find_claim_matches(predicate, args, ctx),
        Prop::Defined { name, args } => find_defined_matches(name, args, ctx),
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

/// Parse a `Value::Timestamp(String)` literal into a [`jiff::Timestamp`]
/// (RFC 3339). Centralised for the same drift-prevention reason as
/// [`parse_date_literal`].
pub(crate) fn parse_timestamp_literal(s: &str) -> Result<jiff::Timestamp, EvalError> {
    s.parse::<jiff::Timestamp>()
        .map_err(|e| EvalError::TypeMismatch(format!("invalid timestamp `{s}`: {e}")))
}

/// An exact span as a decimal count of nanoseconds - the common
/// integer representation under which two durations divide exactly.
/// A [`jiff::SignedDuration`]'s magnitude always fits a `Decimal`
/// (|i64 seconds| * 1e9 + nanos < 2^96), so the conversion is total.
fn duration_nanos_decimal(d: jiff::SignedDuration) -> Decimal {
    let nanos = i128::from(d.as_secs()) * 1_000_000_000 + i128::from(d.subsec_nanos());
    Decimal::try_from_i128_with_scale(nanos, 0)
        .unwrap_or_else(|_| unreachable!("duration nanoseconds always fit a Decimal"))
}

/// The author-facing label for a runtime value's arithmetic kind, for
/// no-rule errors: units must appear (`Decimal[USD]`), never a
/// unit-erased "quantity".
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
/// quantity value, keeping the unit label. Centralised for the same
/// drift-prevention reason as [`parse_date_literal`].
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

/// Parse a `Value::CalendarSpan(String)` literal through the kernel's
/// own grammar ([`crate::calendar::parse_calendar_span`]). Centralised
/// for the same drift-prevention reason as [`parse_date_literal`]; the
/// surface diagnostic path routes through the same grammar.
pub(crate) fn parse_calendar_span_literal(
    s: &str,
) -> Result<crate::calendar::CalendarSpan, EvalError> {
    crate::calendar::parse_calendar_span(s)
        .map_err(|e| EvalError::TypeMismatch(format!("invalid calendar span `{s}`: {e}")))
}

/// Shift a civil date by whole months, then whole days - the kernel's
/// own calendar-shift semantics, spelled out rather than delegated:
/// the month component moves the (year, month) coordinate and clamps
/// the day to the destination month's length; the day component then
/// steps the calendar day by day. `None` when the result leaves the
/// representable calendar.
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
/// `boundary(n) <= at`, where boundary(n) is the anchor shifted by the
/// span's components multiplied by n ONCE (never n repeated clamped
/// hops - the calendar's non-associativity would let those drift).
/// Representable boundaries form half-open periods `[B(n), B(n+1))`;
/// a boundary shifted below or above the representable calendar acts
/// as negative or positive infinity respectively, so the FIRST AND
/// LAST PERIODS ARE CLIPPED to the calendar and the operation is
/// total for every representable (anchor, at) pair. Exact
/// anniversaries enter the new period; dates before the anchor take
/// negative indexes. Binary search over the monotone boundary
/// sequence within fixed calendar-derived bounds: correctness over
/// micro-optimisation.
fn period_index_of(
    anchor: Date,
    span: crate::calendar::CalendarSpan,
    at: Date,
) -> Result<i64, EvalError> {
    if span.months < 0 || span.days < 0 || (span.months == 0 && span.days == 0) {
        return Err(EvalError::PeriodSpanNotPositive(span.to_string()));
    }
    let boundary = |n: i64| -> Option<Date> {
        shift_date(
            anchor,
            n.checked_mul(i64::from(span.months))?,
            n.checked_mul(i64::from(span.days))?,
        )
    };
    // A boundary at or before `at`? Out-of-range candidates count as
    // "no" above the calendar and "yes" below it.
    let at_or_before = |n: i64| -> bool {
        match boundary(n) {
            Some(b) => b <= at,
            // The shift left the calendar. A positive multiple of a
            // positive span can only leave upward and a negative one
            // only downward, so this IS the clipped-period sentinel:
            // below the calendar reads as negative infinity (always
            // at-or-before), above as positive infinity (never).
            None => n < 0,
        }
    };
    // The bracket is derived from the calendar itself, not a magic
    // number: every accepted positive span advances adjacent
    // boundaries by at least one civil day, so the index magnitude
    // between any two representable dates cannot exceed the
    // calendar's whole day range (plus room for the clipped outer
    // period). A widened date representation widens the bound with
    // it.
    let cap: i64 = days_between(Date::MIN, Date::MAX) + 2;
    let (mut lo, mut hi) = (-cap, cap);
    // Enforced in release builds, not merely debug: this operator
    // promises an exact total answer, and a broken bracket must be a
    // loud programmer-error stop, never a silently wrong index. By
    // construction it cannot fire (see the bound's derivation); an
    // internal guard on a structurally-impossible state is within
    // the kernel's no-panic-on-input doctrine.
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

/// The signed count of civil days from `from` to `to` (positive when
/// `to` is later). Total for in-range dates: the difference of two
/// representable dates always fits the day unit.
fn days_between(from: Date, to: Date) -> i64 {
    from.until(to)
        .map(|span| i64::from(span.get_days()))
        .unwrap_or_else(|_| unreachable!("the gap between two civil dates always fits in days"))
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

/// Structural pattern match that never clones the `Bindings` environment:
/// returns the new variable bindings as borrowed references (collected in
/// small scratch storage), or `None` on a mismatch. A repeated variable within one pattern binds consistently -
/// checked against `base`, then the bindings made so far. The literal and
/// actor arms are pure read-only checks. Shared by [`unify_args`] (which
/// builds the environment) and [`claim_matches`] (which only asks whether
/// a match exists), so the two cannot drift.
///
/// Owns the same-arity invariant: a pattern and value list of different
/// lengths never match. As the single matcher, it holds this itself
/// rather than trusting every caller to pre-check (the `zip` below would
/// otherwise prefix-match).
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
/// bindings. The `base` environment map is cloned **once, only on a
/// verified match** - a mismatch never clones it (though `match_args` may
/// still use small scratch storage before failing).
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
/// building the `Bindings` environment - the guard for the lookup paths
/// (`matching_claims`, `ValueOf`) that need only a yes/no, never the
/// resulting bindings. It never clones the base map (it uses small scratch
/// storage for fresh-variable consistency). Shares [`match_args`] with
/// [`unify_args`].
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
        // The exists-style test (`Prop::Exists` is the ancestor, not
        // `Sum`): at least one witness selects `then`, none selects
        // `otherwise`, and the witnesses' bindings are DISCARDED -
        // `require`'s non-export rule. Only the selected branch
        // evaluates, so an error in the untaken branch cannot
        // surface; an error in the condition itself propagates -
        // an undecidable condition never silently selects.
        // Every builtin is strict: arguments evaluate left to right,
        // exactly once, and the FIRST one to fail is the error the
        // caller sees - so a diagnostic still points at the operand a
        // reader would blame. Nothing about the call reaches state,
        // bindings, or the actor; `eval_builtin` sees finished values
        // only, which is what makes the contract checkable.
        ValueExpr::Call { builtin, args } => {
            // Arity first, BEFORE any argument is touched. A call with
            // the wrong shape is wrong about itself, and an operand
            // error from the surplus argument would answer a question
            // nobody asked - the reader needs to hear "abs takes 1
            // argument", not "y is unbound". Only hand-built IR
            // reaches this; validation refuses it earlier.
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
            // Materialising every witness to answer a yes/no is the
            // cost `Prop::Exists`, `Not`, `Implies`, and the `require`
            // gate all pay today; a short-circuiting truth entry point
            // would be one refactor across all five sites, taken when
            // a measurement forces it, not piecemeal here.
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
                // The time-arithmetic matrix the laytime example
                // forces: an instant shifted by an exact span stays an
                // instant; the gap between two instants is a span;
                // spans add, subtract, and cap. Everything else
                // (multiplying instants, modding durations) is a
                // type error until an example argues otherwise.
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
                // The civil-date rules. A calendar span shifts a date:
                // months first (the day clamped to the destination
                // month's length, so Jan 31 + P1M is Feb 28 or 29),
                // then days as plain civil-day steps. Subtraction is
                // the same walk with both components negated - which
                // makes the shift neither reversible nor associative
                // around clamped month ends; that is the calendar's
                // own behaviour, not an approximation.
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
                // The gap between two dates is their signed count of
                // actual days, as a decimal: the ACT numerator every
                // actual-day count convention starts from.
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
                    // The ratio between two spans is a dimensionless
                    // decimal - how many turn-times fit in the excess,
                    // how many days of demurrage. The inputs are exact
                    // integer nanoseconds and the division is Decimal
                    // division: terminating ratios (132h/24h = 5.5)
                    // are exact; a repeating ratio carries Decimal's
                    // 28-digit precision. Money settled off a ratio
                    // eventually wants an explicit rounding rule -
                    // a domain decision, not a hidden kernel one.
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
                // The unit algebra, deliberately minimal: a quantity is
                // an exact decimal under a contractual label, so amounts
                // combine only under the SAME label; a bare decimal
                // scales a quantity (Mul/Div); the ratio of two
                // same-unit quantities is a bare decimal. There is no
                // unit-producing arithmetic - no compound units, ever,
                // until a worked example forces a revisit.
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
            // Order over a set, so the answer cannot depend on match
            // order: every candidate is compared against the running
            // best, and mixing kinds is an error rather than an
            // arbitrary winner.
            let matches = find_matches(body, ctx)?;
            let mut best: Option<EvalValue> = None;
            for m in matches {
                let next = resolve_term(value, &m, ctx.actor)?;
                best = Some(match best {
                    None => {
                        // Checked before it becomes the running best:
                        // otherwise one unordered match succeeds and two
                        // raise a type error, which makes validity a
                        // question of cardinality.
                        ensure_ordered(&next, *op)?;
                        next
                    }
                    Some(current) => {
                        let ordering = compare_ordered(&current, &next, *op)?;
                        if ordering { next } else { current }
                    }
                });
            }
            // An empty sum has a typed zero to fall back on; an empty
            // extremum has nothing. Refusing by name beats inventing a
            // winner - guard with a `require` for a lawful rejection.
            best.ok_or_else(|| EvalError::EmptyExtremum {
                op: op.as_str(),
                body: crate::format::format_prop_inline(body),
            })
        }
        ValueExpr::Sum { value, body, seed } => {
            // Type-driven accumulation: a sum of decimals is a decimal,
            // a sum of durations is a duration (counted laytime is the
            // forcing case), a sum of same-unit quantities is a quantity
            // of that unit. The empty sum is the lowered `seed` - the
            // typed zero of the summed variable's declared kind, so an
            // empty cargo book is `0 t` and an empty time book `PT0S`,
            // with no zero-valued seed claim needed to open either.
            // Mixing kinds, or units within the quantity kind, is an
            // error.
            //
            // The accumulators are WIDER than the value types, so only
            // the FINAL total decides representability. State is a set
            // of claims: a checked fold in match order would let an
            // intermediate overflow refuse one ordering of [MAX, 1,
            // -MAX] and accept another - the answer must depend on the
            // set, never the iteration history.
            enum SumTotal {
                Empty,
                Decimal(BigSum),
                Duration(i128),
                Quantity(BigSum, crate::ir::Unit),
            }
            let matches = find_matches(body, ctx)?;
            let mut total = SumTotal::Empty;
            for m in matches {
                // The target is a full value expression consuming this
                // witness's bindings, evaluated exactly once per match.
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
                        // i128 nanoseconds cannot overflow from claim
                        // counts a real state can hold: each element is
                        // within +-9.3e27ns, so exceeding i128 needs
                        // more claims than any memory can carry.
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
                SumTotal::Decimal(t) => EvalValue::Decimal(t.into_decimal().ok_or_else(|| {
                    EvalError::ArithOutOfRange(
                        "sum of decimals exceeds the exact decimal range".to_string(),
                    )
                })?),
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

/// The arity a builtin takes, refused by name when it does not match.
/// Checked before arguments are evaluated so a misshapen call is
/// reported as such, and again inside [`eval_builtin`] so no caller
/// can reach the operation with the wrong count.
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
/// Exhaustive over [`Builtin`] with no wildcard: a new builtin cannot
/// reach here without an author deciding what it computes. The arity
/// check ahead of the match is a backstop for hand-built IR that never
/// went through validation - a validated programme cannot arrive with
/// the wrong count.
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
        Builtin::Min | Builtin::Max => extremum_of(builtin, &args[0], &args[1]),
    }
}

/// Exact running total of decimals: a big-integer count of units at
/// the largest scale seen. The fold itself cannot overflow, so the
/// FINAL total alone decides representability - order-independent by
/// construction, which is what makes a `sum` over set-valued state
/// well-defined at the range boundary.
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

    /// Back to an exact decimal: strip trailing zeros (a total of
    /// 1.50 + 0.50 is 2, not an unrepresentable 200 at scale 2 short
    /// of range), then refuse only if the normalised total genuinely
    /// does not fit.
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

/// An exact i128 nanosecond total back to a span; `None` when the
/// total lies outside what a signed duration can carry. Floor
/// division puts a total near the negative extreme one second below
/// `i64::MIN`; carrying the remainder back keeps the exact boundary
/// representable instead of refusing it.
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
/// Defined on the kinds the language orders. Same-kind only, for the
/// reason the comparators are: a length against a weight is a category
/// error, and two amounts compare only under the same unit label. The
/// aggregate forms over a proposition are [`ValueExpr::Extremum`] - a
/// construct, because they bind a variable and range over state.
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
        // Dates and instants order, so the earlier of two is an
        // answer - the same total order the comparators read.
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

/// The one place raw decimal arithmetic happens: checked throughout,
/// so an out-of-range result is a named refusal, never a panic - the
/// plain rust_decimal operators panic on overflow, including division
/// (a tiny divisor overflows the quotient). Zero divisors keep their
/// own name.
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

/// The multiple of `quantum` nearest to `v`, exact halves away from
/// zero. Remainder-and-distance, never a quotient: the count of quanta
/// in `v` can exceed the decimal range even when the rounded result is
/// perfectly representable (`round(8, 1e-28)` is exactly 8, but 8e28
/// overflows), so the quotient formulation panics on inputs whose
/// answer exists. Every step is checked; the one honest failure -
/// the nearest multiple itself is outside the decimal range - is a
/// named error, never a panic.
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
    // Away wins at twice-the-remainder >= quantum (the half case goes
    // away from zero); a doubling that overflows is certainly larger.
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
    find_failure(prop, ctx).map(|failure| failure.rendered)
}

/// The failing sub-expression together with the bindings that were live
/// where it failed - the witness. One descent serves both: rendering it
/// twice would let the string and the values disagree about which
/// iteration was blamed.
pub(crate) struct Failure {
    pub(crate) rendered: String,
    /// Bindings in scope at the failure. Inside a definition body these
    /// are the definition's parameters, which is what the rendering names
    /// there too.
    pub(crate) bindings: Bindings,
}

impl Failure {
    fn here(prop: &Prop, ctx: &EvalContext<'_>) -> Self {
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
                        find_failure(body, &ext_ctx)
                            .unwrap_or_else(|| Failure::here(body, &ext_ctx)),
                    );
                }
            }
            None
        }
        // A failing call drills into the definition body under the
        // call's frame, and the rendering keeps both levels: the named
        // business condition first, the responsible body conjunct
        // second. Body conjuncts render with parameter names (binding
        // values are not substituted in v0, matching `Forall`).
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
        // A failing call descends into the body under the call's frame,
        // so a gate factored into a named condition reports the same
        // directly-missing claims its inline form would - with ground
        // call arguments resolved into the rendering.
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
                    // `c` killed the chain. Report it only when it is
                    // itself a positive claim or a defined call (which
                    // descends); a comparator/`not`/etc. failure has no
                    // directly-missing claim in v0.
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

/// Is `candidate` the one an extremum should keep over `current`?
///
/// Ordered kinds only. Subjects are opaque identifiers and booleans are
/// not a scale, so neither has a largest member - they are refused at
/// validation, and this is the runtime backstop for hand-built IR that
/// skipped it. Quantities compare only within one unit, like every other
/// quantity comparison.
/// Does this value belong to a kind with an order at all?
///
/// The static checker refuses unordered kinds, so this is the backstop
/// for hand-built IR and for a field the checker could not narrow. It has
/// to run on the first candidate as well as the comparisons, or a
/// singleton would slip through the gap between them.
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
mod tests {
    use super::*;
    use crate::ir_builder::{
        dec, div, max, min, modulo, mul, subj, term, value_of, value_of_with_default, wildcard,
    };
    use crate::state::{ClaimInstance, State};

    /// The extremum picks from a set, so match order must not decide it,
    /// and the answer must be the member itself rather than a position.
    ///
    /// Table-driven over both ends and both orderings of the same claims:
    /// a max that depended on iteration would pass one row and fail its
    /// mirror.
    #[test]
    fn an_extremum_picks_the_same_member_whatever_the_match_order() {
        use crate::ir::ExtremumOp;
        let rows = [("2025-01-01", 10), ("2026-01-01", 12), ("2024-06-01", 9)];
        let build = |order: &[usize]| {
            State::from_claims(
                order
                    .iter()
                    .map(|&i| ClaimInstance {
                        predicate: "Rate".into(),
                        args: vec![
                            EvalValue::Date(rows[i].0.parse().expect("date")),
                            EvalValue::Decimal(rust_decimal::Decimal::from(rows[i].1)),
                        ],
                    })
                    .collect(),
            )
        };
        let aggregate = |op| ValueExpr::Extremum {
            op,
            value: Term::Var(Var::from("d")),
            body: Box::new(crate::ir_builder::claim(
                "Rate",
                vec![Term::Var(Var::from("d")), Term::Wildcard],
            )),
        };
        for order in [[0usize, 1, 2], [2, 1, 0], [1, 0, 2]] {
            let state = build(&order);
            let bindings = Bindings::new();
            let ctx = EvalContext::new(
                &state,
                None,
                &bindings,
                None,
                crate::definitions::DefinitionTable::new(&[]),
            );
            assert_eq!(
                eval_value(&aggregate(ExtremumOp::Max), &ctx).expect("max"),
                EvalValue::Date("2026-01-01".parse().expect("date")),
                "max over {order:?}"
            );
            assert_eq!(
                eval_value(&aggregate(ExtremumOp::Min), &ctx).expect("min"),
                EvalValue::Date("2024-06-01".parse().expect("date")),
                "min over {order:?}"
            );
        }
    }

    /// Validity is a question of kind, never of how many claims happen
    /// to match.
    ///
    /// The first cut checked only comparisons, so one unordered match
    /// succeeded and two raised a type error - the same programme going
    /// from working to broken because a second claim was admitted. Every
    /// candidate is checked now, singletons included.
    #[test]
    fn an_unordered_candidate_is_refused_however_many_there_are() {
        use crate::ir::ExtremumOp;
        let unordered = [
            ("subject", EvalValue::Subject(Subject::from("a"))),
            ("bool", EvalValue::Bool(true)),
            (
                "collection",
                EvalValue::Collection(vec![EvalValue::Subject(Subject::from("x"))]),
            ),
        ];
        for (label, first) in unordered {
            for count in [1usize, 2] {
                let state = State::from_claims(
                    (0..count)
                        .map(|i| ClaimInstance {
                            predicate: "Thing".into(),
                            args: vec![if i == 0 {
                                first.clone()
                            } else {
                                EvalValue::Subject(Subject::from("b"))
                            }],
                        })
                        .collect(),
                );
                let bindings = Bindings::new();
                let ctx = EvalContext::new(
                    &state,
                    None,
                    &bindings,
                    None,
                    crate::definitions::DefinitionTable::new(&[]),
                );
                let expr = ValueExpr::Extremum {
                    op: ExtremumOp::Max,
                    value: Term::Var(Var::from("v")),
                    body: Box::new(crate::ir_builder::claim(
                        "Thing",
                        vec![Term::Var(Var::from("v"))],
                    )),
                };
                let err =
                    eval_value(&expr, &ctx).expect_err(&format!("{label} x{count} has no order"));
                assert!(
                    err.to_string().contains("ordered kind"),
                    "{label} x{count}: {err}"
                );
            }
        }
    }

    /// Every kind the checker admits must actually evaluate, or the
    /// allow-list and the runtime disagree about what is filterable.
    #[test]
    fn every_ordered_kind_yields_its_largest_member() {
        use crate::ir::ExtremumOp;
        let cases: Vec<(&str, Vec<EvalValue>, EvalValue)> = vec![
            (
                "decimal",
                vec![
                    EvalValue::Decimal(rust_decimal::Decimal::from(1)),
                    EvalValue::Decimal(rust_decimal::Decimal::from(9)),
                ],
                EvalValue::Decimal(rust_decimal::Decimal::from(9)),
            ),
            (
                "date",
                vec![
                    EvalValue::Date("2025-01-01".parse().expect("date")),
                    EvalValue::Date("2026-01-01".parse().expect("date")),
                ],
                EvalValue::Date("2026-01-01".parse().expect("date")),
            ),
            (
                "timestamp",
                vec![
                    EvalValue::Timestamp("2025-01-01T00:00:00Z".parse().expect("ts")),
                    EvalValue::Timestamp("2026-01-01T00:00:00Z".parse().expect("ts")),
                ],
                EvalValue::Timestamp("2026-01-01T00:00:00Z".parse().expect("ts")),
            ),
            (
                "duration",
                vec![
                    EvalValue::Duration("PT1H".parse().expect("dur")),
                    EvalValue::Duration("PT6H".parse().expect("dur")),
                ],
                EvalValue::Duration("PT6H".parse().expect("dur")),
            ),
            (
                "quantity",
                vec![
                    EvalValue::Quantity {
                        amount: rust_decimal::Decimal::from(5),
                        unit: "USD".into(),
                    },
                    EvalValue::Quantity {
                        amount: rust_decimal::Decimal::from(7),
                        unit: "USD".into(),
                    },
                ],
                EvalValue::Quantity {
                    amount: rust_decimal::Decimal::from(7),
                    unit: "USD".into(),
                },
            ),
        ];
        for (label, values, expected) in cases {
            let state = State::from_claims(
                values
                    .into_iter()
                    .map(|v| ClaimInstance {
                        predicate: "Thing".into(),
                        args: vec![v],
                    })
                    .collect(),
            );
            let bindings = Bindings::new();
            let ctx = EvalContext::new(
                &state,
                None,
                &bindings,
                None,
                crate::definitions::DefinitionTable::new(&[]),
            );
            let expr = ValueExpr::Extremum {
                op: ExtremumOp::Max,
                value: Term::Var(Var::from("v")),
                body: Box::new(crate::ir_builder::claim(
                    "Thing",
                    vec![Term::Var(Var::from("v"))],
                )),
            };
            assert_eq!(
                eval_value(&expr, &ctx).unwrap_or_else(|e| panic!("{label}: {e}")),
                expected,
                "{label}"
            );
        }
    }

    /// An empty sum has a typed zero to fall back on; an empty extremum
    /// has no answer, and inventing one would let a rule price against a
    /// version that does not exist. It names the body so the author can
    /// see which selection came up empty.
    #[test]
    fn an_extremum_over_nothing_refuses_by_name() {
        use crate::ir::ExtremumOp;
        let state = State::from_claims(vec![]);
        let bindings = Bindings::new();
        let ctx = EvalContext::new(
            &state,
            None,
            &bindings,
            None,
            crate::definitions::DefinitionTable::new(&[]),
        );
        let expr = ValueExpr::Extremum {
            op: ExtremumOp::Max,
            value: Term::Var(Var::from("d")),
            body: Box::new(crate::ir_builder::claim(
                "Rate",
                vec![Term::Var(Var::from("d"))],
            )),
        };
        let err = eval_value(&expr, &ctx).expect_err("an empty max has no value");
        let text = err.to_string();
        assert!(text.contains("matched nothing"), "got: {text}");
        assert!(text.contains("Rate"), "must name the body: {text}");
        assert!(text.contains("require"), "must name the remedy: {text}");
    }

    /// `claim_matches` and `unify_args` share `match_args`, so they must
    /// agree on every verdict; `unify_args` must additionally extend the
    /// base with exactly the new bindings. Pins that the boolean path
    /// cannot drift from the binding-producing one.
    #[test]
    fn claim_matches_agrees_with_unify_args_and_extends_base() {
        let s = |x: &str| EvalValue::Subject(Subject::from(x));
        let var = |x: &str| Term::Var(Var::from(x));
        let lit = |x: &str| Term::Literal(Value::Subject(x.into()));
        let actor = Subject::from("alice");
        let mut base = Bindings::new();
        base.insert(Var::from("known"), s("k"));

        let cases: Vec<(Vec<Term>, Vec<EvalValue>, bool)> = vec![
            (vec![var("x"), var("y")], vec![s("a"), s("b")], true), // fresh vars
            (vec![var("x"), var("x")], vec![s("a"), s("a")], true), // repeated var, consistent
            (vec![var("x"), var("x")], vec![s("a"), s("b")], false), // repeated var, conflict
            (vec![lit("a")], vec![s("a")], true),                   // literal match
            (vec![lit("a")], vec![s("b")], false),                  // literal mismatch
            (vec![Term::Wildcard], vec![s("z")], true),             // wildcard
            (vec![Term::Actor], vec![s("alice")], true),            // actor match
            (vec![Term::Actor], vec![s("bob")], false),             // actor mismatch
            (vec![var("known")], vec![s("k")], true),               // agrees with base
            (vec![var("known")], vec![s("other")], false),          // conflicts with base
            (vec![var("x"), var("y")], vec![s("a")], false),        // arity mismatch never matches
        ];
        for (pats, vals, expect) in cases {
            let m = claim_matches(&pats, &vals, &base, Some(&actor));
            let u = unify_args(&pats, &vals, &base, Some(&actor));
            assert_eq!(m, u.is_some(), "verdicts disagree on {pats:?}");
            assert_eq!(m, expect, "wrong verdict on {pats:?}");
        }

        // A match extends the base with the new binding and keeps base entries.
        let u = unify_args(&[var("x")], &[s("a")], &base, Some(&actor)).unwrap();
        assert_eq!(u.get(&Var::from("x")), Some(&s("a")));
        assert_eq!(u.get(&Var::from("known")), Some(&s("k")));
    }

    // Evaluate a literal-only value expression against empty state/bindings.
    fn eval_lit(e: &ValueExpr) -> Result<EvalValue, EvalError> {
        let state = State::default();
        let bindings = Bindings::new();
        let ctx = EvalContext::new(
            &state,
            None,
            &bindings,
            None,
            crate::definitions::DefinitionTable::new(&[]),
        );
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
        let ctx = EvalContext::new(
            state,
            None,
            &bindings,
            actor,
            crate::definitions::DefinitionTable::new(&[]),
        );
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
        let ctx = EvalContext::new(
            state,
            None,
            &bindings,
            None,
            crate::definitions::DefinitionTable::new(&[]),
        );
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
        let ctx = EvalContext::new(
            &state,
            None,
            &bindings,
            None,
            crate::definitions::DefinitionTable::new(&[]),
        );
        assert_eq!(
            matching_claims(&"Price".into(), &[Term::Actor, wildcard()], &ctx),
            Err(EvalError::UnboundActor),
        );
    }
}
