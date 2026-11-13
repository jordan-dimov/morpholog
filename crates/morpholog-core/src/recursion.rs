//! The shape a condition must have to call itself.
//!
//! A recursive condition means the least set of argument tuples that
//! satisfies its body. The rules here keep that set, and the questions
//! the evaluator asks while finding it, inside the values the admitted
//! claims already hold.

use std::collections::BTreeSet;
use std::fmt;

use crate::fold::any_prop_node;
use crate::ir::{Definition, DefinitionName, Prop, Term, Var};

/// Why a condition that calls itself is refused.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RecursionRefusal {
    /// The self-call sits under something other than the body's own
    /// `or` of `and`s (`not`, `exists`, `forall`, a `sum`, a nested
    /// `or`), where a larger table could mean fewer matches.
    Position,
    /// Two self-calls in one conjunction.
    SeveralSelfCalls,
    /// A branch takes this parameter from neither a claim pattern nor
    /// the self-call, so its values would not come from admitted claims.
    ParameterNotFromClaim { parameter: String },
    /// A self-call argument that is neither a parameter nor a variable a
    /// claim pattern earlier in the conjunction binds.
    ArgumentNotFromClaim { argument: String },
}

impl fmt::Display for RecursionRefusal {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            RecursionRefusal::Position => f.write_str(
                "the call to itself must be one of the conditions joined by `and` in a branch of the body's `or`",
            ),
            RecursionRefusal::SeveralSelfCalls => {
                f.write_str("a branch may call the definition once")
            }
            RecursionRefusal::ParameterNotFromClaim { parameter } => write!(
                f,
                "every branch must take `{parameter}` from a claim pattern or from the call to itself"
            ),
            RecursionRefusal::ArgumentNotFromClaim { argument } => write!(
                f,
                "it passes itself `{argument}`, which is neither a parameter nor a variable an earlier claim pattern in the branch binds"
            ),
        }
    }
}

/// Whether `prop` is a call to `name`.
fn is_call_to(prop: &Prop, name: &DefinitionName) -> bool {
    matches!(prop, Prop::Defined { name: called, .. } if called == name)
}

/// Whether the definition is a condition that calls itself.
pub(crate) fn calls_itself(def: &Definition) -> bool {
    def.body
        .as_prop()
        .is_some_and(|body| any_prop_node(body, &|p| is_call_to(p, &def.name)))
}

impl Definition {
    /// Whether this is a condition whose body calls it: one that means
    /// the least set of argument tuples satisfying its body.
    pub fn calls_itself(&self) -> bool {
        calls_itself(self)
    }
}

/// [`calls_itself`] for a validated programme, read off the only places
/// its shape rules allow the call: cheap enough to ask at every call.
pub(crate) fn asks_itself(def: &Definition) -> bool {
    fn within(prop: &Prop, name: &DefinitionName) -> bool {
        match prop {
            Prop::Or(operands) | Prop::And(operands) => operands.iter().any(|p| within(p, name)),
            other => is_call_to(other, name),
        }
    }
    def.body
        .as_prop()
        .is_some_and(|body| within(body, &def.name))
}

/// The body's branches: the operands of its `or`s, nested ones included.
fn branches(body: &Prop) -> Vec<&Prop> {
    match body {
        Prop::Or(operands) => operands.iter().flat_map(branches).collect(),
        other => vec![other],
    }
}

/// A branch's conditions in evaluation order: the operands of its
/// `and`s, nested ones included.
fn conjuncts(branch: &Prop) -> Vec<&Prop> {
    match branch {
        Prop::And(operands) => operands.iter().flat_map(conjuncts).collect(),
        other => vec![other],
    }
}

fn pattern_variables<'a>(args: &'a [Term], into: &mut BTreeSet<&'a Var>) {
    for arg in args {
        if let Term::Var(v) = arg {
            into.insert(v);
        }
    }
}

/// The first rule a self-calling condition breaks, if any.
pub(crate) fn refusal(def: &Definition) -> Option<RecursionRefusal> {
    let body = def.body.as_prop()?;
    for branch in branches(body) {
        let conditions = conjuncts(branch);
        let mut from_claims: BTreeSet<&Var> = BTreeSet::new();
        let mut bound_so_far: BTreeSet<&Var> = BTreeSet::new();
        let mut self_calls = 0;
        for condition in conditions {
            match condition {
                Prop::Claim { args, .. } => {
                    pattern_variables(args, &mut from_claims);
                    pattern_variables(args, &mut bound_so_far);
                }
                Prop::Defined { name, args } if name == &def.name => {
                    self_calls += 1;
                    if self_calls > 1 {
                        return Some(RecursionRefusal::SeveralSelfCalls);
                    }
                    for arg in args {
                        let from_claim = match arg {
                            Term::Var(v) => def.parameters.contains(v) || bound_so_far.contains(v),
                            _ => false,
                        };
                        if !from_claim {
                            return Some(RecursionRefusal::ArgumentNotFromClaim {
                                argument: crate::format::format_term(arg),
                            });
                        }
                    }
                    pattern_variables(args, &mut from_claims);
                }
                other => {
                    if any_prop_node(other, &|p| is_call_to(p, &def.name)) {
                        return Some(RecursionRefusal::Position);
                    }
                }
            }
        }
        if let Some(missing) = def.parameters.iter().find(|p| !from_claims.contains(p)) {
            return Some(RecursionRefusal::ParameterNotFromClaim {
                parameter: missing.to_string(),
            });
        }
    }
    None
}
