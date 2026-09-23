//! IR types: the structural surface of a Morpholog programme.
//!
//! These types are pure data. State, evaluation, validation, and
//! persistence live in sibling modules.
//!
//! Bodies use two mutually recursive sorts. A [`Prop`] *searches* state
//! and yields zero, one, or many binding contexts: it is relational, not
//! boolean. A [`ValueExpr`] *computes one value* from a binding context.
//! Keeping them as separate Rust types means a value can never sit where
//! a proposition belongs. They meet where the grammar says: a comparison
//! relates two values, a sum ranges over a proposition.

use serde::{Deserialize, Serialize};

use crate::validate::{ValidationError, validate_program};

/// Defines an opaque identifier newtype over `String`, used for every
/// kernel identifier. It has `From`, `Display`, `as_str`, and a symmetric
/// `PartialEq<str>`, but no `Deref` / `AsRef` / `Borrow`, so the kinds of
/// identifier cannot be mixed up or passed around as plain strings.
macro_rules! opaque_id {
    // Default: no ordering. A `Subject`, for one, must not be orderable.
    ($(#[$meta:meta])* $name:ident) => {
        opaque_id!(@define $(#[$meta])* $name; Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize);
    };
    // `ord`: also derive `PartialOrd` / `Ord`, for ids that are sorted or used
    // as `BTreeSet` / `BTreeMap` keys.
    ($(#[$meta:meta])* ord $name:ident) => {
        opaque_id!(@define $(#[$meta])* $name; Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize);
    };
    // The shared body; only the derive set differs.
    (@define $(#[$meta:meta])* $name:ident; $($derive:path),+ $(,)?) => {
        $(#[$meta])*
        #[derive($($derive),+)]
        #[serde(transparent)]
        pub struct $name(String);

        impl $name {
            /// Borrow the underlying identifier. Use at the edges (formatting,
            /// persistence, key lookup), not to route it through string APIs.
            pub fn as_str(&self) -> &str {
                &self.0
            }
        }

        impl From<String> for $name {
            fn from(s: String) -> Self {
                Self(s)
            }
        }

        impl From<&str> for $name {
            fn from(s: &str) -> Self {
                Self(s.to_string())
            }
        }

        impl std::fmt::Display for $name {
            fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                f.write_str(&self.0)
            }
        }

        impl PartialEq<str> for $name {
            fn eq(&self, other: &str) -> bool {
                self.as_str() == other
            }
        }
        impl PartialEq<&str> for $name {
            fn eq(&self, other: &&str) -> bool {
                self.as_str() == *other
            }
        }
        impl PartialEq<$name> for str {
            fn eq(&self, other: &$name) -> bool {
                self == other.as_str()
            }
        }
        impl PartialEq<$name> for &str {
            fn eq(&self, other: &$name) -> bool {
                *self == other.as_str()
            }
        }
    };
}

opaque_id! {
    /// An opaque subject identifier, Morpholog's one primitive noun.
    /// Predicates attach to subjects, but there are no types over them and
    /// nothing inspects their structure. Not orderable, since subjects have
    /// no sequence; where the kernel must sort them it uses `as_str`.
    Subject
}

opaque_id! {
    /// A variable: bound by `forall` / `exists` / `for`, a `let`, or a
    /// `Term::Var` match, and resolved against the [`crate::EvalValue`]
    /// bindings. Ordered because the trace sorts bindings by name.
    ord Var
}

opaque_id! {
    /// The name of a claim predicate. Ordered because the analysis walkers
    /// collect predicate names into `BTreeSet`s.
    ord PredicateName
}

opaque_id! {
    /// The name of an outbox intent type.
    IntentName
}

opaque_id! {
    /// The name of a declared transformation, which a [`crate::Transition`]
    /// proposes against.
    TransformationName
}

opaque_id! {
    /// The name of a declared invariant, carried into the audit log and
    /// the trace.
    InvariantName
}

opaque_id! {
    /// The optional name an author gives a `require` or `bind`, so a
    /// refusal names the rule instead of quoting the expression. Unique
    /// within one transformation; two transformations may share one.
    RuleName
}

opaque_id! {
    /// The name of a [`Definition`], which a [`Prop::Defined`] call
    /// resolves against. Definitions and predicates share one namespace in
    /// bodies; [`Program::validate`] keeps them disjoint. Ordered for
    /// deterministic cycle detection and diagnostics.
    ord DefinitionName
}

opaque_id! {
    /// A unit symbol on a quantity: `USD`, `t`, `MWh`. A label on an exact
    /// decimal, not a physical dimension: the kernel only ensures that
    /// arithmetic and comparison combine amounts with the same label.
    /// Case-sensitive, with no registry, aliases, or compound symbols
    /// (`USD/day` is a formula, not a unit). Conversions are domain
    /// knowledge, so they belong in claims. Ordered because it is part of
    /// the ordered [`PredicateArgKind`].
    ord Unit
}

/// The typed zero an empty [`ValueExpr::Sum`] evaluates to. An empty sum
/// has no values to take a kind from, so the kind comes from the summed
/// variable's declaration, resolved at lowering. Decimal is the default
/// wherever no declaration decides.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum SumSeed {
    #[default]
    Decimal,
    Duration,
    Quantity(Unit),
}

/// A named, versioned rule that must hold over admitted state. It is
/// checked against the candidate state a [`Transformation`] produces; if
/// any invariant fails, the whole transformation is rejected.
///
/// `version` (always 1 for now) lets audit rows record exactly which
/// invariant versions governed each commit.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Invariant {
    pub name: InvariantName,
    pub version: u32,
    pub body: Prop,
    /// Whether the invariant was written in source or generated from a
    /// [`Discipline`]. Enforcement is the same; the origin lets the
    /// formatter omit generated ones and lets reports trace them back to
    /// their declaration.
    pub origin: InvariantOrigin,
    /// The predicate this invariant declares itself the totality backstop
    /// for (`total over P`): a version of `P` exists wherever one is needed.
    ///
    /// Without a version in force, a rule that selects one silently does
    /// not apply. Declaring the backstop lets the lints check the pairing
    /// instead of guessing it from the rule's shape.
    pub totality_for: Option<PredicateName>,
}

/// See [`Invariant::origin`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InvariantOrigin {
    Authored,
    Discipline,
}

/// A proposition. It *searches* a state from a binding set and yields
/// every extended binding context that satisfies it (zero, one, or
/// many): relational, not boolean. Evaluated by `find_matches`.
///
/// Used in invariant bodies, `require` / `bind`, derived-claim domains,
/// and quantifiers. Where it relates values (`Eq`, `Neq`, `Compare`), its
/// operands are [`ValueExpr`]s.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Prop {
    Claim {
        predicate: PredicateName,
        args: Vec<Term>,
    },
    /// A call to a named [`Definition`], written like a claim
    /// (`name(args)`). The body runs in a fresh context holding only the
    /// parameters: ground arguments pre-bind theirs, unbound ones are
    /// generators. Each match maps parameter values back onto the
    /// arguments. Each distinct result is yielded once, so a call inside
    /// a `Sum` never double-counts.
    Defined {
        name: DefinitionName,
        args: Vec<Term>,
    },
    Implies {
        left: Box<Prop>,
        right: Box<Prop>,
    },
    Exists {
        binding: Var,
        body: Box<Prop>,
    },
    And(Vec<Prop>),
    /// Disjunction: concatenates each branch's binding sets, all from the
    /// same base context, without deduplication. Flat, so `a or b or c`
    /// is one node.
    Or(Vec<Prop>),
    /// Evaluates the subtree against the pre-transition state instead of
    /// the candidate, so an invariant can relate before and after. Raises
    /// [`crate::EvalError::PreStateUnavailable`] where there is no
    /// pre-state (derived claims, `require`, a context with
    /// `pre_state: None`, inside another `Pre`).
    ///
    /// `pre(forall x in C: ...)` reads both domain and body from the
    /// pre-state; `forall x in C: pre(...)` iterates the post-state domain.
    /// They differ when `C` changes.
    Pre(Box<Prop>),
    Not(Box<Prop>),
    /// Exclusive or: exactly one operand holds. Evaluated as
    /// `(left or right) and not (left and right)`, with the same bindings;
    /// just a clearer spelling. Binary, because `a xor b xor c` would be
    /// ambiguous (exactly one, or odd parity), so chains nest.
    Xor(Box<Prop>, Box<Prop>),
    /// Value equality and inequality over two [`ValueExpr`] operands.
    /// Yields the bindings unchanged when it holds, nothing otherwise.
    Eq(Box<ValueExpr>, Box<ValueExpr>),
    Neq(Box<ValueExpr>, Box<ValueExpr>),
    /// Ordered comparison: an operator (`<=` `<` `>=` `>`) over an
    /// [`OrderedDomain`]. Yields the bindings unchanged when it holds,
    /// nothing otherwise.
    ///
    /// `op` is kept as written, so `amount > limit` round-trips as is.
    /// `domain` is explicit, never inferred from operand kinds: the surface
    /// picks it by token (`<` decimal, `before` date) and each domain
    /// checks its own operands. Date windows built from `<=` include both
    /// ends.
    Compare {
        op: CompareOp,
        domain: OrderedDomain,
        left: Box<ValueExpr>,
        right: Box<ValueExpr>,
    },
    Forall {
        binding: Var,
        source: Box<Prop>,
        body: Box<Prop>,
    },
    In(Term, Term),
}

/// A value expression. It *computes exactly one value* from a binding
/// context, or an error. Evaluated by `eval_value`.
///
/// Appears only nested: as a comparison operand, a `let` value, a `for`
/// collection, or a derived-claim value. Forms that range over state
/// (`Sum`, `Extremum`) take a [`Prop`] body.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ValueExpr {
    Term(Term),
    /// Binary arithmetic: `left <op> right`. Operand kinds follow the rule
    /// matrix (`arith_result_kind`):
    /// - decimals support every operator;
    /// - instants shift by durations (`Add`/`Sub`) and subtract into a
    ///   duration;
    /// - dates shift by calendar spans and subtract into a day count;
    /// - durations add, subtract, and divide into a plain ratio;
    /// - same-unit quantities add, subtract, and divide into a ratio, and
    ///   a bare decimal scales them (`Mul`/`Div`);
    /// - `Mod` is decimal-only.
    ///
    /// A pair with no rule is `NoArithRule` at validation and
    /// `TypeMismatch` at evaluation. `Div` and `Mod` raise
    /// [`crate::EvalError::DivisionByZero`] on a zero divisor; the rest
    /// are total. Admission rules stay exact by multiplying
    /// (`a <= c*b`, not `a/b <= c`); `Div` is for read-side projections.
    Arith {
        op: ArithOp,
        left: Box<ValueExpr>,
        right: Box<ValueExpr>,
    },
    /// Sums `value` over every binding the `body` produces, evaluating it
    /// once per witness: a variable (`sum(amount | ...)`), a literal that
    /// counts matches (`sum(1 | ...)`), or a computed value
    /// (`sum(probability * loss | ...)`).
    ///
    /// `seed` is what an empty sum returns, set by
    /// [`crate::lower_sum_seeds`] from declared kinds: an empty sum over a
    /// `Decimal[t]` position is `0 t`. Hand-built IR that skips lowering
    /// keeps the decimal zero, and validation refuses it
    /// (`EmptySumUntyped`) wherever the target is a duration or quantity.
    Sum {
        value: Box<ValueExpr>,
        body: Box<Prop>,
        seed: SumSeed,
    },
    /// The largest or smallest `value` over the bindings satisfying
    /// `body`. "The version in force at this date" is the greatest
    /// `effective_from` not after it.
    ///
    /// Like [`ValueExpr::Sum`] without a seed: an empty extremum has no
    /// answer, so it raises [`crate::EvalError::EmptyExtremum`]. To reject
    /// cleanly instead, put a `require` first, as with
    /// [`ValueExpr::ValueOf`] versus [`Stmt::BindOne`].
    ///
    /// Ordered kinds only: decimals, dates, timestamps, durations, and
    /// same-unit quantities. Validation refuses any other kind.
    Extremum {
        op: ExtremumOp,
        value: Term,
        body: Box<Prop>,
    },
    /// Reads exactly one matching claim and yields the argument at
    /// `extract`, which must be a wildcard in `args` (validation refuses
    /// otherwise). Zero matches is an error unless `default` is given;
    /// several matches is always an error.
    ///
    /// The positional surface form extracts the first wildcard; the named
    /// form (`value P(field: x, hole: _, ..)`) can pick any field, so the
    /// index is stored explicitly.
    ///
    /// In transformation bodies prefer [`Stmt::BindOne`], which rejects
    /// cleanly on zero matches where `ValueOf` raises a kernel error.
    /// `ValueOf` is for value positions: inside sums, arithmetic,
    /// comparisons, a `Let` value, or a `DerivedClaim` value.
    ValueOf {
        predicate: PredicateName,
        args: Vec<Term>,
        extract: usize,
        default: Option<Box<ValueExpr>>,
    },
    /// `if(when, then, otherwise)`: `then` if `when` has at least one
    /// witness, else `otherwise`. As with `require`, bindings made in
    /// `when` are discarded. Only the selected branch is evaluated. An
    /// error in `when` propagates; it never silently selects `otherwise`.
    /// The branches must have the same kind, which may be any kind.
    Cond {
        when: Box<Prop>,
        then: Box<ValueExpr>,
        otherwise: Box<ValueExpr>,
    },
    /// A strict call to a [`Builtin`]: arguments evaluated in order,
    /// then a context-free operation over the resulting values. The
    /// arity is the builtin's, checked at validation.
    Call {
        builtin: Builtin,
        args: Vec<ValueExpr>,
    },
}

impl From<Term> for ValueExpr {
    fn from(t: Term) -> Self {
        ValueExpr::Term(t)
    }
}

/// A comparison operator, independent of operand domain. Carried by
/// [`Prop::Compare`] together with an [`OrderedDomain`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CompareOp {
    Le,
    Lt,
    Ge,
    Gt,
}

/// Which end of the ordering an [`ValueExpr::Extremum`] takes.
///
/// Distinct from [`Builtin::Min`] / [`Builtin::Max`], which compare two
/// values. This picks from the set a body defines, ranging over state.
/// The surface tells them apart by the `|` that introduces a body.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExtremumOp {
    Max,
    Min,
}

