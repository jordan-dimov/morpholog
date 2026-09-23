//! Rust helpers for building kernel IR by hand - **not** the Morpholog
//! language, which is `.morph`. In the product only the parser builds IR.
//! These exist for tests that need precise edge cases or malformed shapes
//! a parser would never emit.
//!
//! The IR has no sugar, so raw construction is verbose. These thin
//! wrappers make it readable:
//!
//! ```ignore
//! use morpholog_core::ir_builder::*;
//!
//! require(and(vec![
//!     claim("Policy", vec![var("policy_id"), wildcard()]),
//!     le(term(var("amount")), term(var("limit"))),
//! ]))
//! ```
//!
//! Naming conventions:
//! - Term literals are short: `subj`, `dec`, `date`. They return `Term`,
//!   ready to drop into a `vec![...]` argument list.
//! - Statement constructors colliding with a Rust keyword take a
//!   trailing underscore: `assert_`, `let_`, `let_new_subject`, `for_`.
//! - `actor()` and `wildcard()` are nullary constructors for
//!   `Term::Actor` and `Term::Wildcard`.

use crate::{
    ArgDecl, ArithOp, Builtin, Claim, CompareOp, Definition, DerivedClaim, Discipline, Intent,
    IntentDecl, Invariant, InvariantOrigin, OrderedDomain, PredicateArgKind, PredicateDecl,
    Program, Prop, Stmt, Subject, SumSeed, Term, Transformation, Value, ValueExpr, Var,
};

/// Build a [`Prop::Compare`] for the comparator constructors below.
fn compare(op: CompareOp, domain: OrderedDomain, lhs: ValueExpr, rhs: ValueExpr) -> Prop {
    Prop::Compare {
        op,
        domain,
        left: Box::new(lhs),
        right: Box::new(rhs),
    }
}

// ============================================================
// Term constructors
// ============================================================

/// A free variable bound somewhere in the surrounding context (a
/// transformation parameter, a `let`, a `for`, an `exists`, or the
/// match positions of an enclosing claim pattern).
pub fn var(name: &str) -> Term {
    Term::Var(name.into())
}

/// Match anything at this position. Valid inside `Prop::Claim`,
/// `Stmt::Retract`, and `ValueExpr::ValueOf` patterns. Invalid in `Term`-as-
/// value positions (resolves to `EvalError::TypeMismatch`).
pub fn wildcard() -> Term {
    Term::Wildcard
}

/// The actor proposing the current transition. Only valid inside a
/// transformation body; in an invariant or derived-claim body it raises
/// `EvalError::UnboundActor`.
pub fn actor() -> Term {
    Term::Actor
}

