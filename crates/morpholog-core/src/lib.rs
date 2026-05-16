//! Morpholog v0 IR and in-memory evaluator.
//!
//! The IR defines the minimum data types needed to represent invariants
//! and transformations as Rust data. The evaluator evaluates an invariant
//! against an in-memory `State` (a set of grounded claims) and returns
//! whether it holds. No PostgreSQL, no transformation execution yet.

use rust_decimal::Decimal;
use std::collections::HashMap;
use std::str::FromStr;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Invariant {
    pub name: String,
    pub version: u32,
    pub body: Expr,
}

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

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Term {
    Var(String),
    Wildcard,
    Literal(Value),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Value {
    /// Arbitrary-precision decimal stored as its exact source string.
    /// Parsing into a numeric type is the evaluator's concern, not the IR's.
    Decimal(String),
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

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Intent {
    pub name: String,
    pub args: Vec<Term>,
}

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
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EvalValue {
    Decimal(Decimal),
    Subject(String),
    Bool(bool),
    Collection(Vec<EvalValue>),
}

/// A grounded claim: all args are values, no variables or wildcards.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClaimInstance {
    pub predicate: String,
    pub args: Vec<EvalValue>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct State {
    pub claims: Vec<ClaimInstance>,
}

pub type Bindings = HashMap<String, EvalValue>;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EvalError {
    UnboundVariable(String),
    TypeMismatch(String),
    NotPredicate,
    NotValue,
    ValueOfZeroMatches(String),
    ValueOfMultipleMatches(String),
}

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
    }
}

// ===========================================================================
// Transformation execution (in-memory)
// ===========================================================================

/// A resolved intent: all args are values, ready to be enqueued in an outbox.
#[derive(Debug, Clone, PartialEq, Eq)]
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
mod tests {
    use super::*;

    fn net_settlement_has_lines() -> Invariant {
        Invariant {
            name: "net_settlement_has_lines".to_string(),
            version: 1,
            body: Expr::Implies {
                left: Box::new(Expr::Claim {
                    predicate: "NetSettlement".to_string(),
                    args: vec![
                        Term::Var("net".to_string()),
                        Term::Wildcard,
                        Term::Wildcard,
                        Term::Wildcard,
                    ],
                }),
                right: Box::new(Expr::Exists {
                    binding: "line".to_string(),
                    body: Box::new(Expr::Claim {
                        predicate: "SettlementLine".to_string(),
                        args: vec![
                            Term::Var("line".to_string()),
                            Term::Var("net".to_string()),
                            Term::Wildcard,
                        ],
                    }),
                }),
            },
        }
    }

    fn no_double_netting() -> Invariant {
        Invariant {
            name: "no_double_netting".to_string(),
            version: 1,
            body: Expr::Implies {
                left: Box::new(Expr::Claim {
                    predicate: "SettlementLine".to_string(),
                    args: vec![
                        Term::Var("line".to_string()),
                        Term::Var("net".to_string()),
                        Term::Wildcard,
                    ],
                }),
                right: Box::new(Expr::Not(Box::new(Expr::Exists {
                    binding: "other".to_string(),
                    body: Box::new(Expr::And(vec![
                        Expr::Claim {
                            predicate: "SettlementLine".to_string(),
                            args: vec![
                                Term::Var("line".to_string()),
                                Term::Var("other".to_string()),
                                Term::Wildcard,
                            ],
                        },
                        Expr::Neq(Term::Var("other".to_string()), Term::Var("net".to_string())),
                    ])),
                }))),
            },
        }
    }

    #[test]
    fn invariant_round_trips_through_equality() {
        assert_eq!(net_settlement_has_lines(), net_settlement_has_lines());
    }

    #[test]
    fn invariant_has_expected_top_level_shape() {
        let inv = net_settlement_has_lines();
        assert_eq!(inv.name, "net_settlement_has_lines");
        assert_eq!(inv.version, 1);
        assert!(matches!(inv.body, Expr::Implies { .. }));
    }

    #[test]
    fn no_double_netting_round_trips() {
        assert_eq!(no_double_netting(), no_double_netting());
    }