impl ExtremumOp {
    /// The surface spelling, used by the formatter and diagnostics.
    pub fn as_str(self) -> &'static str {
        match self {
            ExtremumOp::Max => "max",
            ExtremumOp::Min => "min",
        }
    }
}

/// A strict function over already-evaluated values.
///
/// A [`ValueExpr`] variant decides how its children are evaluated
/// (lazily, under bindings it makes, or against a proposition). A
/// builtin is just handed finished values. Every builtin obeys this
/// contract; anything that cannot becomes a variant instead:
///
/// - every argument is a `ValueExpr`, evaluated exactly once, in order;
/// - it sees only those values - never state, bindings, actor,
///   definitions, or the AST;
/// - it binds nothing and exports nothing;
/// - its predicate footprint is exactly the union of its arguments';
/// - it yields one value or a named refusal.
///
/// Name, arity, kind inference, static refusal, and evaluation each
/// match on it exhaustively, so a new builtin must define all of them.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Builtin {
    /// The magnitude of a signed value: `abs(x)`. Keeps the unit
    /// (`abs` of a `Decimal[USD]` is a `Decimal[USD]`). Defined on
    /// decimals, quantities, and durations.
    Abs,
    /// `round(x, quantum)`: the multiple of `quantum` nearest to `x`,
    /// with exact halves rounding away from zero (2.345 to 0.01 is 2.35;
    /// -2.345 is -2.35). Bare decimals in and out. A non-positive quantum
    /// is refused at validation when literal, and otherwise raises
    /// [`crate::EvalError::RoundQuantumNotPositive`] at evaluation.
    Round,
    /// `period_index(anchor, span, at)`: which anchored period `at` falls
    /// in - the greatest integer n (as a decimal) whose nth boundary is at
    /// or before `at`. Boundary n shifts the anchor by the span times n in
    /// one clamped step, never n clamped hops, which would drift.
    /// Periods are half-open. Boundaries beyond the calendar's ends act
    /// as infinities, so the outermost periods are clipped and every date
    /// has an index; indexes before the anchor are negative. Reads no
    /// state, so a fully literal use is allowed in a `const`. A
    /// non-positive span is refused at validation when literal, and at
    /// evaluation otherwise.
    PeriodIndex,
    /// `period_start_of(anchor, span, index)`: the first day of period
    /// `index`, with the boundary computed exactly as in
    /// [`Builtin::PeriodIndex`], so
    /// `period_index(a, s, period_start_of(a, s, n)) = n` wherever the
    /// boundary exists. Where `period_index` clips, this refuses: a
    /// boundary outside the calendar raises
    /// [`crate::EvalError::ArithOutOfRange`], like `date + span` would.
    /// The index must be a whole number (a literal fraction is refused at
    /// validation, anything else at evaluation); negative indexes are
    /// before the anchor. This is not span arithmetic: `anchor + n * span`
    /// stays inexpressible. Reads no state, so a fully literal use is
    /// allowed in a `const`. Span rules are as for `period_index`.
    PeriodStartOf,
    /// `min(a, b)` / `max(a, b)`: the smaller or larger of two values.
    /// The forms over a proposition are [`ValueExpr::Extremum`].
    Min,
    Max,
}

