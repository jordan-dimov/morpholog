//! IR types: the structural surface of a Morpholog programme.
//!
//! `Invariant`, `Prop`, `ValueExpr`, `Term`, `Value`, `Claim`, `Intent`,
//! `Stmt`, `Transformation`, `Program`, `DerivedClaim`, `DerivedValue`,
//! plus the predicate-declaration types `PredicateDecl`, `ArgDecl`,
//! `PredicateArgKind`. These are pure data; runtime concerns (state,
//! evaluation, proposal execution, validation, persistence) live in
//! sibling modules.
//!
//! The body grammar of invariants and transformations is two mutually
//! recursive sorts, not one. A [`Prop`] *searches* governed state and
//! produces binding witnesses (zero, one, or many satisfying binding
//! contexts) - it is relational, not boolean. A [`ValueExpr`] *computes
//! one value* from a binding context. The split makes the
//! predicate-vs-value boundary a Rust type instead of a runtime error
//! plus a static shape check: the evaluator for each sort is total, with
//! no wrong-shape arm. The cross-references between the sorts encode the
//! grammar - a comparison relates two values, a sum ranges over a
//! proposition.

use serde::{Deserialize, Serialize};

use crate::validate::{ValidationError, validate_program};

/// Defines an opaque identifier newtype over `String`. Every kernel identifier
/// kind (subjects, variables, predicate / intent / transformation / invariant
/// names) is one of these: `#[serde(transparent)]`, `From<String>` /
/// `From<&str>` / `Display` / `as_str`, and a symmetric `PartialEq<str>` so a
/// literal compares either way - but deliberately no `Deref` / `AsRef` /
/// `Borrow`, so the inner string never leaks into general string APIs and the
/// kinds stay un-confusable at the type level.
macro_rules! opaque_id {
    // Default: no ordering. `Ord` is a capability some ids never need, so it
    // is opt-in rather than uniform - a `Subject` is not orderable.
    ($(#[$meta:meta])* $name:ident) => {
        opaque_id!(@define $(#[$meta])* $name; Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize);
    };
    // `ord`: also derive `PartialOrd` / `Ord`, for ids that are sorted or used
    // as `BTreeSet` / `BTreeMap` keys (the per-id doc says why it is load-bearing).
    ($(#[$meta:meta])* ord $name:ident) => {
        opaque_id!(@define $(#[$meta])* $name; Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize);
    };
    // The shared body: the derive set is the only thing that varies between the
    // two public forms, so everything below is written once.
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
    /// An opaque subject identifier - Morpholog's one primitive noun. Predicates
    /// attach to subjects, but there are no types *over* a subject and nothing in
    /// the surface language inspects its structure, so the newtype keeps a subject
    /// distinct at the type level from a predicate name, a variable, or any other
    /// string the kernel handles. It is deliberately not orderable: sorting
    /// subjects would imply a sequence semantics the surface language never
    /// gives them. Where the kernel must order subjects it does so explicitly
    /// through `as_str`, not through the type.
    Subject
}

opaque_id! {
    /// A bound variable - a name introduced by a comprehension binder
    /// (`forall` / `exists` / `for`), a `let`, or matched by a `Term::Var`, and
    /// resolved against the [`crate::EvalValue`] bindings during evaluation.
    /// Bindings are reported sorted by variable name in the trace, so the
    /// derived `Ord` is load-bearing here (not merely uniform with the others).
    ord Var
}

opaque_id! {
    /// An opaque predicate name - the identifier of a claim predicate. Distinct
    /// at the type level from a subject id, a bound variable, an intent name, or
    /// a declaration name, so the compiler keeps the kernel's nouns un-confusable.
    /// Ordered: the analysis walkers collect predicate names into `BTreeSet`s.
    ord PredicateName
}

opaque_id! {
    /// An opaque intent name - the identifier of an outbox intent type. Distinct
    /// at the type level from a predicate name (the other declared vocabulary) and
    /// from every other identifier the kernel handles.
    IntentName
}

opaque_id! {
    /// An opaque transformation name - the identifier of a declared transformation,
    /// and the name a [`crate::Transition`] proposes against. Distinct at the type
    /// level from an invariant name and every other identifier.
    TransformationName
}

opaque_id! {
    /// An opaque invariant name - the identifier of a declared invariant, carried
    /// into the audit log and the trace's invariant-check entries. Distinct at the
    /// type level from a transformation name and every other identifier.
    InvariantName
}

opaque_id! {
    /// An opaque rule name - the optional identifier an author gives a `require`
    /// gate or a `bind` lookup, so a refusal names the rule rather than quoting
    /// the expression that failed. Unique within one transformation, not across
    /// the programme: two acts legitimately carry the same gate verbatim.
    RuleName
}

opaque_id! {
    /// An opaque definition name - the identifier of a declared
    /// [`Definition`] (a named, parameterised proposition), and the name a
    /// [`Prop::Defined`] call resolves against. Distinct at the type level
    /// from a predicate name, although the two share the reference
    /// namespace in body position (a claim-shaped reference resolves to
    /// exactly one of them; [`Program::validate`] enforces the
    /// disjointness). Ordered: cycle detection and diagnostics sort
    /// definition names for deterministic output.
    ord DefinitionName
}

opaque_id! {
    /// An opaque unit symbol on a quantity - `USD`, `t`, `MWh`. A unit in
    /// Morpholog is a contractual label on an exact decimal, not a physical
    /// dimension: the kernel enforces that arithmetic and comparison only
    /// combine like-labelled amounts, and knows nothing else. Case-sensitive,
    /// no registry, no aliases, no compound symbols (`USD/day` is a business
    /// concept expressed in a predicate's field name and formula, never a
    /// unit). Conversions between units are domain knowledge with provenance and
    /// time, so they enter as claims when a worked example forces them - the
    /// same doctrine that keeps timezone interpretation out of the runtime.
    /// Ordered because [`PredicateArgKind`] is ordered (the analysis walkers
    /// collect kind sets into `BTreeSet`s) and the unit is part of the kind.
    ord Unit
}

/// The typed zero an empty [`ValueExpr::Sum`] evaluates to. A sum's
/// runtime kind is driven by its values, but the empty sum has none, so
/// the kind comes from the summed variable's declaration instead -
/// resolved once, at lowering, never during evaluation. Decimal is the
/// default and the fallback wherever no declaration decides (a count
/// sum's literal target, a pre-bound variable).
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum SumSeed {
    #[default]
    Decimal,
    Duration,
    Quantity(Unit),
}

/// A named, versioned rule that must hold over admitted state. Invariants
/// are evaluated against the candidate state produced by a
/// [`Transformation`]; if any active invariant fails, the transformation is
/// rejected atomically.
///
/// The `version` field is carried from day one (v0 is `version: 1`
/// everywhere) so that audit rows can record exactly which invariant
/// version-set governed each committed transition. Adding versioning later
/// would be painful; the empty cost of carrying it now is cheap.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Invariant {
    pub name: InvariantName,
    pub version: u32,
    pub body: Prop,
    /// Whether the invariant was authored in source or generated by a
    /// declared [`Discipline`]'s lowering. Enforcement is identical -
    /// the origin exists so the formatter can omit generated invariants
    /// (the declaration clauses imply them, and reparsing regenerates
    /// them deterministically) and so the legibility surfaces can trace
    /// a generated rule back to its declaration.
    pub origin: InvariantOrigin,
    /// The predicate this invariant declares itself the totality backstop
    /// for: "whatever else I say, a version of `P` exists where one is
    /// needed."
    ///
    /// An effective-dated selection passes vacuously when no version is in
    /// force, so the rule silently stops applying at the edges. The
    /// governing-selection lint has always looked for a backstop by SHAPE;
    /// this lets the author say so, which makes the pairing checked rather
    /// than guessed - an unusual but intended backstop is recognised, and
    /// a shape that matches by accident no longer counts as one.
    pub totality_for: Option<PredicateName>,
}

/// See [`Invariant::origin`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InvariantOrigin {
    Authored,
    Discipline,
}

/// A proposition: the predicate-shaped sort of the body grammar. A
/// `Prop` *searches* a state and a binding set, producing the set of
/// extended binding contexts that satisfy it (zero, one, or many) - it
/// is relational, not boolean. Evaluated by `find_matches`.
///
/// Used inside invariant bodies, transformation `require`/`bind`
/// statements, derived-claim domains, and quantifier composition. The
/// variants are deliberately narrow - composition, claim and
/// (in)equality matching, ordered comparison, and bounded quantification.
/// Where a `Prop` relates values (`Eq`, `Neq`, `Compare`), its operands
/// are [`ValueExpr`]s; the two sorts are mutually recursive.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Prop {
    Claim {
        predicate: PredicateName,
        args: Vec<Term>,
    },
    /// A call to a named [`Definition`]: claim-shaped on the surface
    /// (`name(args)`), resolved against the programme's definitions.
    /// Relational substitution with projection: the body evaluates under
    /// a fresh context carrying only the parameters (ground arguments
    /// pre-bind theirs; unbound ones act as generators), and each body
    /// match projects parameter values back onto the argument terms.
    /// Yields each distinct argument-binding witness once - internal
    /// multiplicity is not observable, so a call composes in `Sum`
    /// bodies without internal witnesses double-counting.
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
    /// Predicate-shaped disjunction. Concatenates the binding sets each
    /// branch produces against the same base context; empty when every
    /// branch is empty. No deduplication (matches `And`'s convention).
    /// Flattened `Vec<Prop>` so `a or b or c` is one node.
    Or(Vec<Prop>),
    /// Evaluates the wrapped subtree against the pre-transition state
    /// instead of the candidate (post) state, so one invariant can
    /// relate pre and post values. Raises
    /// [`crate::EvalError::PreStateUnavailable`] where no pre-state is in
    /// scope (derived-claim bodies, transformation `require`s, a context
    /// built with `pre_state: None`, the inner of a nested `Pre`).
    ///
    /// Quantifier composition is non-commutative: `pre(forall x in C:
    /// ...)` resolves both the domain and the body against pre, while
    /// `forall x in C: pre(...)` iterates the post-state domain and flips
    /// only the body - they diverge when the iteration set changes.
    Pre(Box<Prop>),
    Not(Box<Prop>),
    /// Exclusive or: exactly one of the two operands holds. Defined as,
    /// and evaluated by lowering to, `(left or right) and not (left and
    /// right)` - so it is purely a more legible spelling of that
    /// combination, with identical binding semantics, not new
    /// expressiveness. Binary, not flattened: `a xor b xor c` would be
    /// ambiguous (exactly-one versus odd-parity), so chained `xor` nests
    /// rather than forming one node. Earns its place where the operands
    /// are long claim patterns and the hand-written form reads poorly.
    Xor(Box<Prop>, Box<Prop>),
    /// Value equality and inequality. Both operate on [`ValueExpr`]
    /// operands (a bare `Term`, arithmetic, `Sum`, or `ValueOf`),
    /// evaluated to a value and compared. Predicate-shaped: the unchanged
    /// binding set when the (in)equality holds, empty otherwise. `Eq` and
    /// `Neq` are symmetric - neither restricts its operands to bare terms.
    Eq(Box<ValueExpr>, Box<ValueExpr>),
    Neq(Box<ValueExpr>, Box<ValueExpr>),
    /// Ordered comparison: an operator (`<=` `<` `>=` `>`) over an ordered
    /// domain (decimal or civil date). Predicate-shaped - the unchanged
    /// binding set when the comparison holds, empty otherwise.
    ///
    /// `op` is first-class so the comparison renders and round-trips as
    /// written: `amount > limit` stays `amount > limit`, never `not (amount
    /// <= limit)`. `domain` is carried explicitly rather than inferred from
    /// operand kind, so there is no operator overloading by operand kind -
    /// the surface picks the domain by token (`<` decimal, `before` date)
    /// and each domain type-checks its own operands (`EvalValue::Decimal` /
    /// `EvalValue::Date`). Date windows built from `<=` are inclusive at
    /// both ends: `to == d` admits.
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

/// A value expression: the value-producing sort of the body grammar. A
/// `ValueExpr` *computes exactly one value* from a binding context (or a
/// structural error). Evaluated by `eval_value`.
///
/// Appears only nested: as a comparator or (in)equality operand, a `let`
/// value, a `sum` target's enclosing arithmetic, a `for` collection, or a
/// derived-claim value expression. Where a `ValueExpr` ranges over a
/// proposition (`Sum`), its body is a [`Prop`]; the two sorts are
/// mutually recursive.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ValueExpr {
    Term(Term),
    /// Binary arithmetic: `left <op> right`. The operator is the
    /// [`ArithOp`] field rather than a variant per operator - the
    /// value-sort analogue of [`Prop::Compare`] carrying a
    /// [`CompareOp`]. Operand kinds follow the rule matrix
    /// (`arith_result_kind`): decimals support every operator;
    /// instants shift by durations (`Add`/`Sub`) and difference into
    /// durations (`Sub`); durations add, subtract, cap (`Min`/`Max`),
    /// and divide into a dimensionless ratio; same-unit quantities
    /// add, subtract, cap, and ratio, with a bare decimal scaling
    /// them (`Mul`/`Div`); `Mod` stays decimal-only. A pair with
    /// no rule is `NoArithRule` at validation and `TypeMismatch` at
    /// evaluation. `Div` and `Mod` surface
    /// [`crate::EvalError::DivisionByZero`] on a zero divisor; the
    /// rest are total. Admission gates express ratio rules in the
    /// multiplied form (`a <= c*b`, not `a/b <= c`) to stay exact;
    /// `Div` is reserved for read-side projections.
    Arith {
        op: ArithOp,
        left: Box<ValueExpr>,
        right: Box<ValueExpr>,
    },
    /// Sums `value` over every binding the `body` produces. `value` is
    /// usually a variable bound by the body (`sum(amount | ...)`); a
    /// decimal-literal `value` turns the sum into a count of matches
    /// (`sum(1 | ...)`).
    ///
    /// `seed` is the zero an empty sum evaluates to, resolved statically
    /// by [`crate::lower_sum_seeds`] from the summed variable's declared
    /// kind - so an empty sum over a `Decimal[t]` position is `0 t`, not
    /// a bare decimal that no quantity comparison could accept. Un-lowered
    /// hand-built IR keeps the decimal default.
    Sum {
        value: Term,
        body: Box<Prop>,
        seed: SumSeed,
    },
    /// The largest or smallest `value` over the bindings satisfying
    /// `body` - the selection a governing-claim rule needs on the commit
    /// path ("the version in force at this date" is the greatest
    /// `effective_from` not after it).
    ///
    /// Shaped like [`ValueExpr::Sum`] without a seed, because that is the
    /// whole difference: an empty sum is a typed zero, and an empty
    /// extremum has no answer to give. It raises
    /// [`crate::EvalError::EmptyExtremum`] rather than inventing one, so
    /// an author who wants a lawful refusal writes a `require` first -
    /// the same division of labour as [`ValueExpr::ValueOf`] (errors)
    /// against [`Stmt::BindOne`] (rejects).
    ///
    /// Ordered kinds only - decimals, dates, timestamps, durations, and
    /// same-unit quantities. Subjects are opaque identifiers, booleans are
    /// not a scale, and a collection is not a point on one, so none has a
    /// largest member; all are refused at validation rather than given an
    /// arbitrary order. The check is an allow-list, so a kind added later
    /// has no order until someone decides it does.
    Extremum {
        op: ExtremumOp,
        value: Term,
        body: Box<Prop>,
    },
    /// Reads exactly one matching claim and yields its value-position
    /// binding; wildcards in `args` mark the value position(s). Zero
    /// matches errors unless `default` is supplied; multiple matches
    /// always errors.
    ///
    /// Prefer [`Stmt::BindOne`] in transformation bodies (it rejects
    /// lawfully on zero matches, where `ValueOf` raises a kernel error).
    /// `ValueOf` is for value positions that are not statement-level
    /// binding extensions: inside `Sum`/`Add`/`Sub`/`Eq`/`Compare`,
    /// a `Let` value, or a `DerivedClaim` value expression.
    ValueOf {
        predicate: PredicateName,
        args: Vec<Term>,
        default: Option<Box<ValueExpr>>,
    },
    /// The magnitude of a signed value: `abs(x)`. Unary and
    /// unit-preserving (`abs` of a `Decimal[USD]` is a `Decimal[USD]`),
    /// defined on decimals, quantities, and durations. A dedicated node,
    /// not `max(x, 0 - x)`, so the operand is evaluated once and the form
    /// round-trips as `abs`.

    /// `round(x, quantum)`: the multiple of `quantum` nearest to `x`,
    /// exact halves rounding AWAY FROM ZERO (2.345 to a 0.01 quantum is
    /// 2.35; -2.345 is -2.35). One mode only - a second rounding policy
    /// joins as a parameter when a real domain forces it, not before.
    /// Decimal-only in v0: both operands are bare decimals and the
    /// result is a bare decimal (money convention: currency lives in
    /// field names). A non-positive quantum is refused by name at
    /// validation when written literally and raises
    /// [`crate::EvalError::RoundQuantumNotPositive`] at evaluation
    /// otherwise. A dedicated node for the `abs` reasons: the operand
    /// evaluates once, the form round-trips as `round`, and the
    /// sign-branched shift-and-remainder spelling it replaces can never
    /// be mistaken for user arithmetic.

    /// `if(when, then, otherwise)`: the value selected by whether a
    /// proposition holds. The test is exists-style - at least one
    /// witness selects `then`, none selects `otherwise` - and the
    /// witnesses' bindings are DISCARDED, the same non-export rule
    /// `require` carries: nothing bound inside `when` reaches the
    /// branches or the surrounding expression. Only the selected
    /// branch evaluates (an error in the untaken branch cannot
    /// surface), while an error in the condition itself propagates -
    /// a condition that cannot be decided never silently selects
    /// `otherwise`. Branch kinds unify with no ordering requirement:
    /// selection is not ordering, so subject tags, booleans, and
    /// collections are lawful branch kinds. A kernel node, not sugar:
    /// no existing `ValueExpr` selects, and the relational spelling
    /// (an `or` of tests) can only TEST an already-bound value, never
    /// produce one.
    Cond {
        when: Box<Prop>,
        then: Box<ValueExpr>,
        otherwise: Box<ValueExpr>,
    },
    /// `period_index(anchor, span, at)`: which anniversary-anchored
    /// period `at` falls in - the greatest integer n (as an
    /// integer-valued decimal) whose nth boundary is at or before
    /// `at`. Boundary n is the anchor shifted by the span's
    /// components multiplied by n ONCE and applied with the standard
    /// clamped walk - never n repeated clamped hops, which the
    /// calendar's non-associativity would let drift. Representable
    /// boundaries form half-open periods; a boundary beyond either
    /// end of the representable calendar acts as an infinity, so the
    /// outermost periods are clipped and the extractor is total,
    /// with negative indexes before the anchor. The operator itself
    /// reads no state (its children are ordinary value expressions,
    /// walked as such), so a fully-literal use is lawful in a
    /// `const`. A non-positive span is refused by name at validation
    /// when written literally and at evaluation otherwise (the round
    /// quantum pattern).
    /// A strict call to a [`Builtin`]: arguments evaluated in order,
    /// then a context-free operation over the resulting values. The
    /// arity is the builtin's, checked at validation.
    Call {
        builtin: Builtin,
        args: Vec<ValueExpr>,
    },
}

/// A comparison operator, independent of operand domain. Carried by
/// [`Prop::Compare`] together with an [`OrderedDomain`]; the pair replaces
/// what were once eight flat comparator variants (`Le` through `DateGt`) -
/// the operator stays first-class without the enum exploding by kind.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CompareOp {
    Le,
    Lt,
    Ge,
    Gt,
}

/// Which end of the ordering an [`ValueExpr::Extremum`] takes.
///
/// Distinct from [`Builtin::Min`] / [`Builtin::Max`], which cap one value
/// against another and are strict calls. This picks from a set the body
/// defines - it binds a variable and ranges over state, which is what
/// makes it a construct rather than a builtin. The two never appear in
/// the same position, and the surface tells them apart by the `|` that
/// introduces a body.
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
/// The line against a [`ValueExpr`] variant is evaluation topology: a
/// CONSTRUCT decides how its children are evaluated - lazily, or
/// repeatedly under bindings it produces, or against a proposition -
/// while a BUILTIN is handed finished values and returns one. Every
/// builtin obeys the same contract, and a candidate that cannot is
/// promoted to a variant instead:
///
/// - every argument is a `ValueExpr`, evaluated exactly once, in order;
/// - it sees only those values - never state, bindings, actor,
///   definitions, or the AST;
/// - it binds nothing and exports nothing;
/// - its predicate footprint is exactly the union of its arguments';
/// - it yields one value or a named refusal.
///
/// Kept closed, and every semantic authority matches it exhaustively -
/// surface name, arity, kind inference, static refusal, evaluation -
/// so a new builtin still has to declare its behaviour to the
/// compiler. What it no longer does is redden a dozen walkers whose
/// only answer was "recurse through the arguments".
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Builtin {
    /// `abs(x)`: magnitude, preserving the operand's kind.
    Abs,
    /// `round(x, quantum)`: the multiple of `quantum` nearest `x`,
    /// exact halves away from zero.
    Round,
    /// `period_index(anchor, span, at)`: which anniversary-anchored
    /// period `at` falls in.
    PeriodIndex,
    /// `min(a, b)` / `max(a, b)`: the smaller or larger of two values.
    /// Spelled like the calls they are - the aggregate forms over a
    /// proposition are [`ValueExpr::Extremum`], a construct.
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
            Builtin::Min => "min",
            Builtin::Max => "max",
        }
    }

    /// How many arguments it takes.
    pub fn arity(self) -> usize {
        match self {
            Builtin::Abs => 1,
            Builtin::Round | Builtin::Min | Builtin::Max => 2,
            Builtin::PeriodIndex => 3,
        }
    }
}