    #[test]
    fn no_double_netting_has_expected_shape() {
        let inv = no_double_netting();
        assert_eq!(inv.name, "no_double_netting");
        assert_eq!(inv.version, 1);

        let Expr::Implies { right, .. } = &inv.body else {
            panic!("expected Implies at top level");
        };
        let Expr::Not(inner) = right.as_ref() else {
            panic!("expected Not on right side of Implies");
        };
        let Expr::Exists { body, .. } = inner.as_ref() else {
            panic!("expected Exists inside Not");
        };
        assert!(matches!(body.as_ref(), Expr::And(_)));
    }

    fn net_amount_equals_lines() -> Invariant {
        Invariant {
            name: "net_amount_equals_lines".to_string(),
            version: 1,
            body: Expr::Implies {
                left: Box::new(Expr::Claim {
                    predicate: "NetSettlement".to_string(),
                    args: vec![
                        Term::Var("net".to_string()),
                        Term::Wildcard,
                        Term::Wildcard,
                        Term::Var("amount".to_string()),
                    ],
                }),
                right: Box::new(Expr::Eq(
                    Box::new(Expr::Term(Term::Var("amount".to_string()))),
                    Box::new(Expr::Sum {
                        value: Term::Var("x".to_string()),
                        binding: "x".to_string(),
                        body: Box::new(Expr::Claim {
                            predicate: "SettlementLine".to_string(),
                            args: vec![
                                Term::Wildcard,
                                Term::Var("net".to_string()),
                                Term::Var("x".to_string()),
                            ],
                        }),
                    }),
                )),
            },
        }
    }

    #[test]
    fn net_amount_equals_lines_round_trips() {
        assert_eq!(net_amount_equals_lines(), net_amount_equals_lines());
    }

    #[test]
    fn net_amount_equals_lines_has_expected_shape() {
        let inv = net_amount_equals_lines();
        assert_eq!(inv.name, "net_amount_equals_lines");
        assert_eq!(inv.version, 1);

        let Expr::Implies { right, .. } = &inv.body else {
            panic!("expected Implies at top level");
        };
        let Expr::Eq(lhs, rhs) = right.as_ref() else {
            panic!("expected Eq on right side of Implies");
        };
        assert!(matches!(lhs.as_ref(), Expr::Term(Term::Var(_))));
        assert!(matches!(rhs.as_ref(), Expr::Sum { .. }));
    }

    #[test]
    fn decimal_literal_constructs() {
        let v = Value::Decimal("1250.75".to_string());
        assert_eq!(
            Term::Literal(v),
            Term::Literal(Value::Decimal("1250.75".to_string()))
        );
    }