impl Builtin {
    /// How the surface spells it, and how the formatter renders it.
    pub fn name(self) -> &'static str {
        match self {
            Builtin::Abs => "abs",
            Builtin::Round => "round",
            Builtin::PeriodIndex => "period_index",
            Builtin::PeriodStartOf => "period_start_of",
            Builtin::Min => "min",
            Builtin::Max => "max",
        }
    }

    /// How many arguments it takes.
    pub fn arity(self) -> usize {
        match self {
            Builtin::Abs => 1,
            Builtin::Round | Builtin::Min | Builtin::Max => 2,
            Builtin::PeriodIndex | Builtin::PeriodStartOf => 3,
        }
    }
}

/// A binary infix arithmetic operator, carried by [`ValueExpr::Arith`].
/// Operators written as calls (`min`, `max`) are [`Builtin`]s.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ArithOp {
    Add,
    Sub,
    Mul,
    Div,
    /// Decimal remainder (`%`). Like `Div`, a zero divisor raises
    /// [`crate::EvalError::DivisionByZero`]. For parity and cycles:
    /// `(file + rank) % 2` is a chess square's colour.
    Mod,
}

/// The ordered domain a [`Prop::Compare`] compares over. Never inferred
/// from operand kinds: the surface picks it by token (`<` decimal,
/// `before` date, `strictly_before` timestamp, `shorter_than` duration),
/// and each domain checks its own operands.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OrderedDomain {
    Decimal,
    Date,
    Timestamp,
    Duration,
}

