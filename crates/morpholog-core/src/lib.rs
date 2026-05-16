//! Morpholog v0 semantic kernel.
//!
//! This crate is the synchronous, pure heart of Morpholog. It defines
//! the IR (invariants, transformations, claims, statements, expressions),
//! evaluates invariants against in-memory state, and exposes [`propose`]
//! — the function that turns a proposed transformation into either an
//! accepted post-state or a rejected attempt.
//!
//! `morpholog-core` does no I/O. The PostgreSQL persistence adapter
//! lives in the separate `morpholog-postgres` crate and wraps this
//! kernel as an async boundary.
//!
//! Worked examples (IR data) live under [`examples`].

use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::str::FromStr;

pub mod examples;

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
    pub name: String,
    pub version: u32,
    pub body: Expr,
}

/// Expression nodes used inside invariant bodies, transformation requires,
/// and let-bindings. An `Expr` is evaluated against a state and a set of
/// variable bindings to yield either a boolean / truth-witness (when used
/// as a predicate) or a value (when used in value position).
///
/// The variants are deliberately narrow: predicate composition (`And`,
/// `Not`, `Implies`, `Exists`, `Forall`), claim and inequality matching
/// (`Claim`, `Neq`, `Eq`), one bounded aggregation (`Sum`), one collection
/// primitive (`In`), one functional-lookup primitive (`ValueOf`), and
/// `Term`-as-value lifting. Anything that cannot be expressed within this
/// set is, by design, not yet a runtime concern.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Expr {
    Claim {
        predicate: String,
        args: Vec<Term>,
    },
    Implies {
        left: Box<Expr>,
        right: Box<Expr>,
    },
    Exists {
        binding: String,
        body: Box<Expr>,
    },
    And(Vec<Expr>),
    Not(Box<Expr>),
    Neq(Term, Term),
    Term(Term),
    Eq(Box<Expr>, Box<Expr>),
    Sum {
        value: Term,
        binding: String,
        body: Box<Expr>,
    },
    Forall {
        binding: String,
        source: Box<Expr>,
        body: Box<Expr>,
    },
    In(Term, Term),
    /// Reads exactly one matching claim and yields its value-position binding.
    /// Wildcards in `args` mark the value position(s). Zero matches is an
    /// error unless `default` is supplied; multiple matches is always an error.
    ValueOf {
        predicate: String,
        args: Vec<Term>,
        default: Option<Box<Expr>>,
    },
}

/// A positional argument in a claim, intent, or expression. A `Term` is
/// either a variable to be bound by the surrounding context, a wildcard
/// that matches anything, or a literal constant. Resolved through
/// `Bindings` during evaluation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Term {
    Var(String),
    Wildcard,
    Literal(Value),
}

/// Literal constants embeddable in IR `Term`s. Distinct from `EvalValue`
/// (which is a runtime value, including booleans and collections that
/// cannot appear as IR literals). The variants are deliberately narrow:
/// each was added when a worked example forced it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Value {
    /// Arbitrary-precision decimal stored as its exact source string.
    /// Parsing into a numeric type is the evaluator's concern, not the IR's.
    Decimal(String),
    /// Opaque subject identifier embedded as a literal in the IR.
    /// Lets predicates and requires reference named constants
    /// (purposes, statuses, named authorities, etc.) without forcing
    /// every transformation to take them as extra parameters.
    Subject(String),
}

/// A Claim is an admitted assertion candidate — a statement that may be
/// admitted into governed state. It is not objective reality.
///
/// Distinct from `Expr::Claim`, which is a *query* over candidate state.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Claim {
    pub predicate: String,
    pub args: Vec<Term>,
}

/// An outbound effect declared by a transformation's `emit` statement.
/// Intents are *staged* during transformation execution and *enqueued*
/// to the outbox at commit time; they are never sent during the
/// transaction itself.
///
/// Distinct from [`IntentInstance`], which is the resolved (no-variables)
/// form ready to be enqueued.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Intent {
    pub name: String,
    pub args: Vec<Term>,
}