    fn create_net_settlement() -> Transformation {
        let var = |s: &str| Term::Var(s.to_string());

        Transformation {
            name: "create_net_settlement".to_string(),
            parameters: vec![
                "party_a".to_string(),
                "party_b".to_string(),
                "lines".to_string(),
            ],
            body: vec![
                // require forall { line | line in lines }:
                //   ApprovedSettlementLine(line)
                //   and Between(line, party_a, party_b)
                //   and not Netted(line)
                Stmt::Require(Expr::Forall {
                    binding: "line".to_string(),
                    source: Box::new(Expr::In(var("line"), var("lines"))),
                    body: Box::new(Expr::And(vec![
                        Expr::Claim {
                            predicate: "ApprovedSettlementLine".to_string(),
                            args: vec![var("line")],
                        },
                        Expr::Claim {
                            predicate: "Between".to_string(),
                            args: vec![var("line"), var("party_a"), var("party_b")],
                        },
                        Expr::Not(Box::new(Expr::Claim {
                            predicate: "Netted".to_string(),
                            args: vec![var("line")],
                        })),
                    ])),
                }),
                // let net = new Subject()
                Stmt::LetNewSubject {
                    name: "net".to_string(),
                },
                // let amount = sum { x | line in lines, LineAmount(line, x) }
                Stmt::Let {
                    name: "amount".to_string(),
                    value: Expr::Sum {
                        value: var("x"),
                        binding: "x".to_string(),
                        body: Box::new(Expr::And(vec![
                            Expr::In(var("line"), var("lines")),
                            Expr::Claim {
                                predicate: "LineAmount".to_string(),
                                args: vec![var("line"), var("x")],
                            },
                        ])),
                    },
                },
                // assert NetSettlement(net, party_a, party_b, amount)
                Stmt::Assert(Claim {
                    predicate: "NetSettlement".to_string(),
                    args: vec![var("net"), var("party_a"), var("party_b"), var("amount")],
                }),
                // for line in lines:
                //   let amt = value LineAmount(line, _)
                //   assert SettlementLine(line, net, amt)
                //   assert Netted(line)
                Stmt::For {
                    binding: "line".to_string(),
                    collection: Expr::Term(var("lines")),
                    body: vec![
                        Stmt::Let {
                            name: "amt".to_string(),
                            value: Expr::ValueOf {
                                predicate: "LineAmount".to_string(),
                                args: vec![var("line"), Term::Wildcard],
                                default: None,
                            },
                        },
                        Stmt::Assert(Claim {
                            predicate: "SettlementLine".to_string(),
                            args: vec![var("line"), var("net"), var("amt")],
                        }),
                        Stmt::Assert(Claim {
                            predicate: "Netted".to_string(),
                            args: vec![var("line")],
                        }),
                    ],
                },
                // emit NetSettlementCreated(net)
                Stmt::Emit(Intent {
                    name: "NetSettlementCreated".to_string(),
                    args: vec![var("net")],
                }),
            ],
        }
    }

    #[test]
    fn create_net_settlement_round_trips() {
        assert_eq!(create_net_settlement(), create_net_settlement());
    }

    #[test]
    fn create_net_settlement_has_expected_shape() {
        let t = create_net_settlement();
        assert_eq!(t.name, "create_net_settlement");
        assert_eq!(t.parameters, vec!["party_a", "party_b", "lines"]);
        assert_eq!(t.body.len(), 6);
        assert!(matches!(t.body[0], Stmt::Require(_)));
        assert!(matches!(t.body[1], Stmt::LetNewSubject { .. }));
        assert!(matches!(t.body[2], Stmt::Let { .. }));
        assert!(matches!(t.body[3], Stmt::Assert(_)));
        assert!(matches!(t.body[4], Stmt::For { .. }));
        assert!(matches!(t.body[5], Stmt::Emit(_)));
    }

    #[test]
    fn for_body_contains_let_and_two_asserts() {
        let t = create_net_settlement();
        let Stmt::For { body, .. } = &t.body[4] else {
            panic!("body[4] should be Stmt::For");
        };
        assert_eq!(body.len(), 3);
        assert!(matches!(body[0], Stmt::Let { .. }));
        assert!(matches!(body[1], Stmt::Assert(_)));
        assert!(matches!(body[2], Stmt::Assert(_)));
    }

    // -----------------------------------------------------------------
    // Evaluator tests
    // -----------------------------------------------------------------

    fn dec(n: i64) -> EvalValue {
        EvalValue::Decimal(Decimal::new(n, 0))
    }

    fn subj(s: &str) -> EvalValue {
        EvalValue::Subject(s.to_string())
    }

    fn netting_state(amount: i64) -> State {
        State {
            claims: vec![
                ClaimInstance {
                    predicate: "NetSettlement".to_string(),
                    args: vec![subj("net1"), subj("party_a"), subj("party_b"), dec(amount)],
                },
                ClaimInstance {
                    predicate: "SettlementLine".to_string(),
                    args: vec![subj("l1"), subj("net1"), dec(60)],
                },
                ClaimInstance {
                    predicate: "SettlementLine".to_string(),
                    args: vec![subj("l2"), subj("net1"), dec(40)],
                },
            ],
        }
    }

    #[test]
    fn net_amount_equals_lines_holds_when_amount_matches() {
        let state = netting_state(100);
        let inv = net_amount_equals_lines();
        let result = eval_invariant(&inv, &state).expect("evaluation should not error");
        assert!(result, "invariant should hold for amount = 60 + 40 = 100");
    }

