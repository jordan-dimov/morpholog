//! What a transformation body reads from history, position by position:
//! the storage access plan an adapter loads by.
//!
//! Before a body runs, some of its claim patterns already fix values at
//! some positions: a literal, `actor`, or a parameter no statement
//! rebinds. Every evaluation of such a pattern matches only rows equal to
//! those values there, so a load that keeps, for each read predicate, the
//! rows matching any of its patterns' known coordinates contains every row
//! the body can observe. One pattern with no known coordinate makes its
//! predicate whole. The plan is static; the values arrive with the
//! transition. Keys a body binds as it runs (a `bind`, a `let`, a `for`)
//! are unknown here and make their reads whole; narrowing those is left
//! for a forcing example.
//!
//! Admits are kept apart from reads: an interpreter needs the admitted
//! predicates' membership to tell an effective admission from a repeat,
//! while a route that writes the delta and lets the database say what
//! changed does not.

use std::collections::{BTreeMap, BTreeSet};

use crate::definitions::DefinitionTable;
use crate::eval::literal_value;
use crate::ir::{
    Claim, Definition, DefinitionName, PredicateName, Prop, Stmt, Term, Transformation, Value,
    ValueExpr, Var,
};
use crate::propose::Transition;
use crate::state::EvalValue;

/// A term whose value is fixed before the body runs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum KnownTerm {
    Literal(Value),
    Actor,
    Parameter(Var),
}

/// One claim pattern's known coordinates, all of which a matching row
/// satisfies at once. Never empty.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KeyedPattern {
    pub coordinates: Vec<(usize, KnownTerm)>,
}

/// How much of one predicate a body can observe.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReadFilter {
    /// Some pattern fixes nothing: every row.
    Whole,
    /// Rows matching any of these patterns' coordinates.
    Keyed(Vec<KeyedPattern>),
}

impl ReadFilter {
    fn add(&mut self, coordinates: Vec<(usize, KnownTerm)>) {
        if coordinates.is_empty() {
            *self = ReadFilter::Whole;
            return;
        }
        if let ReadFilter::Keyed(patterns) = self {
            let pattern = KeyedPattern { coordinates };
            if !patterns.contains(&pattern) {
                patterns.push(pattern);
            }
        }
    }

    /// Both filters' rows.
    pub fn union(&mut self, other: &ReadFilter) {
        match (&mut *self, other) {
            (ReadFilter::Whole, _) => {}
            (_, ReadFilter::Whole) => *self = ReadFilter::Whole,
            (ReadFilter::Keyed(mine), ReadFilter::Keyed(theirs)) => {
                for pattern in theirs {
                    if !mine.contains(pattern) {
                        mine.push(pattern.clone());
                    }
                }
            }
        }
    }

    /// The coordinates as values, for one transition. `None` when the
    /// filter is whole.
    pub fn resolve(
        &self,
        transformation: &Transformation,
        transition: &Transition,
    ) -> Option<Vec<Vec<(usize, EvalValue)>>> {
        let ReadFilter::Keyed(patterns) = self else {
            return None;
        };
        let mut resolved = Vec::with_capacity(patterns.len());
        for pattern in patterns {
            let mut coordinates = Vec::with_capacity(pattern.coordinates.len());
            for (position, term) in &pattern.coordinates {
                let value = match term {
                    KnownTerm::Literal(v) => literal_value(v).ok()?,
                    KnownTerm::Actor => EvalValue::Subject(transition.actor.clone()),
                    KnownTerm::Parameter(p) => {
                        let index = transformation.parameters.iter().position(|q| q == p)?;
                        transition.args.get(index)?.clone()
                    }
                };
                coordinates.push((*position, value));
            }
            resolved.push(coordinates);
        }
        Some(resolved)
    }
}

/// A transformation's reads and admits, by predicate.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ReadPlan {
    /// Predicates the body consults: gates, lookups, sums, retract
    /// patterns, through definitions.
    pub reads: BTreeMap<PredicateName, ReadFilter>,
    /// Predicates the body admits, keyed by the admitted claim's known
    /// coordinates: enough to tell whether the claim is already present.
    pub admits: BTreeMap<PredicateName, ReadFilter>,
}

impl ReadPlan {
    pub fn of(transformation: &Transformation, definitions: &[Definition]) -> Self {
        let mut rebound = BTreeSet::new();
        for stmt in &transformation.body {
            rebound_names(stmt, &mut rebound);
        }
        let known: Env = transformation
            .parameters
            .iter()
            .filter(|p| !rebound.contains(*p))
            .map(|p| (p.clone(), KnownTerm::Parameter(p.clone())))
            .collect();
        let mut plan = ReadPlan::default();
        let mut walker = Walker {
            definitions: DefinitionTable::new(definitions),
            plan: &mut plan,
        };
        for stmt in &transformation.body {
            walker.stmt(stmt, &known, &mut BTreeSet::new());
        }
        plan
    }

    /// Every keyed (predicate, position) among the reads: what an index
    /// serves. An admit's membership seeks through whichever of its
    /// coordinates the reads already index; one is enough to find a row.
    pub fn read_positions(&self) -> BTreeSet<(PredicateName, usize)> {
        let mut out = BTreeSet::new();
        for (predicate, filter) in &self.reads {
            if let ReadFilter::Keyed(patterns) = filter {
                for pattern in patterns {
                    for (position, _) in &pattern.coordinates {
                        out.insert((predicate.clone(), *position));
                    }
                }
            }
        }
        out
    }
}