impl OrderedDomain {
    /// Whether an operand of this kind may appear under this domain's
    /// comparators, for the static check (the evaluator has its own
    /// match). `Any` is admitted everywhere. Whether a bare decimal and a
    /// quantity may be compared with each other is decided elsewhere.
    pub fn admits(self, kind: &PredicateArgKind) -> bool {
        matches!(
            (self, kind),
            (_, PredicateArgKind::Any)
                | (
                    OrderedDomain::Decimal,
                    PredicateArgKind::Decimal | PredicateArgKind::Quantity(_)
                )
                | (OrderedDomain::Date, PredicateArgKind::Date)
                | (OrderedDomain::Timestamp, PredicateArgKind::Timestamp)
                | (OrderedDomain::Duration, PredicateArgKind::Duration)
        )
    }

    /// The domain that orders a concrete kind, for diagnostics ("this
    /// operand is a Date; use `on_or_before`"). `None` for `Any` and for
    /// kinds nothing orders (subjects, bools, collections, calendar
    /// spans).
    pub fn for_concrete_kind(kind: &PredicateArgKind) -> Option<OrderedDomain> {
        match kind {
            PredicateArgKind::Decimal | PredicateArgKind::Quantity(_) => {
                Some(OrderedDomain::Decimal)
            }
            PredicateArgKind::Date => Some(OrderedDomain::Date),
            PredicateArgKind::Timestamp => Some(OrderedDomain::Timestamp),
            PredicateArgKind::Duration => Some(OrderedDomain::Duration),
            PredicateArgKind::Subject
            | PredicateArgKind::Bool
            | PredicateArgKind::Collection
            | PredicateArgKind::CalendarSpan
            | PredicateArgKind::Any => None,
        }
    }
}