    #[test]
    fn net_amount_equals_lines_fails_when_amount_mismatches() {
        let state = netting_state(101);
        let inv = net_amount_equals_lines();
        let result = eval_invariant(&inv, &state).expect("evaluation should not error");
        assert!(
            !result,
            "invariant should fail for amount = 101 vs lines = 100"
        );
    }

    // -----------------------------------------------------------------
    // Transformation execution tests
    // -----------------------------------------------------------------

    /// Build a pre-state with l1 (60) and l2 (40), both approved, between
    /// party_a and party_b, neither netted. `extra` lets a test add extra
    /// claims (e.g. a pre-existing SettlementLine to provoke an invariant
    /// violation).
    fn netting_pre_state(extra: Vec<ClaimInstance>) -> State {
        let mut claims = vec![
            ClaimInstance {
                predicate: "ApprovedSettlementLine".to_string(),
                args: vec![subj("l1")],
            },
            ClaimInstance {
                predicate: "Between".to_string(),
                args: vec![subj("l1"), subj("party_a"), subj("party_b")],
            },
            ClaimInstance {
                predicate: "LineAmount".to_string(),
                args: vec![subj("l1"), dec(60)],
            },
            ClaimInstance {
                predicate: "ApprovedSettlementLine".to_string(),
                args: vec![subj("l2")],
            },
            ClaimInstance {
                predicate: "Between".to_string(),
                args: vec![subj("l2"), subj("party_a"), subj("party_b")],
            },
            ClaimInstance {
                predicate: "LineAmount".to_string(),
                args: vec![subj("l2"), dec(40)],
            },
        ];
        claims.extend(extra);
        State { claims }
    }

    fn all_invariants() -> Vec<Invariant> {
        vec![
            net_settlement_has_lines(),
            net_amount_equals_lines(),
            no_double_netting(),
        ]
    }

    fn netting_args() -> Vec<EvalValue> {
        vec![
            subj("party_a"),
            subj("party_b"),
            EvalValue::Collection(vec![subj("l1"), subj("l2")]),
        ]
    }

    #[test]
    fn propose_accepts_well_formed_netting() {
        let pre = netting_pre_state(vec![]);
        let t = create_net_settlement();
        let outcome =
            propose(&t, netting_args(), &pre, &all_invariants()).expect("propose should not error");

        let Outcome::Accepted {
            asserted_claims,
            retracted_claims,
            emitted_intents,
            ..
        } = outcome
        else {
            panic!("expected Accepted, got {outcome:?}");
        };

        // Five asserts: NetSettlement + (SettlementLine + Netted) * 2
        assert_eq!(asserted_claims.len(), 5);
        assert_eq!(retracted_claims.len(), 0);
        assert_eq!(emitted_intents.len(), 1);
        assert_eq!(emitted_intents[0].name, "NetSettlementCreated");

        // Exactly one NetSettlement assertion with the expected total.
        let net_settlement = asserted_claims
            .iter()
            .find(|f| f.predicate == "NetSettlement")
            .expect("should have asserted a NetSettlement");
        assert_eq!(net_settlement.args[3], dec(100));
    }

    #[test]
    fn propose_rejects_when_line_already_netted() {
        let extra = vec![ClaimInstance {
            predicate: "Netted".to_string(),
            args: vec![subj("l1")],
        }];
        let pre = netting_pre_state(extra);
        let t = create_net_settlement();
        let outcome =
            propose(&t, netting_args(), &pre, &all_invariants()).expect("propose should not error");

        let Outcome::Rejected { reason } = outcome else {
            panic!("expected Rejected, got {outcome:?}");
        };
        assert!(reason.contains("require failed"), "got: {reason}");
    }

    // -----------------------------------------------------------------
    // Revenue restatement (Example 2) — IR data
    // -----------------------------------------------------------------