/// A binary decimal arithmetic operator. Carried by [`ValueExpr::Arith`];
/// the value-sort analogue of [`CompareOp`], replacing what would be a flat
/// variant per operator. A new operator is one row here, not a fresh
/// `ValueExpr` variant rippled across every match.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
/// Infix only, by construction: `min`/`max` read as calls and live in
/// [`Builtin`], so nothing here needs an is-it-infix predicate.
pub enum ArithOp {
    Add,
    Sub,
    Mul,
    Div,
    /// Decimal remainder (`%`). Like `Div`, a zero divisor surfaces
    /// [`crate::EvalError::DivisionByZero`]. Expresses parity and cyclic
    /// rules - `(file + rank) % 2` for a chess square's colour.
    Mod,
}

impl ArithOp {}

/// The ordered domain an [`Prop::Compare`] compares over. Explicit in the
/// IR, never inferred from operand kind: the surface picks it by token (`<`
/// decimal, `before` date, `strictly_before` timestamp, `shorter_than`
/// duration), so there is no runtime operator overloading and each domain
/// type-checks its own operands.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OrderedDomain {
    Decimal,
    Date,
    Timestamp,
    Duration,
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
    // Candidate counterparts: the unit-less arithmetic kinds, plus the
    // known side's own unit when the known side is a quantity. A unit
    // the expression has not already named cannot be INFERRED - only
    // declared - so a bare-decimal known side never infers a quantity
    // counterpart, even though the scaling rule would evaluate one.
    // Note `Sub` with a known left-hand `Date` fits two rules (a span
    // counterpart yields a date, a date counterpart yields days), so
    // nothing is inferred there; both-sides-known checking still runs.
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
        // The civil-date rules. A calendar span shifts a date (months
        // first, day clamped to the destination month, then days); the
        // difference of two dates is their signed count of actual days,
        // as a decimal. Deliberately absent: `Date +/- Duration` (exact
        // seconds cannot shift a day-less-precise value), and
        // `Timestamp +/- CalendarSpan` (a calendar shift of an instant
        // needs a time zone, which the kernel refuses to guess).
        (ArithOp::Add | ArithOp::Sub, Date, CalendarSpan) => Some(Date),
        (ArithOp::Sub, Date, Date) => Some(Decimal),
        (ArithOp::Add | ArithOp::Sub, Duration, Duration) => Some(Duration),
        // The ratio of two spans is a dimensionless decimal - how many
        // days of demurrage, how many turn-times in the gap. Exact for
        // terminating ratios; see the evaluator's arm for the precision
        // contract.
        (ArithOp::Div, Duration, Duration) => Some(Decimal),
        // The unit algebra, deliberately minimal: amounts combine only
        // under the SAME label; the ratio of two same-unit amounts is
        // a bare decimal; a bare decimal scales a quantity. Nothing
        // here produces a unit that was not already written down - no
        // compound units, no unit-producing multiplication.
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