/// Given one known operand of an additive/cap operator, the kind the
/// other operand must have - if exactly one rule in the matrix fits.
/// `known_is_left` says which side the known kind sits on (the matrix
/// is not symmetric: `Timestamp + Duration` has a rule, `Duration +
/// Timestamp` does not). Returns `(expected other kind, result kind)`,
/// or `None` when zero or several rules fit and nothing can be
/// soundly assumed.
pub(crate) fn arith_unique_counterpart(
    op: ArithOp,
    known: &PredicateArgKind,
    known_is_left: bool,
) -> Option<(PredicateArgKind, PredicateArgKind)> {
    use PredicateArgKind::{CalendarSpan, Date, Decimal, Duration, Timestamp};
    // Candidates: the unit-less kinds, plus the known side's own unit if
    // it is a quantity. A unit is never inferred from nothing, so a bare
    // decimal never implies a quantity counterpart. `Date - x` fits two
    // rules (span or date), so nothing is inferred there.
    let mut candidates = vec![Decimal, Timestamp, Duration, Date, CalendarSpan];
    if let PredicateArgKind::Quantity(u) = known {
        candidates.push(PredicateArgKind::Quantity(u.clone()));
    }
    let mut fits = candidates.into_iter().filter_map(|other| {
        let (l, r) = if known_is_left {
            (known, &other)
        } else {
            (&other, known)
        };
        arith_result_kind(op, l, r).map(|result| (other.clone(), result))
    });
    match (fits.next(), fits.next()) {
        (Some(unique), None) => Some(unique),
        _ => None,
    }
}

pub(crate) fn arith_result_kind(
    op: ArithOp,
    left: &PredicateArgKind,
    right: &PredicateArgKind,
) -> Option<PredicateArgKind> {
    use PredicateArgKind::{CalendarSpan, Date, Decimal, Duration, Quantity, Timestamp};
    match (op, left, right) {
        (_, Decimal, Decimal) => Some(Decimal),
        (ArithOp::Add | ArithOp::Sub, Timestamp, Duration) => Some(Timestamp),
        (ArithOp::Sub, Timestamp, Timestamp) => Some(Duration),
        // A calendar span shifts a date (months first, clamped to the
        // month's end, then days). Two dates subtract to a signed count of
        // days, as a decimal. No `Date +/- Duration` (a date has no time
        // of day) and no `Timestamp +/- CalendarSpan` (that needs a time
        // zone).
        (ArithOp::Add | ArithOp::Sub, Date, CalendarSpan) => Some(Date),
        (ArithOp::Sub, Date, Date) => Some(Decimal),
        (ArithOp::Add | ArithOp::Sub, Duration, Duration) => Some(Duration),
        // Two spans divide into a plain decimal ("how many days").
        // Precision is covered in the evaluator.
        (ArithOp::Div, Duration, Duration) => Some(Decimal),
        // Units: amounts combine only with the same unit; two same-unit
        // amounts divide into a bare decimal; a bare decimal scales a
        // quantity. No rule creates a unit that was not written down.
        (ArithOp::Add | ArithOp::Sub, Quantity(u), Quantity(v)) if u == v => {
            Some(Quantity(u.clone()))
        }
        (ArithOp::Div, Quantity(u), Quantity(v)) if u == v => Some(Decimal),
        (ArithOp::Mul, Quantity(u), Decimal)
        | (ArithOp::Mul, Decimal, Quantity(u))
        | (ArithOp::Div, Quantity(u), Decimal) => Some(Quantity(u.clone())),
        _ => None,
    }
}

/// A positional argument in a claim, intent, or expression: a variable,
/// a wildcard, a literal, or `Actor`. `Term::Actor` resolves only in a
/// transformation body; in an invariant it raises
/// `EvalError::UnboundActor`, because authority checks belong in
/// `require`, not invariants.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Term {
    Var(Var),
    Wildcard,
    Literal(Value),
    /// Resolves to the actor of the proposed transition. Available
    /// inside transformation bodies; not available inside invariant
    /// bodies.
    Actor,
}