    fn current_recognition_matches_current_verification() -> Invariant {
        let var = |s: &str| Term::Var(s.to_string());
        Invariant {
            name: "current_recognition_matches_current_verification".to_string(),
            version: 1,
            body: Expr::Implies {
                left: Box::new(Expr::And(vec![
                    Expr::Claim {
                        predicate: "CurrentBankRecognition".to_string(),
                        args: vec![var("asset"), var("period"), var("r")],
                    },
                    Expr::Claim {
                        predicate: "BankRecognisedRevenue".to_string(),
                        args: vec![var("asset"), var("period"), var("amount"), var("r")],
                    },
                ])),
                right: Box::new(Expr::Exists {
                    binding: "v".to_string(),
                    body: Box::new(Expr::And(vec![
                        Expr::Claim {
                            predicate: "IndependentlyVerifiedRevenue".to_string(),
                            args: vec![var("asset"), var("period"), var("amount"), var("v")],
                        },
                        Expr::Not(Box::new(Expr::Exists {
                            binding: "newer".to_string(),
                            body: Box::new(Expr::Claim {
                                predicate: "Supersedes".to_string(),
                                args: vec![var("newer"), var("v")],
                            }),
                        })),
                    ])),
                }),
            },
        }
    }

    fn at_most_one_current_recognition_per_asset_period() -> Invariant {
        let var = |s: &str| Term::Var(s.to_string());
        Invariant {
            name: "at_most_one_current_recognition_per_asset_period".to_string(),
            version: 1,
            body: Expr::Implies {
                left: Box::new(Expr::And(vec![
                    Expr::Claim {
                        predicate: "CurrentBankRecognition".to_string(),
                        args: vec![var("asset"), var("period"), var("a")],
                    },
                    Expr::Claim {
                        predicate: "CurrentBankRecognition".to_string(),
                        args: vec![var("asset"), var("period"), var("b")],
                    },
                ])),
                right: Box::new(Expr::Eq(
                    Box::new(Expr::Term(var("a"))),
                    Box::new(Expr::Term(var("b"))),
                )),
            },
        }
    }

    fn at_most_one_direct_successor() -> Invariant {
        let var = |s: &str| Term::Var(s.to_string());
        Invariant {
            name: "at_most_one_direct_successor".to_string(),
            version: 1,
            body: Expr::Implies {
                left: Box::new(Expr::And(vec![
                    Expr::Claim {
                        predicate: "Supersedes".to_string(),
                        args: vec![var("new_a"), var("old")],
                    },
                    Expr::Claim {
                        predicate: "Supersedes".to_string(),
                        args: vec![var("new_b"), var("old")],
                    },
                ])),
                right: Box::new(Expr::Eq(
                    Box::new(Expr::Term(var("new_a"))),
                    Box::new(Expr::Term(var("new_b"))),
                )),
            },
        }
    }

    fn correct_independent_verification() -> Transformation {
        let var = |s: &str| Term::Var(s.to_string());
        Transformation {
            name: "correct_independent_verification".to_string(),
            parameters: vec![
                "asset".to_string(),
                "period".to_string(),
                "new_amount".to_string(),
                "new_verification_id".to_string(),
                "prior_verification_id".to_string(),
            ],
            body: vec![
                // The prior verification must exist.
                Stmt::Require(Expr::Claim {
                    predicate: "IndependentlyVerifiedRevenue".to_string(),
                    args: vec![
                        var("asset"),
                        var("period"),
                        Term::Wildcard,
                        var("prior_verification_id"),
                    ],
                }),
                // The prior verification must not already be superseded.
                Stmt::Require(Expr::Not(Box::new(Expr::Claim {
                    predicate: "Supersedes".to_string(),
                    args: vec![Term::Wildcard, var("prior_verification_id")],
                }))),
                // Admit the new verification.
                Stmt::Assert(Claim {
                    predicate: "IndependentlyVerifiedRevenue".to_string(),
                    args: vec![
                        var("asset"),
                        var("period"),
                        var("new_amount"),
                        var("new_verification_id"),
                    ],
                }),
                // Record the supersession.
                Stmt::Assert(Claim {
                    predicate: "Supersedes".to_string(),
                    args: vec![var("new_verification_id"), var("prior_verification_id")],
                }),
                // Invalidate any dependent current bank recognition.
                // History (BankRecognisedRevenue) is preserved; the pointer moves.
                Stmt::Retract {
                    predicate: "CurrentBankRecognition".to_string(),
                    args: vec![var("asset"), var("period"), Term::Wildcard],
                },
                Stmt::Emit(Intent {
                    name: "VerificationCorrected".to_string(),
                    args: vec![var("new_verification_id"), var("prior_verification_id")],
                }),
            ],
        }
    }