/// A positional argument in a claim, intent, or expression: a variable
/// bound by the surrounding context, a wildcard matching anything, a
/// literal constant, or `Actor`. `Term::Actor` resolves only inside a
/// transformation body; in an invariant it surfaces as
/// `EvalError::UnboundActor` - the require-vs-invariant doctrine made
/// enforceable: authority checks belong in `require`, not invariants.
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

/// Literal constants embeddable in IR `Term`s. Distinct from `EvalValue`
/// (a runtime value, including the booleans and collections that cannot
/// appear as IR literals).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Value {
    /// Arbitrary-precision decimal stored as its exact source string.
    /// Parsing into a numeric type is the evaluator's concern, not the IR's.
    Decimal(String),
    /// Opaque subject identifier embedded as a literal in the IR.
    /// Lets predicates and requires reference named constants
    /// (purposes, statuses, named authorities, etc.) without forcing
    /// every transformation to take them as extra parameters.
    Subject(Subject),
    /// ISO-8601 civil date (`YYYY-MM-DD`) stored as its exact source string.
    /// Parsing into [`jiff::civil::Date`] is the evaluator's concern, not the
    /// IR's; mirrors how [`Value::Decimal`] defers parsing to evaluation.
    /// No time-of-day, no time zone: the civil-date kind for
    /// validity-window modelling, beside the exact-instant
    /// [`Value::Timestamp`] and exact-span [`Value::Duration`] kinds.
    Date(String),
    /// An exact instant on the UTC timeline (RFC 3339, e.g.
    /// `2026-10-24T14:00:00Z`), stored as its exact source string;
    /// parsing into [`jiff::Timestamp`] is the evaluator's concern.
    /// Deliberately zone-less: civil-time interpretation (port-local
    /// days, DST boundaries) is domain knowledge to be admitted as
    /// claims, not a hidden runtime assumption.
    Timestamp(String),
    /// An exact span of time (ISO 8601, e.g. `PT6H`), stored as its
    /// exact source string; parsing into [`jiff::SignedDuration`] is
    /// the evaluator's concern. Exact seconds only - no calendar
    /// units (months, years), whose lengths depend on context the
    /// kernel refuses to guess. Calendar shifts are the separate
    /// [`Value::CalendarSpan`], which only date arithmetic accepts.
    Duration(String),
    /// A calendar span (`P3M`, `P45D`), stored as its exact source
    /// string; parsing via [`crate::calendar::parse_calendar_span`] is
    /// the evaluator's concern. An arithmetic operand only - it shifts
    /// a `Date` and is refused everywhere else: not declarable as an
    /// argument kind, not admissible into a claim or intent, never
    /// ordered or summed. Equality over the normalised value is
    /// lawful (`span(P1Y) = span(P12M)` holds). Kept apart from
    /// [`Value::Duration`] because a month has no exact length until
    /// it lands on a date.
    CalendarSpan(String),
    /// A unit-tagged exact decimal quantity (`25000 USD`, `0 t`). The
    /// amount is stored as its exact source string, like
    /// [`Value::Decimal`]; the unit is an opaque [`Unit`] symbol. The
    /// evaluator enforces same-unit arithmetic and comparison; the
    /// kernel holds no unit knowledge beyond label equality.
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