/// A literal constant in an IR `Term`. Unlike the runtime `EvalValue`, it
/// has no booleans or collections.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Value {
    /// Arbitrary-precision decimal, kept as its exact source string and
    /// parsed at evaluation.
    Decimal(String),
    /// A named subject constant (a purpose, status, or authority), so it
    /// need not be passed as a parameter.
    Subject(Subject),
    /// ISO-8601 civil date (`YYYY-MM-DD`), kept as its source string and
    /// parsed into [`jiff::civil::Date`] at evaluation. No time of day and
    /// no time zone.
    Date(String),
    /// An exact UTC instant (RFC 3339, e.g. `2026-10-24T14:00:00Z`), kept
    /// as its source string and parsed into [`jiff::Timestamp`] at
    /// evaluation. No time zone: local time is domain knowledge, admitted
    /// as claims.
    Timestamp(String),
    /// An exact span of time (ISO 8601, e.g. `PT6H`), kept as its source
    /// string and parsed into [`jiff::SignedDuration`] at evaluation.
    /// Exact seconds only; months and years vary in length, so they are
    /// [`Value::CalendarSpan`].
    Duration(String),
    /// A calendar span (`P3M`, `P45D`), kept as its source string and
    /// parsed via [`crate::calendar::parse_calendar_span`] at evaluation.
    /// It only shifts a `Date`: it cannot be declared as an argument kind,
    /// admitted in a claim or intent, ordered, or summed. Equality compares
    /// normalised values (`span(P1Y) = span(P12M)` holds). Separate from
    /// [`Value::Duration`] because a month has no fixed length.
    CalendarSpan(String),
    /// A unit-tagged exact decimal (`25000 USD`, `0 t`). The amount is
    /// kept as its source string, like [`Value::Decimal`]; the unit is an
    /// opaque [`Unit`].
    Quantity { amount: String, unit: Unit },
}

/// A Claim is an admitted assertion candidate - a statement that may be
/// admitted into governed state. It is not objective reality.
///
/// Distinct from `Prop::Claim`, which is a *query* over candidate state.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Claim {
    pub predicate: PredicateName,
    pub args: Vec<Term>,
}

/// An outbound effect declared by a transformation's `emit` statement.
/// Intents are *staged* during transformation execution and *enqueued*
/// to the outbox at commit time; they are never sent during the
/// transaction itself.
///
/// Distinct from [`crate::IntentInstance`], which is the resolved (no-variables)
/// form ready to be enqueued.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Intent {
    pub name: IntentName,
    pub args: Vec<Term>,
}

/// One step in a transformation body, run in order. `Require` and
/// `BindOne` can reject the transformation; the others stage changes or
/// extend the bindings. Binding rules are in `docs/runtime-semantics.md`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Stmt {
    /// A yes/no gate over the pre-state; its matches do not export.
    /// `name` is an optional stable name for refusals, which otherwise
    /// quote the expression and change with every rewording.
    Require {
        prop: Prop,
        name: Option<RuleName>,
    },
    /// Unique lookup. Evaluates the proposition against state and
    /// bindings:
    /// - zero matches: the transformation is rejected;
    /// - one match: its bindings *replace* the current ones for later
    ///   statements;
    /// - several: `EvalError::TypeMismatch`, since the state was expected
    ///   to be unique (a missing uniqueness invariant, or corruption).
    BindOne {
        prop: Prop,
        name: Option<RuleName>,
    },
    Let {
        name: Var,
        value: ValueExpr,
    },
    LetNewSubject {
        name: Var,
    },
    Assert(Claim),
    /// Retracts every pre-state claim matching the pattern. Variables in
    /// `args` must be bound; wildcards match anything. Zero matches is a
    /// no-op, not an error.
    Retract {
        predicate: PredicateName,
        args: Vec<Term>,
    },
    /// `collection` is evaluated as a value (it must yield an
    /// `EvalValue::Collection`); `binding` ranges over its items.
    For {
        binding: Var,
        collection: ValueExpr,
        body: Vec<Stmt>,
    },
    Emit(Intent),
}

/// A named, parameterised proposal to change admitted state: the only way
/// governed state changes. Run via [`crate::propose`], its [`Stmt`]s read a
/// snapshot of the pre-state, stage admissions, retractions, and intents,
/// and produce an [`crate::Outcome`] for the caller to commit or discard.
///
/// Reads always see the pre-transformation snapshot. Writes take effect
/// only at commit.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Transformation {
    pub name: TransformationName,
    pub parameters: Vec<Var>,
    pub body: Vec<Stmt>,
}

/// A named, parameterised proposition: a condition declared once and
/// called from invariants, gates, derived-claim domains, and other
/// definitions. It changes no state; it only names a [`Prop`] so rules
/// read the way the business speaks.
///
/// A [`Prop::Defined`] call pairs its `args` with `parameters` by
/// position. The body runs with only the parameters bound (ground
/// arguments pre-bind; unbound ones are generators), and each match maps
/// back onto the arguments. The body cannot see the caller's other
/// bindings, and the caller never sees the body's names.
///
/// Bodies are context-free: `Term::Actor` and `Prop::Pre` inside one are
/// validation errors, so a definition means the same in a gate as in an
/// invariant. Pass `actor` as an argument, or wrap the call in
/// `pre(...)`. Definitions may call each other; cycles are errors. A
/// parameter the body binds may arrive unbound; one it only uses must
/// arrive bound; one it never references is refused.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Definition {
    pub name: DefinitionName,
    pub parameters: Vec<Var>,
    pub body: Prop,
    /// Whether this was written or generated from a discipline. The
    /// formatter omits generated ones, and lowering uses it to tell whether
    /// it already ran; a name alone cannot tell them apart.
    pub origin: DefinitionOrigin,
}