    #[test]
    fn correct_independent_verification_retracts_dependent_current_pointer() {
        let invariants = vec![
            current_recognition_matches_current_verification(),
            at_most_one_current_recognition_per_asset_period(),
            at_most_one_direct_successor(),
        ];

        let pre = State {
            claims: vec![
                claim_instance(
                    "IndependentlyVerifiedRevenue",
                    &[subj("asset_a"), subj("p_2026_04"), dec(92), subj("ver_001")],
                ),
                claim_instance(
                    "BankRecognisedRevenue",
                    &[subj("asset_a"), subj("p_2026_04"), dec(92), subj("rec_001")],
                ),
                claim_instance(
                    "CurrentBankRecognition",
                    &[subj("asset_a"), subj("p_2026_04"), subj("rec_001")],
                ),
            ],
        };

        let args = vec![
            subj("asset_a"),
            subj("p_2026_04"),
            dec(91),
            subj("ver_002"),
            subj("ver_001"),
        ];

        let outcome = propose(&correct_independent_verification(), args, &pre, &invariants)
            .expect("propose should not error");

        let Outcome::Accepted {
            candidate_state,
            asserted_claims,
            retracted_claims,
            emitted_intents,
            ..
        } = outcome
        else {
            panic!("expected Accepted, got {outcome:?}");
        };

        assert_eq!(
            asserted_claims.len(),
            2,
            "should assert new IV + Supersedes"
        );
        assert_eq!(
            retracted_claims.len(),
            1,
            "should retract the current pointer"
        );
        assert_eq!(retracted_claims[0].predicate, "CurrentBankRecognition");
        assert_eq!(emitted_intents.len(), 1);
        assert_eq!(emitted_intents[0].name, "VerificationCorrected");

        // Historical BankRecognisedRevenue must still be in candidate state.
        assert!(
            candidate_state.claims.iter().any(|c| {
                c.predicate == "BankRecognisedRevenue"
                    && c.args == vec![subj("asset_a"), subj("p_2026_04"), dec(92), subj("rec_001")]
            }),
            "historical BankRecognisedRevenue must be preserved"
        );

        // CurrentBankRecognition must be gone.
        assert!(
            !candidate_state
                .claims
                .iter()
                .any(|c| c.predicate == "CurrentBankRecognition"),
            "current bank recognition pointer must be retracted"
        );

        // New verification must be present.
        assert!(
            candidate_state.claims.iter().any(|c| {
                c.predicate == "IndependentlyVerifiedRevenue"
                    && c.args == vec![subj("asset_a"), subj("p_2026_04"), dec(91), subj("ver_002")]
            }),
            "new IndependentlyVerifiedRevenue must be present"
        );

        // Supersession recorded.
        assert!(
            candidate_state.claims.iter().any(|c| {
                c.predicate == "Supersedes" && c.args == vec![subj("ver_002"), subj("ver_001")]
            }),
            "Supersedes(ver_002, ver_001) must be recorded"
        );
    }

    fn claim_instance(pred: &str, args: &[EvalValue]) -> ClaimInstance {
        ClaimInstance {
            predicate: pred.to_string(),
            args: args.to_vec(),
        }
    }

    fn admit_independent_verification() -> Transformation {
        let var = |s: &str| Term::Var(s.to_string());
        Transformation {
            name: "admit_independent_verification".to_string(),
            parameters: vec![
                "asset".to_string(),
                "period".to_string(),
                "amount".to_string(),
                "verification_id".to_string(),
            ],
            body: vec![
                Stmt::Assert(Claim {
                    predicate: "IndependentlyVerifiedRevenue".to_string(),
                    args: vec![
                        var("asset"),
                        var("period"),
                        var("amount"),
                        var("verification_id"),
                    ],
                }),
                Stmt::Emit(Intent {
                    name: "IndependentVerificationAdmitted".to_string(),
                    args: vec![var("verification_id")],
                }),
            ],
        }
    }

