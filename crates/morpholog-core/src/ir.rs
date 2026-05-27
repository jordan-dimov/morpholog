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
    /// Binary decimal arithmetic: `left <op> right`, both operands
    /// evaluating to `EvalValue::Decimal`. The operator is the [`ArithOp`]
    /// field rather than a variant per operator - the value-sort analogue
    /// of [`Prop::Compare`] carrying a [`CompareOp`]. `Div` and `Mod`
    /// surface [`crate::EvalError::DivisionByZero`] on a zero divisor; the
    /// rest are total. Admission gates express ratio rules in the
    /// multiplied form (`a <= c*b`, not `a/b <= c`) to stay exact; `Div`
    /// is reserved for read-side projections.
    Arith {
        op: ArithOp,
        left: Box<ValueExpr>,
        right: Box<ValueExpr>,
    },
    /// Sums `value` over every binding the `body` produces. `value` is
    /// usually a variable bound by the body (`sum(amount | ...)`); a
    /// decimal-literal `value` turns the sum into a count of matches
    /// (`sum(1 | ...)`).
    Sum {
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

/// A binary decimal arithmetic operator. Carried by [`ValueExpr::Arith`];
/// the value-sort analogue of [`CompareOp`], replacing what would be a flat
/// variant per operator. A new operator is one row here, not a fresh
/// `ValueExpr` variant rippled across every match.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ArithOp {
    Add,
    Sub,
    Mul,
    Div,
    Min,
    Max,
    /// Decimal remainder (`%`). Like `Div`, a zero divisor surfaces
    /// [`crate::EvalError::DivisionByZero`]. Expresses parity and cyclic
    /// rules - `(file + rank) % 2` for a chess square's colour.
    Mod,
}

impl ArithOp {
    /// Infix operators (`+` `-` `*` `/` `%`) render `left <op> right` and
    /// parenthesise inside another arithmetic operand; the function-form
    /// operators (`min` / `max`) render `op(left, right)` and are
    /// self-delimiting, needing no parens.
    pub fn is_infix(self) -> bool {
        matches!(
            self,
            ArithOp::Add | ArithOp::Sub | ArithOp::Mul | ArithOp::Div | ArithOp::Mod
        )
    }
}

/// The ordered domain an [`Prop::Compare`] compares over. Explicit in the
/// IR, never inferred from operand kind: the surface picks it by token (`<`
/// decimal, `before` date), so there is no runtime operator overloading and
/// each domain type-checks its own operands.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OrderedDomain {
    Decimal,
    Date,
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
    /// No time-of-day, no time zone: validity-window modelling on civil
    /// dates is the only temporal primitive in v0.
    Date(String),
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
    Require(Prop),
    /// Deterministic unique-lookup binding statement. Evaluates a
    /// predicate-shaped proposition against current state and bindings:
    /// - Zero matches: transformation rejected (lawful: the expected
    ///   governed record is absent).
    /// - One match: the returned binding set *replaces* the current
    ///   bindings; later statements see the newly-bound variables.
    /// - Multiple matches: `EvalError::TypeMismatch` (the programme
    ///   expected unique state but admitted ambiguous state - a missing
    ///   structural-uniqueness invariant, or corruption).
    BindOne(Prop),
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
    /// Out of scope for v0: recursive derived-claim references from
    /// inside the same derived claim's domain; source spans on
    /// diagnostics (the IR drops parser spans on lowering).
    ///
    /// `validate` is **not** called automatically by `propose`. The
    /// kernel boundary is statement-level, not programme-level;
    /// adding a programme validation pass to every proposal would
    /// muddle that distinction. `morpholog check` runs it
    /// explicitly; tests on the built-in registry do the same.
    pub fn validate(&self) -> Result<(), Vec<ValidationError>> {
        validate_program(self)
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
/// enum was considered and rejected (CLAUDE.md: the
/// `Value`/`EvalValue` duality is already a delicate distinction; a
/// declaration-kind annotation should not be tangled into it).
///
/// `Any` is the kind escape hatch for argument positions whose kind
/// is genuinely polymorphic (e.g. a future audit-row payload that may
/// hold any admitted value), or for declarations that are not yet
/// ready to commit to a kind. Use it sparingly; the value of the
/// declaration metadata is highest when kinds are specific.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum PredicateArgKind {
    Subject,
    Decimal,
    Date,
    Bool,
    Collection,
    Any,
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