/// One step inside a transformation body. Statements run in declared
/// order against a binding context; a failing `Require` short-circuits
/// the transformation, while `Assert`, `Retract`, `Emit`, `Let`,
/// `LetNewSubject`, and `For` extend the staged outcome or the binding
/// context. `Retract` of a non-existent claim is an idempotent no-op
/// (see the variant doc), not a short-circuit.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Stmt {
    Require(Expr),
    Let {
        name: String,
        value: Expr,
    },
    LetNewSubject {
        name: String,
    },
    Assert(Claim),
    /// Pattern-based retraction. Each Var in `args` is resolved against
    /// the current bindings; each Wildcard matches anything. All claims
    /// in the pre-state matching the resolved pattern are staged for
    /// retraction. Zero matches is an idempotent no-op (not an error).
    Retract {
        predicate: String,
        args: Vec<Term>,
    },
    For {
        binding: String,
        collection: Expr,
        body: Vec<Stmt>,
    },
    Emit(Intent),
}

/// A named, parameterised proposal to change admitted state. A
/// transformation is the only path by which governed state may change.
/// Its body is a sequence of [`Stmt`]s; when invoked via [`propose`],
/// the body executes against a snapshot of pre-state, stages assertions
/// and retractions and intents, and produces an [`Outcome`] that the
/// caller can either commit or discard.
///
/// Reads inside a transformation always see the *pre-transformation*
/// snapshot. Writes are staged and become real only at commit.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Transformation {
    pub name: String,
    pub parameters: Vec<String>,
    pub body: Vec<Stmt>,
}

// ===========================================================================
// In-memory evaluator
// ===========================================================================

/// A runtime value flowing through evaluation. Distinct from the IR's
/// `Value` (which holds literals only).
///
/// JSON encoding uses an adjacently-tagged shape
/// (`{ "type": "...", "value": ... }`), suitable for the PG JSONB columns
/// defined in `crates/morpholog-core/sql/schema.sql`. Decimals serialise
/// as JSON **strings** to preserve exactness; never as JSON numbers.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", content = "value", rename_all = "lowercase")]
pub enum EvalValue {
    Decimal(#[serde(with = "rust_decimal::serde::str")] Decimal),
    Subject(String),
    Bool(bool),
    Collection(Vec<EvalValue>),
}

/// A grounded claim: all args are values, no variables or wildcards.
///
/// JSON encoding shape: `{ "predicate": "...", "args": [ ... ] }`.
///
/// Used as-is for elements of `audit.asserted_claims` and
/// `audit.retracted_claims` (each column is a JSONB array of these objects).
///
/// For row writes to the `claims` table itself, the PG adapter **splits**
/// the claim across two columns: `predicate_name` (text, from `predicate`)
/// and `arguments` (JSONB array, from `args`). The `arguments` column has
/// a CHECK constraint that requires `jsonb_typeof(arguments) = 'array'`,
/// so writing the full object there would fail. The `claim_args_serialise_as_a_json_array`
/// test pins this contract.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ClaimInstance {
    pub predicate: String,
    pub args: Vec<EvalValue>,
}

/// The admitted state of the runtime: a set of grounded [`ClaimInstance`]s
/// against which invariants are evaluated and transformations are
/// proposed. State is set-valued: identity is `(predicate, args)`. The
/// PG adapter persists this set as rows in `morpholog.claims`; this
/// in-memory representation is what the kernel evaluates against.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct State {
    pub claims: Vec<ClaimInstance>,
}

/// Variable bindings used during expression evaluation and
/// transformation execution. Maps variable name to resolved
/// [`EvalValue`].
pub type Bindings = HashMap<String, EvalValue>;

/// Errors raised by the evaluator and the transformation runner. These
/// are distinct from *lawful business rejection* (which is reported as
/// [`Outcome::Rejected`]); an `EvalError` indicates that an expression
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
        }
    }
}

impl std::error::Error for EvalError {}

/// Evaluate an invariant against a state. Returns true if the invariant
/// holds, false if it fails.
pub fn eval_invariant(inv: &Invariant, state: &State) -> Result<bool, EvalError> {
    let bindings = Bindings::new();
    let matches = find_matches(&inv.body, state, &bindings)?;
    Ok(!matches.is_empty())
}