    fn recognise_bank_revenue() -> Transformation {
        let var = |s: &str| Term::Var(s.to_string());
        Transformation {
            name: "recognise_bank_revenue".to_string(),
            parameters: vec![
                "asset".to_string(),
                "period".to_string(),
                "amount".to_string(),
                "recognition_id".to_string(),
            ],
            body: vec![
                // No existing current pointer for this (asset, period).
                Stmt::Require(Expr::Not(Box::new(Expr::Claim {
                    predicate: "CurrentBankRecognition".to_string(),
                    args: vec![var("asset"), var("period"), Term::Wildcard],
                }))),
                Stmt::Assert(Claim {
                    predicate: "BankRecognisedRevenue".to_string(),
                    args: vec![
                        var("asset"),
                        var("period"),
                        var("amount"),
                        var("recognition_id"),
                    ],
                }),
                Stmt::Assert(Claim {
                    predicate: "CurrentBankRecognition".to_string(),
                    args: vec![var("asset"), var("period"), var("recognition_id")],
                }),
                Stmt::Emit(Intent {
                    name: "BankRevenueRecognised".to_string(),
                    args: vec![var("recognition_id")],
                }),
            ],
        }
    }

    fn restate_bank_revenue() -> Transformation {
        let var = |s: &str| Term::Var(s.to_string());
        Transformation {
            name: "restate_bank_revenue".to_string(),
            parameters: vec![
                "asset".to_string(),
                "period".to_string(),
                "new_amount".to_string(),
                "new_recognition_id".to_string(),
                "prior_recognition_id".to_string(),
            ],
            body: vec![
                // The prior recognition must exist (otherwise there is
                // nothing to restate and Supersedes would be meaningless).
                Stmt::Require(Expr::Claim {
                    predicate: "BankRecognisedRevenue".to_string(),
                    args: vec![
                        var("asset"),
                        var("period"),
                        Term::Wildcard,
                        var("prior_recognition_id"),
                    ],
                }),
                // Retract any current pointer for (asset, period). Idempotent
                // if none exists (the verifier's correction may already have
                // retracted it).
                Stmt::Retract {
                    predicate: "CurrentBankRecognition".to_string(),
                    args: vec![var("asset"), var("period"), Term::Wildcard],
                },
                Stmt::Assert(Claim {
                    predicate: "BankRecognisedRevenue".to_string(),
                    args: vec![
                        var("asset"),
                        var("period"),
                        var("new_amount"),
                        var("new_recognition_id"),
                    ],
                }),
                Stmt::Assert(Claim {
                    predicate: "CurrentBankRecognition".to_string(),
                    args: vec![var("asset"), var("period"), var("new_recognition_id")],
                }),
                Stmt::Assert(Claim {
                    predicate: "Supersedes".to_string(),
                    args: vec![var("new_recognition_id"), var("prior_recognition_id")],
                }),
                Stmt::Emit(Intent {
                    name: "BankRevenueRestated".to_string(),
                    args: vec![var("new_recognition_id"), var("prior_recognition_id")],
                }),
            ],
        }
    }

    fn must_accept(
        t: &Transformation,
        args: Vec<EvalValue>,
        pre: State,
        invariants: &[Invariant],
    ) -> State {
        match propose(t, args, &pre, invariants).expect("propose should not error") {
            Outcome::Accepted {
                candidate_state, ..
            } => candidate_state,
            Outcome::Rejected { reason } => {
                panic!(
                    "expected Accepted from `{}`, got Rejected: {reason}",
                    t.name
                )
            }
        }
    }

    fn has_claim(state: &State, predicate: &str, args: &[EvalValue]) -> bool {
        state
            .claims
            .iter()
            .any(|c| c.predicate == predicate && c.args == args)
    }