/// Names a body binds as it runs: unknown before it does.
fn rebound_names(stmt: &Stmt, out: &mut BTreeSet<Var>) {
    match stmt {
        Stmt::Let { name, .. } | Stmt::LetNewSubject { name } => {
            out.insert(name.clone());
        }
        Stmt::For { binding, body, .. } => {
            out.insert(binding.clone());
            for inner in body {
                rebound_names(inner, out);
            }
        }
        Stmt::Require { .. }
        | Stmt::BindOne { .. }
        | Stmt::Assert(_)
        | Stmt::Retract { .. }
        | Stmt::Emit(_) => {}
    }
}

/// The variables known at a point, and what they are known as.
type Env = BTreeMap<Var, KnownTerm>;

struct Walker<'a> {
    definitions: DefinitionTable<'a>,
    plan: &'a mut ReadPlan,
}

impl Walker<'_> {
    fn stmt(&mut self, stmt: &Stmt, known: &Env, seen: &mut BTreeSet<DefinitionName>) {
        match stmt {
            Stmt::Require { prop, .. } | Stmt::BindOne { prop, .. } => {
                self.prop(prop, known, seen);
            }
            Stmt::Let { value, .. } => self.value(value, known, seen),
            Stmt::LetNewSubject { .. } | Stmt::Emit(_) => {}
            Stmt::Assert(Claim { predicate, args }) => {
                let coordinates = coordinates(args, known);
                self.plan
                    .admits
                    .entry(predicate.clone())
                    .or_insert_with(|| ReadFilter::Keyed(Vec::new()))
                    .add(coordinates);
            }
            Stmt::Retract { predicate, args } => self.pattern(predicate, args, known),
            Stmt::For {
                collection, body, ..
            } => {
                self.value(collection, known, seen);
                for inner in body {
                    self.stmt(inner, known, seen);
                }
            }
        }
    }

    fn pattern(&mut self, predicate: &PredicateName, args: &[Term], known: &Env) {
        let coordinates = coordinates(args, known);
        self.plan
            .reads
            .entry(predicate.clone())
            .or_insert_with(|| ReadFilter::Keyed(Vec::new()))
            .add(coordinates);
    }

    fn prop(&mut self, prop: &Prop, known: &Env, seen: &mut BTreeSet<DefinitionName>) {
        match prop {
            Prop::Claim { predicate, args } => self.pattern(predicate, args, known),
            // The definition's parameters are known as whatever the call
            // passes for them, when that is known; its body is walked in
            // that environment, so a nested call inherits the same rule.
            Prop::Defined { name, args } => {
                let definitions = self.definitions;
                definitions.enter(name, seen, |def, seen| {
                    let inner: Env = def
                        .parameters
                        .iter()
                        .zip(args)
                        .filter_map(|(param, arg)| {
                            known_term(arg, known).map(|term| (param.clone(), term))
                        })
                        .collect();
                    self.prop(&def.body, &inner, seen);
                });
            }
            Prop::And(props) | Prop::Or(props) => {
                for p in props {
                    self.prop(p, known, seen);
                }
            }
            Prop::Implies { left, right } | Prop::Xor(left, right) => {
                self.prop(left, known, seen);
                self.prop(right, known, seen);
            }
            Prop::Not(p) | Prop::Exists { body: p, .. } | Prop::Pre(p) => {
                self.prop(p, known, seen);
            }
            Prop::Forall { source, body, .. } => {
                self.prop(source, known, seen);
                self.prop(body, known, seen);
            }
            Prop::Eq(l, r)
            | Prop::Neq(l, r)
            | Prop::Compare {
                left: l, right: r, ..
            } => {
                self.value(l, known, seen);
                self.value(r, known, seen);
            }
            Prop::In(_, _) => {}
        }
    }

    fn value(&mut self, expr: &ValueExpr, known: &Env, seen: &mut BTreeSet<DefinitionName>) {
        match expr {
            ValueExpr::ValueOf {
                predicate,
                args,
                default,
                ..
            } => {
                self.pattern(predicate, args, known);
                if let Some(d) = default {
                    self.value(d, known, seen);
                }
            }
            ValueExpr::Arith { left, right, .. } => {
                self.value(left, known, seen);
                self.value(right, known, seen);
            }
            ValueExpr::Sum { value, body, .. } => {
                self.value(value, known, seen);
                self.prop(body, known, seen);
            }
            ValueExpr::Extremum { body, .. } => self.prop(body, known, seen),
            ValueExpr::Cond {
                when,
                then,
                otherwise,
            } => {
                self.prop(when, known, seen);
                self.value(then, known, seen);
                self.value(otherwise, known, seen);
            }
            ValueExpr::Call { args, .. } => {
                for a in args {
                    self.value(a, known, seen);
                }
            }
            ValueExpr::Term(_) => {}
        }
    }
}

/// What a pattern's argument is known as, if anything.
fn known_term(term: &Term, known: &Env) -> Option<KnownTerm> {
    match term {
        Term::Literal(v) => Some(KnownTerm::Literal(v.clone())),
        Term::Actor => Some(KnownTerm::Actor),
        Term::Var(v) => known.get(v).cloned(),
        Term::Wildcard => None,
    }
}

fn coordinates(args: &[Term], known: &Env) -> Vec<(usize, KnownTerm)> {
    args.iter()
        .enumerate()
        .filter_map(|(i, term)| known_term(term, known).map(|t| (i, t)))
        .collect()
}