/// One step inside a transformation body, run in declared order against
/// a binding context. `Require` and `BindOne` can short-circuit the
/// transformation; `Assert`, `Retract`, `Emit`, `Let`, `LetNewSubject`,
/// and `For` extend the staged outcome or the bindings. The
/// require/bind_one/let/for binding quartet is documented per variant
/// below and in full in `docs/runtime-semantics.md`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Stmt {
    /// A yes/no gate over the pre-state. `name` is the author's optional
    /// stable identifier: a refusal quotes the rendered expression, which
    /// any rewording changes, so anything that must name the rule that
    /// refused - a test, a runbook, refusals grouped by cause - needs
    /// something the prose cannot invalidate.
    Require {
        prop: Prop,
        name: Option<RuleName>,
    },
    /// Deterministic unique-lookup binding statement. Evaluates a
    /// predicate-shaped proposition against current state and bindings:
    /// - Zero matches: transformation rejected (lawful: the expected
    ///   governed record is absent).
    /// - One match: the returned binding set *replaces* the current
    ///   bindings; later statements see the newly-bound variables.
    /// - Multiple matches: `EvalError::TypeMismatch` (the programme
    ///   expected unique state but admitted ambiguous state - a missing
    ///   structural-uniqueness invariant, or corruption).
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
    /// Pattern-based retraction. Each Var in `args` is resolved against
    /// the current bindings; each Wildcard matches anything. All claims
    /// in the pre-state matching the resolved pattern are staged for
    /// retraction. Zero matches is an idempotent no-op (not an error).
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