/// Where a [`Definition`] came from - the definition-sort analogue of
/// [`InvariantOrigin`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum DefinitionOrigin {
    /// Written in the source, or hand-built by a caller.
    #[default]
    Authored,
    /// Materialised by [`crate::lower_discipline_definitions`] from a
    /// declared discipline clause.
    Discipline,
}

/// A governed domain model: vocabularies, definitions, invariants,
/// transformations, and derived claims as one unit. It holds no state,
/// connection, or schema, only the rules. A caller proposes by looking up
/// a transformation by name and passing it to [`crate::propose`] (or the
/// PostgreSQL adapter's `propose_against_pg`).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Program {
    pub name: String,
    /// The vocabulary of admissible claim shapes. Every `Prop::Claim`,
    /// `Stmt::Assert`, `Stmt::Retract`, `ValueExpr::ValueOf`, and
    /// `DerivedClaim` output must target a declared predicate (validated
    /// by [`Program::validate`]).
    pub predicates: Vec<PredicateDecl>,
    /// The outbox intents this programme may emit. Every `Stmt::Emit` must
    /// target one, so a misspelling is a validation error. Separate
    /// namespace from predicates.
    pub intents: Vec<IntentDecl>,
    /// Named propositions (see [`Definition`]). They share a namespace
    /// with `predicates`, so a name used by both is a validation error.
    pub definitions: Vec<Definition>,
    pub invariants: Vec<Invariant>,
    pub transformations: Vec<Transformation>,
    pub derived_claims: Vec<DerivedClaim>,
}

impl Program {
    /// Look up a transformation by name. Returns `None` if no
    /// transformation in the program has that name.
    pub fn transformation(&self, name: &str) -> Option<&Transformation> {
        self.transformations.iter().find(|t| t.name == name)
    }

    /// Look up an invariant by name. Returns `None` if no invariant
    /// in the program has that name.
    pub fn invariant(&self, name: &str) -> Option<&Invariant> {
        self.invariants.iter().find(|i| i.name == name)
    }

    /// Look up a derived claim by predicate name. Returns `None` if no
    /// derived claim in the program has that name.
    pub fn derived_claim(&self, name: &str) -> Option<&DerivedClaim> {
        self.derived_claims
            .iter()
            .find(|d| d.predicate.as_str() == name)
    }

    /// Look up a predicate declaration by name. Returns `None` if no
    /// declaration in the program has that name, and the first of any
    /// duplicates, which [`Program::validate`] reports as
    /// `ValidationError::DuplicateDecl`.
    pub fn predicate(&self, name: &str) -> Option<&PredicateDecl> {
        self.predicates.iter().find(|p| p.name.as_str() == name)
    }

    /// Look up an intent declaration by name. Returns `None` if no
    /// intent in the program has that name. Same duplicate-handling
    /// semantics as [`Self::predicate`].
    pub fn intent(&self, name: &str) -> Option<&IntentDecl> {
        self.intents.iter().find(|i| i.name.as_str() == name)
    }

    /// Full static validation of the programme:
    ///
    /// - **Structural**: every predicate and intent reference targets a
    ///   declaration at the declared arity; no duplicate names within a
    ///   vocabulary.
    /// - **Kinds**: every value in a slot, comparator, or arithmetic
    ///   operand has a compatible kind; a variable's uses must agree;
    ///   `Any` is unconstrained, not a kind-eraser.
    /// - **Binding flow**: a name must be bound before it is used, under
    ///   the runtime's export rules.
    /// - **Actor context**: no `Term::Actor` in an invariant or
    ///   derived-claim body.
    /// - **Nesting depth**: expressions and `for` statements may not nest
    ///   so deep that evaluation could overflow the stack.
    /// - **Derived claims**: no rule, including another derived claim's
    ///   domain, may read a derived claim; nothing admits one.
    ///
    /// Returns every error found, not just the first.
    ///
    /// `propose` does **not** call this; `morpholog check` does, and so
    /// do the tests over the worked examples.
    pub fn validate(&self) -> Result<(), Vec<ValidationError>> {
        validate_program(self)
    }

    /// Validate and return a proof-of-validity handle. Same checks and
    /// errors as [`Self::validate`]; on success, a
    /// [`crate::ValidatedProgram`] for the analysis API
    /// ([`crate::transformation_param_kinds`],
    /// [`crate::transformation_arg_schema`]), so validation runs once.
    pub fn validated(&self) -> Result<crate::ValidatedProgram<'_>, Vec<ValidationError>> {
        self.validate()
            .map(|()| crate::ValidatedProgram::from_validated(self))
    }
}