/// Subject literal: a named constant (a purpose, role or status) or a
/// specific subject identifier.
pub fn subj(s: &str) -> Term {
    Term::Literal(Value::Subject(Subject::from(s)))
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

/// RFC 3339 UTC-instant literal. Stored as the exact source string;
/// the evaluator parses it to `jiff::Timestamp` on use.
pub fn timestamp(s: &str) -> Term {
    Term::Literal(Value::Timestamp(s.to_string()))
}

/// ISO-8601 duration literal in exact time units (e.g. `PT6H`). Stored
/// as the exact source string; the evaluator parses it to
/// `jiff::SignedDuration` on use.
pub fn duration(s: &str) -> Term {
    Term::Literal(Value::Duration(s.to_string()))
}

/// Calendar-span literal in date units (e.g. `P3M`, `P45D`), the
/// surface's `span(P3M)`. Stored as the exact source string; parsed with
/// [`crate::calendar::parse_calendar_span`] on use.
pub fn span(s: &str) -> Term {
    Term::Literal(Value::CalendarSpan(s.to_string()))
}

/// Unit-tagged quantity literal (`qty("25000", "USD")` builds the IR
/// for the surface's `25000 USD`). Amount stored as the exact source
/// string; the unit is an opaque symbol.
pub fn qty(amount: &str, unit: &str) -> Term {
    Term::Literal(Value::Quantity {
        amount: amount.to_string(),
        unit: crate::ir::Unit::from(unit.to_string()),
    })
}

/// Same as [`subj`]; the name tells the reader the subject is a role.
/// Subjects are deliberately untyped, so avoid adding more aliases.
pub fn role(s: &str) -> Term {
    subj(s)
}

// ============================================================
// Prop constructors (the predicate-shaped sort)
// ============================================================

/// Claim pattern. Each `args` term is a variable to bind, a wildcard, a
/// literal to match, or `actor()`. Every position must match.
pub fn claim(predicate: &str, args: Vec<Term>) -> Prop {
    Prop::Claim {
        predicate: predicate.into(),
        args,
    }
}

/// Call to a named [`Definition`]. Args pair with its parameters by
/// position: bound args filter, unbound variables receive the body's
/// bindings.
pub fn defined(name: &str, args: Vec<Term>) -> Prop {
    Prop::Defined {
        name: name.into(),
        args,
    }
}

pub fn and(props: Vec<Prop>) -> Prop {
    Prop::And(props)
}

pub fn or(props: Vec<Prop>) -> Prop {
    Prop::Or(props)
}

pub fn not(inner: Prop) -> Prop {
    Prop::Not(Box::new(inner))
}

pub fn xor(left: Prop, right: Prop) -> Prop {
    Prop::Xor(Box::new(left), Box::new(right))
}

/// Evaluate the wrapped subtree against the state before the transition.
/// Works only in an invariant during a proposal; elsewhere it raises
/// [`crate::EvalError::PreStateUnavailable`].
pub fn pre(inner: Prop) -> Prop {
    Prop::Pre(Box::new(inner))
}

pub fn implies(left: Prop, right: Prop) -> Prop {
    Prop::Implies {
        left: Box::new(left),
        right: Box::new(right),
    }
}

pub fn exists(binding: &str, body: Prop) -> Prop {
    Prop::Exists {
        binding: binding.into(),
        body: Box::new(body),
    }
}

pub fn forall(binding: &str, source: Prop, body: Prop) -> Prop {
    Prop::Forall {
        binding: binding.into(),
        source: Box::new(source),
        body: Box::new(body),
    }
}

pub fn eq(lhs: ValueExpr, rhs: ValueExpr) -> Prop {
    Prop::Eq(Box::new(lhs), Box::new(rhs))
}

/// `Prop::Neq` over two terms, the common case. Build `Prop::Neq`
/// directly to compare full value expressions.
pub fn neq(t1: Term, t2: Term) -> Prop {
    Prop::Neq(Box::new(ValueExpr::Term(t1)), Box::new(ValueExpr::Term(t2)))
}

pub fn le(lhs: ValueExpr, rhs: ValueExpr) -> Prop {
    compare(CompareOp::Le, OrderedDomain::Decimal, lhs, rhs)
}

pub fn date_le(lhs: ValueExpr, rhs: ValueExpr) -> Prop {
    compare(CompareOp::Le, OrderedDomain::Date, lhs, rhs)
}

pub fn timestamp_le(lhs: ValueExpr, rhs: ValueExpr) -> Prop {
    compare(CompareOp::Le, OrderedDomain::Timestamp, lhs, rhs)
}

pub fn duration_le(lhs: ValueExpr, rhs: ValueExpr) -> Prop {
    compare(CompareOp::Le, OrderedDomain::Duration, lhs, rhs)
}

pub fn in_(elem: Term, coll: Term) -> Prop {
    Prop::In(elem, coll)
}

// ============================================================
// ValueExpr constructors (the value-producing sort)
// ============================================================

/// Lift a [`Term`] into value position (inside `le`, `add`, `sum`'s
/// value, and so on).
pub fn term(t: Term) -> ValueExpr {
    ValueExpr::Term(t)
}

fn arith(op: ArithOp, lhs: ValueExpr, rhs: ValueExpr) -> ValueExpr {
    ValueExpr::Arith {
        op,
        left: Box::new(lhs),
        right: Box::new(rhs),
    }
}

pub fn sub(lhs: ValueExpr, rhs: ValueExpr) -> ValueExpr {
    arith(ArithOp::Sub, lhs, rhs)
}

pub fn add(lhs: ValueExpr, rhs: ValueExpr) -> ValueExpr {
    arith(ArithOp::Add, lhs, rhs)
}

pub fn mul(lhs: ValueExpr, rhs: ValueExpr) -> ValueExpr {
    arith(ArithOp::Mul, lhs, rhs)
}

pub fn div(lhs: ValueExpr, rhs: ValueExpr) -> ValueExpr {
    arith(ArithOp::Div, lhs, rhs)
}

/// `abs(x)` - the magnitude of a signed value (decimal, quantity, or
/// duration), preserving its kind.
pub fn abs(operand: ValueExpr) -> ValueExpr {
    call(Builtin::Abs, vec![operand])
}

/// A strict call to a builtin. The named helpers below are the ones
/// worth spelling; this is the general form.
pub fn call(builtin: Builtin, args: Vec<ValueExpr>) -> ValueExpr {
    ValueExpr::Call { builtin, args }
}

/// `period_index(anchor, span, at)` - which anniversary-anchored
/// period `at` falls in, as an integer-valued decimal; negative
/// before the anchor.
pub fn period_index(anchor: ValueExpr, span: ValueExpr, at: ValueExpr) -> ValueExpr {
    call(Builtin::PeriodIndex, vec![anchor, span, at])
}

/// `period_start_of(anchor, span, index)` - the first day of period
/// `index`. Refuses an index whose start falls outside the calendar.
pub fn period_start_of(anchor: ValueExpr, span: ValueExpr, index: ValueExpr) -> ValueExpr {
    call(Builtin::PeriodStartOf, vec![anchor, span, index])
}

/// `if(when, then, otherwise)` - `then` if the proposition holds, else
/// `otherwise`. Only the chosen branch is evaluated; the condition binds
/// nothing.
pub fn cond(when: Prop, then: ValueExpr, otherwise: ValueExpr) -> ValueExpr {
    ValueExpr::Cond {
        when: Box::new(when),
        then: Box::new(then),
        otherwise: Box::new(otherwise),
    }
}

/// `round(x, quantum)` - the multiple of `quantum` nearest to `x`,
/// exact halves away from zero. Decimal-only.
pub fn round(value: ValueExpr, quantum: ValueExpr) -> ValueExpr {
    call(Builtin::Round, vec![value, quantum])
}

// `modulo`, not `mod`: the latter is a Rust keyword.
pub fn modulo(lhs: ValueExpr, rhs: ValueExpr) -> ValueExpr {
    arith(ArithOp::Mod, lhs, rhs)
}

pub fn min(lhs: ValueExpr, rhs: ValueExpr) -> ValueExpr {
    call(Builtin::Min, vec![lhs, rhs])
}

pub fn max(lhs: ValueExpr, rhs: ValueExpr) -> ValueExpr {
    call(Builtin::Max, vec![lhs, rhs])
}

pub fn sum(value: impl Into<ValueExpr>, body: Prop) -> ValueExpr {
    ValueExpr::Sum {
        value: Box::new(value.into()),
        body: Box::new(body),
        seed: SumSeed::default(),
    }
}

/// Match exactly one claim and yield the value at its first wildcard.
/// Zero matches is an error (see [`value_of_with_default`] for a
/// fallback); more than one always is.
///
/// **Prefer [`bind_one`] in transformation bodies**: it reads more
/// directly and rejects cleanly on zero matches. Use `value_of` only in
/// value positions (arithmetic, comparisons, `Sum`, `Let`, a derived-claim
/// value).
pub fn value_of(predicate: &str, args: Vec<Term>) -> ValueExpr {
    let extract = first_wildcard(&args);
    ValueExpr::ValueOf {
        predicate: predicate.into(),
        args,
        extract,
        default: None,
    }
}

/// `value_of` with a fallback expression evaluated when zero matches.
/// Multiple matches still error.
pub fn value_of_with_default(predicate: &str, args: Vec<Term>, default: ValueExpr) -> ValueExpr {
    let extract = first_wildcard(&args);
    ValueExpr::ValueOf {
        predicate: predicate.into(),
        args,
        extract,
        default: Some(Box::new(default)),
    }
}

/// The first wildcard is the value to extract. With none, the index is
/// out of range and validation refuses it, so the builder never fails.
fn first_wildcard(args: &[Term]) -> usize {
    args.iter()
        .position(|t| matches!(t, Term::Wildcard))
        .unwrap_or(args.len())
}

/// `value_of` extracting position `extract`, which need not be the first
/// wildcard. `args[extract]` must be a wildcard or validation refuses.
pub fn value_of_extracting(predicate: &str, args: Vec<Term>, extract: usize) -> ValueExpr {
    ValueExpr::ValueOf {
        predicate: predicate.into(),
        args,
        extract,
        default: None,
    }
}

// ============================================================
// Stmt constructors
// ============================================================

pub fn require(prop: Prop) -> Stmt {
    Stmt::Require { prop, name: None }
}

/// A `require` carrying the author's stable identifier, so a refusal names
/// the rule instead of quoting the expression.
pub fn require_named(name: &str, prop: Prop) -> Stmt {
    Stmt::Require {
        prop,
        name: Some(name.into()),
    }
}

/// Unique lookup. Unlike [`require`], which is a yes/no gate, `bind_one`
/// *replaces* the binding context with the single match. No match is a
/// rejection (a business outcome); more than one is a kernel error (a
/// programme bug).
///
/// ```ignore
/// bind_one(claim("Policy", vec![var("policy_id"), var("aggregate_limit")]))
/// ```
///
/// binds both `policy_id` and `aggregate_limit` for the rest of the body.
pub fn bind_one(prop: Prop) -> Stmt {
    Stmt::BindOne { prop, name: None }
}

/// The named counterpart to [`bind_one`], for the same reason
/// [`require_named`] exists.
pub fn bind_one_named(name: &str, prop: Prop) -> Stmt {
    Stmt::BindOne {
        prop,
        name: Some(name.into()),
    }
}

pub fn assert_(predicate: &str, args: Vec<Term>) -> Stmt {
    Stmt::Assert(Claim {
        predicate: predicate.into(),
        args,
    })
}

pub fn retract(predicate: &str, args: Vec<Term>) -> Stmt {
    Stmt::Retract {
        predicate: predicate.into(),
        args,
    }
}

pub fn emit(name: &str, args: Vec<Term>) -> Stmt {
    Stmt::Emit(Intent {
        name: name.into(),
        args,
    })
}

pub fn let_(name: &str, value: ValueExpr) -> Stmt {
    Stmt::Let {
        name: name.into(),
        value,
    }
}

pub fn let_new_subject(name: &str) -> Stmt {
    Stmt::LetNewSubject { name: name.into() }
}

pub fn for_(binding: &str, collection: ValueExpr, body: Vec<Stmt>) -> Stmt {
    Stmt::For {
        binding: binding.into(),
        collection,
        body,
    }
}

// ============================================================
// Parameter-list sugar
// ============================================================

/// Parameter list for a [`crate::Transformation`]:
/// `params(&["claim_id", "amount"])`.
pub fn params(names: &[&str]) -> Vec<Var> {
    names.iter().map(|s| Var::from(*s)).collect()
}

// ============================================================
// PredicateDecl builder
// ============================================================

/// Builder for a [`PredicateDecl`]. Start with [`predicate`], call one kind
/// method per argument in positional order, and finish with
/// [`PredicateDeclBuilder::build`].
#[must_use]
pub struct PredicateDeclBuilder {
    name: String,
    args: Vec<ArgDecl>,
    disciplines: Vec<Discipline>,
}

impl PredicateDeclBuilder {
    fn arg(mut self, name: &str, kind: PredicateArgKind) -> Self {
        self.args.push(ArgDecl {
            name: name.to_string(),
            kind,
        });
        self
    }

    pub fn subject(self, name: &str) -> Self {
        self.arg(name, PredicateArgKind::Subject)
    }

    pub fn decimal(self, name: &str) -> Self {
        self.arg(name, PredicateArgKind::Decimal)
    }

    pub fn date(self, name: &str) -> Self {
        self.arg(name, PredicateArgKind::Date)
    }

    pub fn timestamp(self, name: &str) -> Self {
        self.arg(name, PredicateArgKind::Timestamp)
    }

    pub fn duration(self, name: &str) -> Self {
        self.arg(name, PredicateArgKind::Duration)
    }

    /// Unit-tagged decimal argument - the surface's `name: Decimal[USD]`.
    pub fn quantity(self, name: &str, unit: &str) -> Self {
        self.arg(
            name,
            PredicateArgKind::Quantity(crate::ir::Unit::from(unit.to_string())),
        )
    }

    /// Boolean argument. Not `bool`, which would read as a cast.
    pub fn boolean(self, name: &str) -> Self {
        self.arg(name, PredicateArgKind::Bool)
    }

    pub fn collection(self, name: &str) -> Self {
        self.arg(name, PredicateArgKind::Collection)
    }

    /// Kind escape hatch for a genuinely polymorphic argument position.
    pub fn any(self, name: &str) -> Self {
        self.arg(name, PredicateArgKind::Any)
    }

    /// Attach declared disciplines; see [`Discipline`].
    pub fn disciplines(mut self, disciplines: Vec<Discipline>) -> Self {
        self.disciplines = disciplines;
        self
    }

    pub fn build(self) -> PredicateDecl {
        PredicateDecl {
            name: self.name.into(),
            args: self.args,
            disciplines: self.disciplines,
        }
    }
}

/// Start a predicate declaration. Chain one kind method per argument
/// position and finish with `.build()`.
pub fn predicate(name: &str) -> PredicateDeclBuilder {
    PredicateDeclBuilder {
        name: name.to_string(),
        args: Vec::new(),
        disciplines: Vec::new(),
    }
}

// ============================================================
// Top-level declaration builders
// ============================================================

/// Build an [`Invariant`] with `version` 1, as the surface emits.
pub fn invariant(name: &str, body: Prop) -> Invariant {
    Invariant {
        totality_for: None,
        name: name.into(),
        version: 1,
        body,
        origin: InvariantOrigin::Authored,
    }
}

/// Build a [`Transformation`]. Parameters come from [`params`].
pub fn transformation(name: &str, parameters: Vec<Var>, body: Vec<Stmt>) -> Transformation {
    Transformation {
        name: name.into(),
        parameters,
        body,
    }
}

/// Build a [`Definition`] - a named, parameterised proposition.
/// Parameters come from [`params`]; call it with [`defined`].
pub fn definition(name: &str, parameters: Vec<Var>, body: Prop) -> Definition {
    Definition {
        name: name.into(),
        parameters,
        body,
        origin: crate::ir::DefinitionOrigin::Authored,
    }
}

/// Builder for a [`Program`]. Set the non-empty sections and finish with
/// `.build()`; omitted sections default to empty.
#[must_use]
pub struct ProgramBuilder {
    name: String,
    predicates: Vec<PredicateDecl>,
    intents: Vec<IntentDecl>,
    definitions: Vec<Definition>,
    invariants: Vec<Invariant>,
    transformations: Vec<Transformation>,
    derived_claims: Vec<DerivedClaim>,
}

impl ProgramBuilder {
    pub fn predicates(mut self, v: Vec<PredicateDecl>) -> Self {
        self.predicates = v;
        self
    }

    pub fn intents(mut self, v: Vec<IntentDecl>) -> Self {
        self.intents = v;
        self
    }

    pub fn definitions(mut self, v: Vec<Definition>) -> Self {
        self.definitions = v;
        self
    }

    pub fn invariants(mut self, v: Vec<Invariant>) -> Self {
        self.invariants = v;
        self
    }

    pub fn transformations(mut self, v: Vec<Transformation>) -> Self {
        self.transformations = v;
        self
    }

    pub fn derived_claims(mut self, v: Vec<DerivedClaim>) -> Self {
        self.derived_claims = v;
        self
    }

    pub fn build(self) -> Program {
        Program {
            name: self.name,
            predicates: self.predicates,
            intents: self.intents,
            definitions: self.definitions,
            invariants: self.invariants,
            transformations: self.transformations,
            derived_claims: self.derived_claims,
        }
    }
}

/// Start a [`Program`]; chain section setters and finish with `.build()`.
pub fn program(name: &str) -> ProgramBuilder {
    ProgramBuilder {
        name: name.to_string(),
        predicates: Vec::new(),
        intents: Vec::new(),
        definitions: Vec::new(),
        invariants: Vec::new(),
        transformations: Vec::new(),
        derived_claims: Vec::new(),
    }
}