/// A named, parameterised proposal to change admitted state. A
/// transformation is the only path by which governed state may change.
/// Its body is a sequence of [`Stmt`]s; when invoked via [`crate::propose`],
/// the body executes against a snapshot of pre-state, stages assertions
/// and retractions and intents, and produces an [`crate::Outcome`] that the
/// caller can either commit or discard.
///
/// Reads inside a transformation always see the *pre-transformation*
/// snapshot. Writes are staged and become real only at commit.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Transformation {
    pub name: TransformationName,
    pub parameters: Vec<Var>,
    pub body: Vec<Stmt>,
}

/// A named, parameterised proposition: a reusable condition declared once
/// and called from invariant bodies, transformation gates, derived-claim
/// domains, and other definitions. Body grammar, not a third first-class
/// construct: a definition never changes state and carries no standing -
/// it only names a [`Prop`] so the rules that use it read as the business
/// speaks.
///
/// A call site is a [`Prop::Defined`] node whose `args` pair positionally
/// with `parameters`. Evaluation is relational substitution with
/// projection: the body is evaluated under a fresh binding context
/// carrying only the parameters (ground call arguments pre-bind their
/// parameter; unbound ones leave it free, acting as a generator), and
/// each body match projects the parameter values back onto the call's
/// argument terms. The body cannot see the caller's other bindings, and
/// the caller never sees the body's internal names - a call binds exactly
/// its argument variables, like a claim match.
///
/// Bodies are context-free in v0: `Term::Actor` and `Prop::Pre` inside a
/// definition body are validation errors, so a definition means the same
/// thing in a gate as in an invariant. (`actor` is passed as an ordinary
/// call argument where a gate needs it; a *call* wrapped in `pre(...)`
/// works, because the context swap applies to the body's evaluation.)
/// Definitions may call other definitions; cycles are a validation
/// error. A parameter the body binds is generator-capable (a call may
/// pass an unbound variable there); a parameter the body only uses
/// must arrive bound at every call; a parameter the body never
/// references is refused.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Definition {
    pub name: DefinitionName,
    pub parameters: Vec<Var>,
    pub body: Prop,
    /// Who wrote this definition. Discipline-generated selectors must be
    /// distinguishable from authored ones: the formatter has to omit them
    /// (printing one makes it authored on reparse), the lowering has to
    /// know whether it has already run, and a hand-built programme that
    /// shadows a generated name must not lose it silently. Matching on
    /// the name alone got all three subtly wrong.
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