    #[test]
    fn full_restatement_chain_preserves_history_and_updates_pointer() {
        let invariants = vec![
            current_recognition_matches_current_verification(),
            at_most_one_current_recognition_per_asset_period(),
            at_most_one_direct_successor(),
        ];

        let a = subj("asset_a");
        let p = subj("p_2026_04");

        // 1. Admit initial independent verification at 92.
        let s1 = must_accept(
            &admit_independent_verification(),
            vec![a.clone(), p.clone(), dec(92), subj("ver_001")],
            State::default(),
            &invariants,
        );

        // 2. Bank recognises 92, rec_001. I1 holds against the current verification.
        let s2 = must_accept(
            &recognise_bank_revenue(),
            vec![a.clone(), p.clone(), dec(92), subj("rec_001")],
            s1,
            &invariants,
        );

        // 3. Verifier corrects to 91 (ver_002 supersedes ver_001). The
        // dependent CurrentBankRecognition is retracted as part of the
        // verifier's transformation body, so I1 is vacuously satisfied
        // (no current pointer remains).
        let s3 = must_accept(
            &correct_independent_verification(),
            vec![
                a.clone(),
                p.clone(),
                dec(91),
                subj("ver_002"),
                subj("ver_001"),
            ],
            s2,
            &invariants,
        );

        // 4. Bank restates to 91 with a new recognition_id. New current
        // pointer; new Supersedes link.
        let s4 = must_accept(
            &restate_bank_revenue(),
            vec![
                a.clone(),
                p.clone(),
                dec(91),
                subj("rec_002"),
                subj("rec_001"),
            ],
            s3,
            &invariants,
        );

        // Final state: 2 IV + 2 BR + 2 Supersedes + 1 Current = 7 claims.
        assert_eq!(s4.claims.len(), 7);

        assert!(has_claim(
            &s4,
            "IndependentlyVerifiedRevenue",
            &[a.clone(), p.clone(), dec(92), subj("ver_001")],
        ));
        assert!(has_claim(
            &s4,
            "IndependentlyVerifiedRevenue",
            &[a.clone(), p.clone(), dec(91), subj("ver_002")],
        ));
        assert!(has_claim(
            &s4,
            "Supersedes",
            &[subj("ver_002"), subj("ver_001")],
        ));

        assert!(
            has_claim(
                &s4,
                "BankRecognisedRevenue",
                &[a.clone(), p.clone(), dec(92), subj("rec_001")],
            ),
            "historical BR(92, rec_001) must be preserved"
        );
        assert!(has_claim(
            &s4,
            "BankRecognisedRevenue",
            &[a.clone(), p.clone(), dec(91), subj("rec_002")],
        ));
        assert!(has_claim(
            &s4,
            "Supersedes",
            &[subj("rec_002"), subj("rec_001")],
        ));

        assert!(
            has_claim(
                &s4,
                "CurrentBankRecognition",
                &[a.clone(), p.clone(), subj("rec_002")],
            ),
            "current pointer must be rec_002"
        );
        assert!(
            !has_claim(
                &s4,
                "CurrentBankRecognition",
                &[a.clone(), p.clone(), subj("rec_001")],
            ),
            "old current pointer must be retracted"
        );
    }

    #[test]
    fn propose_rejects_when_candidate_state_violates_no_double_netting() {
        // l1 already participates in an older settlement, but Netted(l1)
        // is missing from pre-state (inconsistent legacy data). The require
        // check passes, the transformation stages a second SettlementLine
        // for l1, and the invariant catches it on the candidate state.
        let extra = vec![ClaimInstance {
            predicate: "SettlementLine".to_string(),
            args: vec![subj("l1"), subj("old_net"), dec(60)],
        }];
        let pre = netting_pre_state(extra);
        let t = create_net_settlement();
        let outcome =
            propose(&t, netting_args(), &pre, &all_invariants()).expect("propose should not error");

        let Outcome::Rejected { reason } = outcome else {
            panic!("expected Rejected, got {outcome:?}");
        };
        assert!(reason.contains("no_double_netting"), "got: {reason}");
    }
}