/// A predicate declaration: its name and its named, kinded arguments.
///
/// Matching is positional. Argument *names* serve the named surface
/// forms and `morpholog inspect predicates`. Argument *kinds* (see
/// [`PredicateArgKind`]) are checked by [`Program::validate`] against
/// every value reaching that position.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PredicateDecl {
    pub name: PredicateName,
    pub args: Vec<ArgDecl>,
    /// Declared claim disciplines (see [`Discipline`]). Serialised only
    /// when present, so output for programmes without any is unchanged.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub disciplines: Vec<Discipline>,
}

/// A declared property of a claim shape, enforced by lowering to ordinary
/// generated invariants or definitions (see `lower_disciplines`) or by a
/// static check. Properties of claim shapes only, never a way to write
/// arbitrary rule templates.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "discipline", rename_all = "snake_case")]
pub enum Discipline {
    /// `unique by (fields)`: two claims agreeing on these fields agree on
    /// every field. One generated invariant per clause; several clauses
    /// may coexist.
    UniqueBy { fields: Vec<String> },
    /// `effective by (keys) on (date_field)`: effective-dated, with one
    /// version per key per date; the version in force at a moment is the
    /// latest whose date is not after it.
    ///
    /// Lowers to a selector definition the author calls, plus a uniqueness
    /// invariant. It composes with `current pointer by`, which governs
    /// corrections within a version.
    EffectiveBy {
        keys: Vec<String>,
        on: String,
        /// `partial`: gaps in coverage are intended, so no hint is given
        /// for a missing totality backstop. Also declaring `total over`
        /// this predicate is an error.
        partial: bool,
    },
    /// `append only`: no transformation may `retract` this predicate.
    /// Checked statically, since only a `retract` statement retracts.
    /// Corrections go through supersession or exception claims.
    AppendOnly,
    /// `current pointer by (fields)`: a retractable current pointer.
    /// Lowers exactly like `unique by (fields)` and records the class.
    CurrentPointerBy { fields: Vec<String> },
    /// `superseded via L`: names the lineage predicate `L(successor,
    /// prior)` recording this pointer's history. Lowers to **no-fork
    /// only** (`unique by` the prior on `L`: one prior, at most one
    /// successor) and makes `L` append-only. Joins and cycles are not
    /// prevented. Requires `current pointer by` on the same predicate.
    SupersededVia { lineage: PredicateName },
}

/// One argument declaration, used by [`PredicateDecl`] and
/// [`IntentDecl`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ArgDecl {
    pub name: String,
    pub kind: PredicateArgKind,
}

/// An outbox intent declaration: its name and its argument list. Shaped
/// like [`PredicateDecl`] but a separate vocabulary: predicates describe
/// claims, intents describe outbox effects. `emit` arguments are checked
/// against it as `admit` arguments are against predicates.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct IntentDecl {
    pub name: IntentName,
    pub args: Vec<ArgDecl>,
}

/// The expected kind of a predicate argument position.
///
/// A declared expectation, separate from the IR literal [`Value`] and the
/// runtime [`crate::EvalValue`].
///
/// `Any` is for positions whose kind is genuinely open. Use it sparingly;
/// specific kinds catch more mistakes.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub enum PredicateArgKind {
    Subject,
    Decimal,
    Date,
    Timestamp,
    Duration,
    Bool,
    Collection,
    /// A unit-tagged exact decimal, declared `Decimal[USD]`. Two
    /// quantity kinds are compatible only when their units are equal.
    Quantity(Unit),
    /// The kind of a `span(P3M)` literal. It cannot be declared, so no
    /// claim, intent, or argument carries one; it exists for kind
    /// inference inside date arithmetic.
    CalendarSpan,
    Any,
}

/// Renders the declaration syntax (`Decimal[USD]`, never "Quantity"), so
/// every diagnostic names the unit. Shared by the formatter and the
/// validation errors.
impl std::fmt::Display for PredicateArgKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            PredicateArgKind::Subject => write!(f, "Subject"),
            PredicateArgKind::Decimal => write!(f, "Decimal"),
            PredicateArgKind::Date => write!(f, "Date"),
            PredicateArgKind::Timestamp => write!(f, "Timestamp"),
            PredicateArgKind::Duration => write!(f, "Duration"),
            PredicateArgKind::Bool => write!(f, "Bool"),
            PredicateArgKind::Collection => write!(f, "Collection"),
            PredicateArgKind::Quantity(u) => write!(f, "Decimal[{u}]"),
            PredicateArgKind::CalendarSpan => write!(f, "CalendarSpan"),
            PredicateArgKind::Any => write!(f, "Any"),
        }
    }
}
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DerivedClaim {
    pub predicate: PredicateName,
    pub keys: Vec<Var>,
    pub values: Vec<DerivedValue>,
    pub domain: Prop,
}

/// One computed value in a [`DerivedClaim`]. `name` is descriptive
/// only: the output [`crate::ClaimInstance`] is positional, keys then
/// values in declaration order. `expr` runs once per distinct key tuple.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DerivedValue {
    pub name: String,
    pub expr: ValueExpr,
}