/// A governed domain model: a named set of predicate and intent
/// vocabularies, invariants, transformations, and derived claims,
/// packaged so the runtime, CLI, and external callers can refer to it as
/// one unit. It is the smallest possible container - it owns no state,
/// no connection, no schema, just the rules and the admitted
/// state-change paths. A caller proposes by looking up a transformation
/// by name and passing it to [`crate::propose`] (or the PostgreSQL
/// adapter's `propose_against_pg`) with the invariants and arguments.
/// `name` is a stable snake_case identifier the CLI selects on; each
/// worked example exposes a `program()` constructor.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Program {
    pub name: String,
    /// The vocabulary of admissible claim shapes. Every `Prop::Claim`,
    /// `Stmt::Assert`, `Stmt::Retract`, `ValueExpr::ValueOf`, and
    /// `DerivedClaim` output must target a declared predicate (validated
    /// by [`Program::validate`]).
    pub predicates: Vec<PredicateDecl>,
    /// The vocabulary of outbox intent shapes this programme may emit.
    /// Every `Stmt::Emit` must target a declared intent, so a misspelled
    /// name is a validation error, not a silent route-to-nowhere.
    /// Separate namespace from predicates.
    pub intents: Vec<IntentDecl>,
    /// Named, parameterised propositions (see [`Definition`]). Shares the
    /// claim-shaped reference namespace with `predicates` - a body
    /// reference `name(args)` resolves to a predicate or a definition,
    /// never both - so a definition name colliding with a predicate name
    /// is a validation error.
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
    /// derived claim in the program has that name. Symmetric with
    /// [`Program::transformation`] and [`Program::invariant`].
    pub fn derived_claim(&self, name: &str) -> Option<&DerivedClaim> {
        self.derived_claims
            .iter()
            .find(|d| d.predicate.as_str() == name)
    }

    /// Look up a predicate declaration by name. Returns `None` if no
    /// declaration in the program has that name. Symmetric with the
    /// other lookup methods.
    ///
    /// If duplicate declarations exist this returns the first; the
    /// validator's arity lookup uses the last. Either way, duplicate
    /// declarations are invalid and are reported by
    /// [`Program::validate`] as `ValidationError::DuplicateDecl`.
    pub fn predicate(&self, name: &str) -> Option<&PredicateDecl> {
        self.predicates.iter().find(|p| p.name.as_str() == name)
    }

    /// Look up an intent declaration by name. Returns `None` if no
    /// intent in the program has that name. Same duplicate-handling
    /// semantics as [`Self::predicate`].
    pub fn intent(&self, name: &str) -> Option<&IntentDecl> {
        self.intents.iter().find(|i| i.name.as_str() == name)
    }

    /// Full static validation of the whole programme. Several checks
    /// contribute to a single error list:
    ///
    /// - **Structural**: every predicate and intent reference targets
    ///   a declaration at the declared arity; no two declarations in a
    ///   vocabulary share a name.
    /// - **Kind/type**: every value flowing into a slot, comparator,
    ///   or arithmetic operand carries a compatible kind; variables
    ///   refine-and-conflict across their uses; `Any` is unconstrained,
    ///   not a kind-eraser.
    /// - **Binding flow**: a name consumed where a bound value is
    ///   required must have been bound first, following the runtime
    ///   quartet's export rules.
    /// - **Actor context**: `Term::Actor` in an invariant or
    ///   derived-claim body, where no proposing transition is in scope.
    /// - **Nesting depth**: a body whose expressions or `for`-statements
    ///   nest past a fixed limit, which the recursive evaluator would
    ///   otherwise risk exhausting the stack on.
    ///
    /// Returns the **full** error list on failure (not just the
    /// first); a programme migration that adds declarations should
    /// see every site at once.
    ///
    /// A derived claim's domain naming a derived claim - its own or
    /// another's - is refused: a derived is computed from admitted
    /// claims, and nothing admits a derived, so deriveds do not compose.
    ///
    /// Out of scope for v0: source spans on diagnostics (the IR drops
    /// parser spans on lowering).
    ///
    /// `validate` is **not** called automatically by `propose`. The
    /// kernel boundary is statement-level, not programme-level;
    /// adding a programme validation pass to every proposal would
    /// muddle that distinction. `morpholog check` runs it
    /// explicitly; tests over the worked examples do the same.
    pub fn validate(&self) -> Result<(), Vec<ValidationError>> {
        validate_program(self)
    }

    /// Validate and return a proof-of-validity handle. Same checks
    /// as [`Self::validate`], but the success case carries a
    /// [`crate::ValidatedProgram`] the analysis surface
    /// ([`crate::transformation_param_kinds`],
    /// [`crate::transformation_arg_schema`]) consumes - so callers
    /// that need both validation and analysis only pay the
    /// validation cost once, and the analysis API can drop its
    /// defensive re-validation. The error shape is unchanged.
    pub fn validated(&self) -> Result<crate::ValidatedProgram<'_>, Vec<ValidationError>> {
        self.validate()
            .map(|()| crate::ValidatedProgram::from_validated(self))
    }
}