/// `find_matches` is the predicate-evaluation primitive. It returns the
/// set of binding extensions under which the expression holds. An empty
/// vector means the expression fails; a non-empty vector means it succeeds
/// (potentially with extended bindings).
fn find_matches(e: &Expr, state: &State, base: &Bindings) -> Result<Vec<Bindings>, EvalError> {
    match e {
        Expr::Claim { predicate, args } => find_claim_matches(predicate, args, state, base),
        Expr::And(exprs) => find_conjunction(exprs, state, base),
        Expr::Not(inner) => {
            let m = find_matches(inner, state, base)?;
            Ok(if m.is_empty() {
                vec![base.clone()]
            } else {
                vec![]
            })
        }
        Expr::Implies { left, right } => {
            let lm = find_matches(left, state, base)?;
            for m in lm {
                if find_matches(right, state, &m)?.is_empty() {
                    return Ok(vec![]);
                }
            }
            Ok(vec![base.clone()])
        }
        Expr::Exists { binding: _, body } => {
            let m = find_matches(body, state, base)?;
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
            let sm = find_matches(source, state, base)?;
            for m in sm {
                if find_matches(body, state, &m)?.is_empty() {
                    return Ok(vec![]);
                }
            }
            Ok(vec![base.clone()])
        }
        Expr::Eq(lhs, rhs) => {
            let l = eval_value(lhs, state, base)?;
            let r = eval_value(rhs, state, base)?;
            Ok(if l == r { vec![base.clone()] } else { vec![] })
        }
        Expr::Neq(t1, t2) => {
            let l = resolve_term(t1, base)?;
            let r = resolve_term(t2, base)?;
            Ok(if l != r { vec![base.clone()] } else { vec![] })
        }
        Expr::In(elem, coll) => find_in_matches(elem, coll, base),
        Expr::Term(_) | Expr::Sum { .. } | Expr::ValueOf { .. } => Err(EvalError::NotPredicate),
    }
}

fn find_claim_matches(
    predicate: &str,
    args: &[Term],
    state: &State,
    base: &Bindings,
) -> Result<Vec<Bindings>, EvalError> {
    let mut out = vec![];
    for claim in &state.claims {
        if claim.predicate != predicate || claim.args.len() != args.len() {
            continue;
        }
        if let Some(b) = unify_args(args, &claim.args, base) {
            out.push(b);
        }
    }
    Ok(out)
}

fn unify_args(patterns: &[Term], values: &[EvalValue], base: &Bindings) -> Option<Bindings> {
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
        }
    }
    Some(b)
}

fn find_conjunction(
    exprs: &[Expr],
    state: &State,
    base: &Bindings,
) -> Result<Vec<Bindings>, EvalError> {
    let mut current = vec![base.clone()];
    for expr in exprs {
        let mut next = vec![];
        for b in &current {
            next.extend(find_matches(expr, state, b)?);
        }
        if next.is_empty() {
            return Ok(vec![]);
        }
        current = next;
    }
    Ok(current)
}

