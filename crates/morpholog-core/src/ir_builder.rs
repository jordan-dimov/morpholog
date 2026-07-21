//! A Rust construction kit for the kernel IR - **not** the Morpholog
//! language. The product surface is `.morph`; the worked examples are
//! authored there and parsed, and the parser is the only thing in the
//! product that builds IR. These builders exist for tests: assembling
//! precise edge cases and adversarial or malformed shapes (the kind a
//! parser would never emit) directly, where authoring a `.morph` file
//! would be the wrong tool.
//!
//! The kernel IR is deliberately low-level - every variant is one thing,
//! no syntactic sugar - so hand-constructing a transformation body reads
//! like raw struct construction. These thin wrappers give it a readable
//! shape:
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
    ArgDecl, ArithOp, Claim, CompareOp, Definition, DerivedClaim, Discipline, Intent, IntentDecl,
    Invariant, InvariantOrigin, OrderedDomain, PredicateArgKind, PredicateDecl, Program, Prop,
    Stmt, Subject, SumSeed, Term, Transformation, Value, ValueExpr, Var,
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

/// Unit-tagged quantity literal (`qty("25000", "USD")` builds the IR
/// for the surface's `25000 USD`). Amount stored as the exact source
/// string; the unit is an opaque symbol.
pub fn qty(amount: &str, unit: &str) -> Term {
    Term::Literal(Value::Quantity {
        amount: amount.to_string(),
        unit: crate::ir::Unit::from(unit.to_string()),
    })
}

/// Semantic alias for [`subj`]. Identical runtime representation;
/// documents reader intent at the call site when the subject names a
/// delegated role. Resist adding more aliases until an example forces
/// one - the subject-as-string model is intentional, and pseudo-types
/// over it would not help.
pub fn role(s: &str) -> Term {
    subj(s)
}

// ============================================================
// Prop constructors (the predicate-shaped sort)
// ============================================================

/// Claim pattern. Each `args` term is either a variable to bind, a
/// wildcard, a literal to match, or `actor()`. Match semantics:
/// every position must unify against the candidate claim.
pub fn claim(predicate: &str, args: Vec<Term>) -> Prop {
    Prop::Claim {
        predicate: predicate.into(),
        args,
    }
}

/// Call to a named [`Definition`]. The call-shaped sibling of [`claim`]:
/// args pair positionally with the definition's parameters, ground args
/// filter, unbound variable args receive the body's projected bindings.
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

/// Opt the wrapped subtree into pre-transition state lookup.
/// Legal only inside invariant bodies during a proposal; surfaces
/// [`crate::EvalError::PreStateUnavailable`] anywhere else.
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

/// `Prop::Neq` over two terms - the common authoring case (comparing two
/// variables or literals). The IR's `Neq` is symmetric with `Eq` and
/// accepts full value expressions; build `Prop::Neq` directly for the
/// rarer expression-on-either-side case.
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

/// Lift a [`Term`] into value position. Used wherever a sub-expression
/// must evaluate to a value (e.g. inside `le`, `date_le`, `add`, `sub`,
/// `sum`'s value).
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
    ValueExpr::Abs(Box::new(operand))
}

// `modulo`, not `mod`: the latter is a Rust keyword.
pub fn modulo(lhs: ValueExpr, rhs: ValueExpr) -> ValueExpr {
    arith(ArithOp::Mod, lhs, rhs)
}

pub fn min(lhs: ValueExpr, rhs: ValueExpr) -> ValueExpr {
    arith(ArithOp::Min, lhs, rhs)
}

pub fn max(lhs: ValueExpr, rhs: ValueExpr) -> ValueExpr {
    arith(ArithOp::Max, lhs, rhs)
}

pub fn sum(value: Term, body: Prop) -> ValueExpr {
    ValueExpr::Sum {
        value,
        body: Box::new(body),
        seed: SumSeed::default(),
    }
}

/// Functional lookup: match exactly one claim and yield its
/// wildcard-position value. Zero matches errors unless `default` is
/// supplied; multiple matches always errors. Use
/// [`value_of_with_default`] for the fallback form.
///
/// **Prefer [`bind_one`] in transformation bodies.** To extract a
/// uniquely-matching claim's values into the statement-level binding
/// context, `bind_one` reads more directly and rejects lawfully on
/// zero matches. Reach for `value_of` only in value-producing
/// positions (arithmetic, comparisons, `Sum`, `Let`, or a
/// `DerivedClaim` value expression) where a statement form does not fit.
pub fn value_of(predicate: &str, args: Vec<Term>) -> ValueExpr {
    ValueExpr::ValueOf {
        predicate: predicate.into(),
        args,
        default: None,
    }
}

/// `value_of` with a fallback expression evaluated when zero matches.
/// Multiple matches still error.
pub fn value_of_with_default(predicate: &str, args: Vec<Term>, default: ValueExpr) -> ValueExpr {
    ValueExpr::ValueOf {
        predicate: predicate.into(),
        args,
        default: Some(Box::new(default)),
    }
}

// ============================================================
// Stmt constructors
// ============================================================

pub fn require(prop: Prop) -> Stmt {
    Stmt::Require(prop)
}

/// Deterministic unique-lookup binding statement. The companion to
/// [`require`]: where `require` is a yes/no gate that does not export
/// bindings, `bind_one` evaluates a predicate-shaped expression,
/// *replaces* the current binding context with the single matching
/// binding set, and short-circuits with a kernel error if more than
/// one claim matches (programme bug) or a lawful rejection if none
/// matches (business outcome).
///
/// ```ignore
/// bind_one(claim("Policy", vec![var("policy_id"), var("aggregate_limit")]))
/// ```
///
/// binds both `policy_id` and `aggregate_limit` for the rest of the body.
pub fn bind_one(prop: Prop) -> Stmt {
    Stmt::BindOne(prop)
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

/// Convenience for the parameter list of a [`crate::Transformation`].
/// Equivalent to `names.iter().map(|s| Var::from(*s)).collect()` but
/// reads as `params(&["claim_id", "amount"])` at the call site.
pub fn params(names: &[&str]) -> Vec<Var> {
    names.iter().map(|s| Var::from(*s)).collect()
}

// ============================================================
// PredicateDecl builder
// ============================================================

/// Builder for a [`PredicateDecl`]. Construct with [`predicate`],
/// chain one kind method per argument position
/// (`subject`/`decimal`/`date`/`boolean`/`collection`/`any`), and
/// terminate with [`PredicateDeclBuilder::build`]. Call order is the
/// predicate's positional argument order. Names surface in
/// `morpholog inspect predicates`; kinds drive kind-checking.
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

    /// Boolean-kinded argument. Named `boolean` rather than `bool`
    /// because `bool` is the Rust type and `.bool(name)` reads as a
    /// cast at the call site.
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

/// Build an [`Invariant`]. `version` defaults to 1, the v0 value the
/// surface always emits.
pub fn invariant(name: &str, body: Prop) -> Invariant {
    Invariant {
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