/// A predicate declaration: the name of a predicate and the named-and-
/// kinded shape of its argument list. Declarations appear in
/// [`Program::predicates`]; references appear inside `Prop::Claim`,
/// `Stmt::Assert`, `Stmt::Retract`, `ValueExpr::ValueOf`, and
/// `DerivedClaim` output positions.
///
/// Argument *names* in a declaration are documentation - they describe
/// what each position means, surface in `morpholog inspect predicates`,
/// and inform future parser diagnostics. They have no runtime effect on
/// matching, which remains positional.
///
/// Argument *kinds* (see [`PredicateArgKind`]) constrain the kinds of
/// values flowing through the binding context: [`Program::validate`]
/// checks every value reaching an argument position against the
/// declared kind and rejects incompatible ones.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PredicateDecl {
    pub name: PredicateName,
    pub args: Vec<ArgDecl>,
    /// Declared claim disciplines (see [`Discipline`]). Serialised only
    /// when present, so manifests and `inspect predicates` output for
    /// undisciplined programmes are byte-identical to before the field
    /// existed - the wire change is purely additive.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub disciplines: Vec<Discipline>,
}

/// A declared property of a claim shape - a modelling commitment the
/// predicate carries on its face, enforced by lowering to ordinary
/// generated invariants (see `lower_disciplines`) or, where cheaper, by
/// a static authoring-time check. Disciplines are deliberately boring,
/// deterministic, generated, visible, and few: properties of claim
/// shapes, never a back door for arbitrary rule templates.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "discipline", rename_all = "snake_case")]
pub enum Discipline {
    /// `unique by (fields)`: the named fields determine the whole
    /// claim - any two claims agreeing on the key fields agree on
    /// every field (SQL-UNIQUE-style full agreement). Lowers to one
    /// generated invariant per clause; several clauses may coexist.
    UniqueBy { fields: Vec<String> },
    /// `effective by (keys) on (date_field)`: this predicate is
    /// effective-dated - one version per key per date, and the version
    /// governing a moment is the latest whose date is not after it.
    ///
    /// Lowers to a generated DEFINITION rather than an invariant: the
    /// selector is something the author calls, not a rule the runtime
    /// enforces. `current pointer by` governs corrections *within* a
    /// version; this governs time *across* versions, and the two compose.
    EffectiveBy {
        keys: Vec<String>,
        on: String,
        /// `partial`: coverage gaps are intended.
        ///
        /// An effective-dated rule passes vacuously where no version is in
        /// force, so a predicate with no totality companion earns a hint.
        /// That hint is right for the usual case and wrong for a model
        /// where a rule genuinely should not apply before the first version
        /// exists - and under `--strict` there was no way to say so, which
        /// left an author choosing between a companion that is not true and
        /// abandoning strict checking entirely.
        ///
        /// A declaration, not a suppression: it states what the author
        /// believes about the model, and contradicting it by also declaring
        /// `total over` this predicate is an error rather than a
        /// preference.
        partial: bool,
    },
    /// `append only`: no transformation may `retract` this predicate.
    /// Enforced statically (retraction only happens through a
    /// `retract` statement, so the authoring-time ban is complete and
    /// costs nothing at runtime). Ordinary programmes correct
    /// append-only claims by supersession or exception claims, never
    /// retraction.
    AppendOnly,
    /// `current pointer by (fields)`: this predicate is a retractable
    /// current-pointer (the doctrine's middle class). Lowers the
    /// pointer singleton - exactly a `unique by (fields)` generated
    /// invariant - and records the class as metadata.
    CurrentPointerBy { fields: Vec<String> },
    /// `superseded via L`: names the lineage predicate recording this
    /// pointer's supersession history. `L` must have exactly two
    /// arguments in the `(successor, prior)` convention the worked
    /// examples established; the lowering generates **no-fork only** -
    /// `unique by` the prior (second) field on `L`, so one prior has
    /// at most one successor - and marks `L` append-only. It does NOT
    /// claim well-formed lineage: joins (two priors sharing a
    /// successor) and cycles are not prevented. Only meaningful on a
    /// `current pointer by` predicate; required to accompany one.
    SupersededVia { lineage: PredicateName },
}