fn find_in_matches(elem: &Term, coll: &Term, base: &Bindings) -> Result<Vec<Bindings>, EvalError> {
    let coll_val = resolve_term(coll, base)?;
    let items = match coll_val {
        EvalValue::Collection(v) => v,
        _ => return Err(EvalError::TypeMismatch("In expects a collection".into())),
    };
    match elem {
        Term::Wildcard => Err(EvalError::TypeMismatch("wildcard not valid in In".into())),
        Term::Literal(_) => {
            let e = resolve_term(elem, base)?;
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

fn eval_value(e: &Expr, state: &State, bindings: &Bindings) -> Result<EvalValue, EvalError> {
    match e {
        Expr::Term(t) => resolve_term(t, bindings),
        Expr::Sum {
            value,
            binding: _,
            body,
        } => {
            let matches = find_matches(body, state, bindings)?;
            let mut total = Decimal::ZERO;
            for m in matches {
                match resolve_term(value, &m)? {
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
            let matches = find_claim_matches(predicate, args, state, bindings)?;
            match matches.len() {
                1 => {
                    let pos = args
                        .iter()
                        .position(|t| matches!(t, Term::Wildcard))
                        .ok_or_else(|| {
                            EvalError::TypeMismatch("ValueOf requires a wildcard arg".into())
                        })?;
                    let claim = state
                        .claims
                        .iter()
                        .find(|f| {
                            f.predicate == *predicate
                                && f.args.len() == args.len()
                                && unify_args(args, &f.args, bindings).is_some()
                        })
                        .ok_or_else(|| EvalError::ValueOfZeroMatches(predicate.clone()))?;
                    Ok(claim.args[pos].clone())
                }
                0 => match default {
                    Some(d) => eval_value(d, state, bindings),
                    None => Err(EvalError::ValueOfZeroMatches(predicate.clone())),
                },
                _ => Err(EvalError::ValueOfMultipleMatches(predicate.clone())),
            }
        }
        _ => Err(EvalError::NotValue),
    }
}

fn resolve_term(t: &Term, bindings: &Bindings) -> Result<EvalValue, EvalError> {
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
    }
}

// ===========================================================================
// Transformation execution (in-memory)
// ===========================================================================

/// A resolved intent: all args are values, ready to be enqueued in an outbox.
///
/// JSON encoding shape: `{ "name": "...", "args": [ ... ] }`.
///
/// Used as-is for elements of `audit.emitted_intents` (a JSONB array of these
/// objects).
///
/// For row writes to the `outbox` table, the PG adapter **splits** the intent
/// across two columns: `intent_type` (text, from `name`) and `arguments`
/// (JSONB array, from `args`). The `arguments` column has a CHECK constraint
/// that requires `jsonb_typeof(arguments) = 'array'`, so writing the full
/// object there would fail. The `intent_args_serialise_as_a_json_array`
/// test pins this contract.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct IntentInstance {
    pub name: String,
    pub args: Vec<EvalValue>,
}

/// The result of proposing a transformation. Either the candidate state is
/// admissible (Accepted) or some predicate or invariant rejected it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outcome {
    Accepted {
        asserted_claims: Vec<ClaimInstance>,
        retracted_claims: Vec<ClaimInstance>,
        emitted_intents: Vec<IntentInstance>,
        candidate_state: State,
    },
    Rejected {
        reason: String,
    },
}

enum StmtOutcome {
    Continue,
    Rejected(String),
}

/// Propose a transformation against a pre-state. Stages asserts/retracts/
/// intents, builds the candidate state, evaluates every invariant against
/// that candidate state, and returns Accepted iff all invariants hold.
///
/// No PostgreSQL, no audit, no outbox — that's a later concern. This
/// proves the semantic loop: transformation proposes, invariants decide.
pub fn propose(
    transformation: &Transformation,
    args: Vec<EvalValue>,
    pre_state: &State,
    invariants: &[Invariant],
) -> Result<Outcome, EvalError> {
    if args.len() != transformation.parameters.len() {
        return Err(EvalError::TypeMismatch(format!(
            "transformation `{}` expects {} args, got {}",
            transformation.name,
            transformation.parameters.len(),
            args.len(),
        )));
    }

    let mut bindings = Bindings::new();
    for (name, val) in transformation.parameters.iter().zip(args) {
        bindings.insert(name.clone(), val);
    }

    let mut asserted: Vec<ClaimInstance> = vec![];
    let mut retracted: Vec<ClaimInstance> = vec![];
    let mut emitted: Vec<IntentInstance> = vec![];

    for stmt in &transformation.body {
        match execute_stmt(
            stmt,
            pre_state,
            &mut bindings,
            &mut asserted,
            &mut retracted,
            &mut emitted,
        )? {
            StmtOutcome::Continue => {}
            StmtOutcome::Rejected(reason) => return Ok(Outcome::Rejected { reason }),
        }
    }

    let candidate = build_candidate_state(pre_state, &asserted, &retracted);

    for inv in invariants {
        if !eval_invariant(inv, &candidate)? {
            return Ok(Outcome::Rejected {
                reason: format!("invariant `{}` violated", inv.name),
            });
        }
    }

    Ok(Outcome::Accepted {
        asserted_claims: asserted,
        retracted_claims: retracted,
        emitted_intents: emitted,
        candidate_state: candidate,
    })
}

fn execute_stmt(
    stmt: &Stmt,
    pre_state: &State,
    bindings: &mut Bindings,
    asserted: &mut Vec<ClaimInstance>,
    retracted: &mut Vec<ClaimInstance>,
    emitted: &mut Vec<IntentInstance>,
) -> Result<StmtOutcome, EvalError> {
    match stmt {
        Stmt::Require(expr) => {
            let matches = find_matches(expr, pre_state, bindings)?;
            if matches.is_empty() {
                Ok(StmtOutcome::Rejected(
                    "require failed: predicate did not hold over pre-state".to_string(),
                ))
            } else {
                Ok(StmtOutcome::Continue)
            }
        }
        Stmt::Let { name, value } => {
            let v = eval_value(value, pre_state, bindings)?;
            bindings.insert(name.clone(), v);
            Ok(StmtOutcome::Continue)
        }
        Stmt::LetNewSubject { name } => {
            let id = uuid::Uuid::now_v7().to_string();
            bindings.insert(name.clone(), EvalValue::Subject(id));
            Ok(StmtOutcome::Continue)
        }
        Stmt::Assert(claim) => {
            asserted.push(resolve_claim(claim, bindings)?);
            Ok(StmtOutcome::Continue)
        }
        Stmt::Retract { predicate, args } => {
            for claim in &pre_state.claims {
                if claim.predicate != *predicate || claim.args.len() != args.len() {
                    continue;
                }
                if unify_args(args, &claim.args, bindings).is_some() {
                    retracted.push(claim.clone());
                }
            }
            Ok(StmtOutcome::Continue)
        }
        Stmt::For {
            binding,
            collection,
            body,
        } => {
            let coll_val = eval_value(collection, pre_state, bindings)?;
            let items = match coll_val {
                EvalValue::Collection(v) => v,
                _ => return Err(EvalError::TypeMismatch("For expects a collection".into())),
            };
            for item in items {
                bindings.insert(binding.clone(), item);
                for inner in body {
                    match execute_stmt(inner, pre_state, bindings, asserted, retracted, emitted)? {
                        StmtOutcome::Continue => {}
                        StmtOutcome::Rejected(r) => return Ok(StmtOutcome::Rejected(r)),
                    }
                }
            }
            bindings.remove(binding);
            Ok(StmtOutcome::Continue)
        }
        Stmt::Emit(intent) => {
            emitted.push(resolve_intent(intent, bindings)?);
            Ok(StmtOutcome::Continue)
        }
    }
}

fn resolve_claim(claim: &Claim, bindings: &Bindings) -> Result<ClaimInstance, EvalError> {
    let mut args = Vec::with_capacity(claim.args.len());
    for t in &claim.args {
        if matches!(t, Term::Wildcard) {
            return Err(EvalError::TypeMismatch(
                "wildcard not allowed in assert/retract".into(),
            ));
        }
        args.push(resolve_term(t, bindings)?);
    }
    Ok(ClaimInstance {
        predicate: claim.predicate.clone(),
        args,
    })
}

fn resolve_intent(intent: &Intent, bindings: &Bindings) -> Result<IntentInstance, EvalError> {
    let mut args = Vec::with_capacity(intent.args.len());
    for t in &intent.args {
        if matches!(t, Term::Wildcard) {
            return Err(EvalError::TypeMismatch(
                "wildcard not allowed in emit".into(),
            ));
        }
        args.push(resolve_term(t, bindings)?);
    }
    Ok(IntentInstance {
        name: intent.name.clone(),
        args,
    })
}

fn build_candidate_state(
    pre: &State,
    asserted: &[ClaimInstance],
    retracted: &[ClaimInstance],
) -> State {
    let mut claims = pre.claims.clone();
    claims.retain(|f| !retracted.iter().any(|r| r == f));
    for a in asserted {
        if !claims.iter().any(|f| f == a) {
            claims.push(a.clone());
        }
    }
    State { claims }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    //! Kernel-internal unit tests for IR literals.
    //!
    //! Tests that depend on private items (`unify_args`, `resolve_term`,
    //! `Bindings`) live here. Tests that exercise the public surface —
    //! example chains, codec round-trips, IR-shape assertions — live in
    //! the `tests/` directory as integration tests, one file per example
    //! plus `tests/codec.rs` and the shared `tests/common/mod.rs`.

    use super::*;

    #[test]
    fn decimal_literal_constructs() {
        let v = Value::Decimal("1250.75".to_string());
        assert_eq!(
            Term::Literal(v),
            Term::Literal(Value::Decimal("1250.75".to_string()))
        );
    }

    #[test]
    fn subject_literal_constructs_and_resolves() {
        let v = Value::Subject("bank_debt_service".to_string());
        assert_eq!(
            Term::Literal(v),
            Term::Literal(Value::Subject("bank_debt_service".to_string()))
        );
        let resolved = resolve_term(
            &Term::Literal(Value::Subject("bank_debt_service".to_string())),
            &Bindings::new(),
        )
        .unwrap();
        assert_eq!(
            resolved,
            EvalValue::Subject("bank_debt_service".to_string())
        );
    }

    #[test]
    fn subject_literal_unifies_with_matching_subject_arg() {
        let pattern = vec![Term::Literal(Value::Subject("p1".to_string()))];
        let value = vec![EvalValue::Subject("p1".to_string())];
        assert!(unify_args(&pattern, &value, &Bindings::new()).is_some());

        let mismatch = vec![EvalValue::Subject("p2".to_string())];
        assert!(unify_args(&pattern, &mismatch, &Bindings::new()).is_none());

        let wrong_kind = vec![EvalValue::Decimal(Decimal::new(1, 0))];
        assert!(unify_args(&pattern, &wrong_kind, &Bindings::new()).is_none());
    }
}