/// One argument-position declaration. Used by both
/// [`PredicateDecl`] and [`IntentDecl`]; both vocabularies share
/// the same `name`-plus-kind shape.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ArgDecl {
    pub name: String,
    pub kind: PredicateArgKind,
}

/// A declaration of an outbox intent: the intent name and the
/// shape of its argument list. Declarations appear in
/// [`Program::intents`]; references appear inside [`Stmt::Emit`].
///
/// Mirrors [`PredicateDecl`] structurally - intents and predicates
/// have the same shape (named, kinded positional args) but live in
/// distinct vocabularies because they play distinct roles: predicates
/// describe admitted claim shapes, intents describe outbox-effect
/// shapes. The check pass validates `emit` arg kinds against
/// these declarations the same way it validates `assert` against
/// [`PredicateDecl`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct IntentDecl {
    pub name: IntentName,
    pub args: Vec<ArgDecl>,
}

/// The expected kind of a predicate argument position.
///
/// Deliberately a separate type from [`Value`] and [`crate::EvalValue`] - this
/// names a *declaration-time* expectation about an argument position,
/// not a runtime value or an IR literal. Conflating them in a single
/// enum was considered and rejected: the `Value` / `EvalValue` duality
/// is already a delicate distinction (IR-literal vs runtime-value), and
/// a declaration-kind annotation should not be tangled into it.
///
/// `Any` is the kind escape hatch for argument positions whose kind
/// is genuinely polymorphic (e.g. a future audit-row payload that may
/// hold any admitted value), or for declarations that are not yet
/// ready to commit to a kind. Use it sparingly; the value of the
/// declaration metadata is highest when kinds are specific.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub enum PredicateArgKind {
    Subject,
    Decimal,
    Date,
    Timestamp,
    Duration,
    Bool,
    Collection,
    /// A unit-tagged exact decimal - declared `Decimal[USD]` on the
    /// surface. Two quantity kinds are compatible only when their
    /// units are equal; the unit is the whole of the kind's meaning
    /// (a contractual label, not a physical dimension).
    Quantity(Unit),
    /// The kind of a `span(P3M)` calendar-span literal. Expression-only:
    /// the surface has no declaration spelling for it, so no claim,
    /// intent, or transformation argument can carry one - it exists so
    /// kind inference has a name for the literal inside date arithmetic.
    CalendarSpan,
    Any,
}

/// Renders the declaration syntax - `Decimal[USD]`, never a
/// unit-erased "Quantity" - so every diagnostic that names a kind
/// names the unit. The formatter and the validation errors both
/// route through this impl; they cannot drift.
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

/// One computed value in a [`DerivedClaim`]. `name` is the variable
/// name within the derived claim's scope (used only for documentation
/// today; the output [`crate::ClaimInstance`] is positional, key values
/// followed by computed values in declaration order). `expr` is a
/// [`ValueExpr`] that runs once per distinct key binding.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DerivedValue {
    pub name: String,
    pub expr: ValueExpr,
}
