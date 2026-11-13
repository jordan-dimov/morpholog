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
//! transition.
//!
//! A `bind` of one claim pattern with a known coordinate makes its other
//! variables known to the statements after it, each as the value some
//! matching row holds at that position. The kernel binds only rows of
//! the snapshot, so a load keeping the rows equal to any such value
//! holds every row a later pattern can match, and a chain is keyed as
//! far as each link is keyed by the one before it. A variable ranging
//! over a collection parameter, through a `for`, a `forall ... in`, or a
//! membership test beside a pattern in one conjunction, is known as one
//! of that parameter's elements. A key a `let` produces is still unknown
//! here and makes its reads whole.
//!
//! Admits are kept apart from reads: an interpreter needs the admitted
//! predicates' membership to tell an effective admission from a repeat,
//! while a route that writes the delta and lets the database say what
//! changed does not.

use std::collections::{BTreeMap, BTreeSet};

use crate::definitions::DefinitionTable;
use crate::ir::{
    Claim, Definition, DefinitionName, PredicateName, Prop, RuleName, Stmt, Term, Transformation,
    Value, ValueExpr, Var,
};
use crate::propose::Transition;
use crate::state::EvalValue;

/// A term whose value is fixed before the body runs, or is one of the
/// values a snapshot row holds.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum KnownTerm {
    Literal(Value),
    Actor,
    Parameter(Var),
    /// The value at `position` of a `predicate` row matching the
    /// coordinates `of`: what a `bind` earlier in the body found there.
    /// `of` is never empty.
    Field {
        predicate: PredicateName,
        position: usize,
        of: Vec<(usize, KnownTerm)>,
    },
    /// One of the elements of the collection this parameter carries.
    Element(Var),
    /// The seed's value, or a value reached from it by following `link`
    /// any number of times: what a condition that walks a tree up from a
    /// known part can ask about.
    Reached {
        seed: Box<KnownTerm>,
        link: Link,
    },
}

/// One step through a two-place predicate: from a row whose coordinate
/// `from` holds a value already reached, to the value at `to`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Link {
    pub predicate: PredicateName,
    pub from: usize,
    pub to: usize,
}

/// A known coordinate with the transition's values in place: a value,
/// or the value at `position` of a `predicate` row matching `of`. A
/// loader resolves the second against the database, a reference
/// implementation against the whole state.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Key {
    Value(EvalValue),
    Field {
        predicate: PredicateName,
        position: usize,
        of: Vec<(usize, Key)>,
    },
    /// Any of these values: a collection argument's elements, each once,
    /// never more than [`MAX_ELEMENT_KEYS`].
    OneOf(Vec<EvalValue>),
    /// The seed's values and every value reached from them through
    /// `link`. A loader walks the link in the database.
    Reached {
        seed: Box<Key>,
        link: Link,
    },
}

/// The most elements a collection may hold and still key a read; a
/// larger one reads its predicate whole. Measured at this size an index
/// seek over the elements already locks the relation, as a whole read
/// does, so keys beyond it narrow nothing, and the statement stays far
/// from PostgreSQL's limit on bound parameters, one per element.
pub const MAX_ELEMENT_KEYS: usize = 1_000;

impl Key {
    /// Whether any predicate the key reads through is one of `these`.
    pub fn reads_any(&self, these: &BTreeSet<PredicateName>) -> bool {
        match self {
            Key::Value(_) | Key::OneOf(_) => false,
            Key::Field { predicate, of, .. } => {
                these.contains(predicate) || of.iter().any(|(_, key)| key.reads_any(these))
            }
            Key::Reached { seed, link } => these.contains(&link.predicate) || seed.reads_any(these),
        }
    }
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
            insert(patterns, KeyedPattern { coordinates });
        }
    }

    /// Both filters' rows.
    pub fn union(&mut self, other: &ReadFilter) {
        match (&mut *self, other) {
            (ReadFilter::Whole, _) => {}
            (_, ReadFilter::Whole) => *self = ReadFilter::Whole,
            (ReadFilter::Keyed(mine), ReadFilter::Keyed(theirs)) => {
                for pattern in theirs {
                    insert(mine, pattern.clone());
                }
            }
        }
    }

    /// The coordinates as keys, for one transition. `None` when the
    /// filter is whole.
    pub fn resolve(
        &self,
        transformation: &Transformation,
        transition: &Transition,
    ) -> Option<Vec<Vec<(usize, Key)>>> {
        let ReadFilter::Keyed(patterns) = self else {
            return None;
        };
        patterns
            .iter()
            .map(|pattern| resolve_coordinates(&pattern.coordinates, transformation, transition))
            .collect()
    }
}

/// A pattern fixing everything another fixes matches only rows the other
/// matches, so the filter keeps the one fixing less. A keyed bind
/// followed by a retract of the row it found would otherwise add a key on
/// every other position, over values few rows tell apart.
fn insert(patterns: &mut Vec<KeyedPattern>, pattern: KeyedPattern) {
    let within = |narrow: &KeyedPattern, wide: &KeyedPattern| {
        narrow
            .coordinates
            .iter()
            .all(|c| wide.coordinates.contains(c))
    };
    if patterns.iter().any(|kept| within(kept, &pattern)) {
        return;
    }
    patterns.retain(|kept| !within(&pattern, kept));
    patterns.push(pattern);
}

fn resolve_coordinates(
    coordinates: &[(usize, KnownTerm)],
    transformation: &Transformation,
    transition: &Transition,
) -> Option<Vec<(usize, Key)>> {
    coordinates
        .iter()
        .map(|(position, term)| Some((*position, resolve_term(term, transformation, transition)?)))
        .collect()
}

fn resolve_term(
    term: &KnownTerm,
    transformation: &Transformation,
    transition: &Transition,
) -> Option<Key> {
    let key = match term {
        KnownTerm::Literal(v) => Key::Value(EvalValue::from(v)),
        KnownTerm::Actor => Key::Value(EvalValue::Subject(transition.actor.clone())),
        KnownTerm::Parameter(p) => {
            let index = transformation.parameters.iter().position(|q| q == p)?;
            Key::Value(transition.args.get(index)?.clone())
        }
        KnownTerm::Field {
            predicate,
            position,
            of,
        } => Key::Field {
            predicate: predicate.clone(),
            position: *position,
            of: resolve_coordinates(of, transformation, transition)?,
        },
        KnownTerm::Element(p) => {
            let index = transformation.parameters.iter().position(|q| q == p)?;
            let EvalValue::Collection(items) = transition.args.get(index)? else {
                return None;
            };
            let mut distinct: Vec<EvalValue> = Vec::with_capacity(items.len());
            for item in items {
                if !distinct.contains(item) {
                    distinct.push(item.clone());
                }
            }
            if distinct.len() > MAX_ELEMENT_KEYS {
                return None;
            }
            Key::OneOf(distinct)
        }
        KnownTerm::Reached { seed, link } => Key::Reached {
            seed: Box::new(resolve_term(seed, transformation, transition)?),
            link: link.clone(),
        },
    };
    Some(key)
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

/// A body read that makes its predicate whole, and where it is: the
/// top-level statement (counted from 1), the name of the rule it sits in
/// when that rule has one, and the definitions it was reached through,
/// outermost first.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WholeRead {
    pub predicate: PredicateName,
    pub statement: usize,
    pub rule: Option<RuleName>,
    pub through: Vec<DefinitionName>,
}

impl ReadPlan {
    pub fn of(transformation: &Transformation, definitions: &[Definition]) -> Self {
        let mut plan = ReadPlan::default();
        Self::walk(transformation, definitions, &mut plan, None);
        plan
    }

    /// Every read that makes its predicate whole, in body order, each
    /// place once: what to key, for a body whose loads contend.
    pub fn whole_reads(
        transformation: &Transformation,
        definitions: &[Definition],
    ) -> Vec<WholeRead> {
        let mut trace = Trace::default();
        Self::walk(
            transformation,
            definitions,
            &mut ReadPlan::default(),
            Some(&mut trace),
        );
        trace.found
    }

    /// The one analysis behind both: `trace`, when given, also records
    /// where each read lost its keys. The proposal path passes none and
    /// builds no records.
    fn walk(
        transformation: &Transformation,
        definitions: &[Definition],
        plan: &mut ReadPlan,
        trace: Option<&mut Trace>,
    ) {
        let mut rebound = BTreeSet::new();
        let mut let_bound = BTreeSet::new();
        for stmt in &transformation.body {
            rebound_names(stmt, &mut rebound);
            let_names(stmt, &mut let_bound);
        }
        let mut known: Env = transformation
            .parameters
            .iter()
            .filter(|p| !rebound.contains(*p))
            .map(|p| (p.clone(), KnownTerm::Parameter(p.clone())))
            .collect();
        let mut walker = Walker {
            definitions: DefinitionTable::new(definitions),
            rebound,
            let_bound,
            plan,
            trace,
        };
        for (index, stmt) in transformation.body.iter().enumerate() {
            if let Some(trace) = walker.trace.as_deref_mut() {
                trace.statement = index + 1;
            }
            walker.stmt(stmt, &known, &mut BTreeSet::new());
            walker.extend(stmt, &mut known);
        }
    }

    /// The (predicate, position) pairs an index must serve: every keyed
    /// coordinate of the reads, and for each keyed admit pattern one
    /// coordinate, since a membership check finds its row through any
    /// one of them: a coordinate a read already indexes when there is one,
    /// the pattern's first otherwise. Every admit coordinate would index
    /// every amount a body writes.
    pub fn seek_positions(&self) -> BTreeSet<(PredicateName, usize)> {
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
        for (predicate, filter) in &self.admits {
            if let ReadFilter::Keyed(patterns) = filter {
                for pattern in patterns {
                    let served = pattern
                        .coordinates
                        .iter()
                        .any(|(position, _)| out.contains(&(predicate.clone(), *position)));
                    if !served && let Some((position, _)) = pattern.coordinates.first() {
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
        Stmt::ForEach { body, .. } => {
            for inner in body {
                rebound_names(inner, out);
            }
        }
        Stmt::If {
            then, otherwise, ..
        } => {
            for inner in then.iter().chain(otherwise) {
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

/// Names a `let` or a `new Subject()` binds, anywhere in the body: a
/// loop binding so named is replaced as the body runs, so it is never
/// known as an element.
fn let_names(stmt: &Stmt, out: &mut BTreeSet<Var>) {
    match stmt {
        Stmt::Let { name, .. } | Stmt::LetNewSubject { name } => {
            out.insert(name.clone());
        }
        Stmt::For { body, .. } | Stmt::ForEach { body, .. } => {
            for inner in body {
                let_names(inner, out);
            }
        }
        Stmt::If {
            then, otherwise, ..
        } => {
            for inner in then.iter().chain(otherwise) {
                let_names(inner, out);
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
    /// Names some statement binds as the body runs: never known.
    rebound: BTreeSet<Var>,
    /// Names a `let` or a `new Subject()` binds: never an element.
    let_bound: BTreeSet<Var>,
    plan: &'a mut ReadPlan,
    trace: Option<&'a mut Trace>,
}

/// Where the walk is, and the whole reads found so far, for
/// [`ReadPlan::whole_reads`].
#[derive(Default)]
struct Trace {
    statement: usize,
    rule: Option<RuleName>,
    through: Vec<DefinitionName>,
    found: Vec<WholeRead>,
}

impl Walker<'_> {
    fn stmt(&mut self, stmt: &Stmt, known: &Env, seen: &mut BTreeSet<DefinitionName>) {
        match stmt {
            Stmt::Require { prop, name } | Stmt::BindOne { prop, name } => {
                let outer = match (self.trace.as_deref_mut(), name) {
                    (Some(trace), Some(_)) => {
                        Some(std::mem::replace(&mut trace.rule, name.clone()))
                    }
                    _ => None,
                };
                self.prop(prop, known, seen);
                if let (Some(trace), Some(outer)) = (self.trace.as_deref_mut(), outer) {
                    trace.rule = outer;
                }
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
            Stmt::ForEach {
                predicate,
                args,
                body,
            } => {
                // The pattern is read as a `bind`'s is, and what it binds
                // is known to the body as a `bind`'s is to its block.
                self.pattern(predicate, args, known);
                let mut known = known.clone();
                self.fields(predicate, args, &mut known);
                for inner in body {
                    self.stmt(inner, &known, seen);
                    self.extend(inner, &mut known);
                }
            }
            Stmt::For {
                binding,
                collection,
                body,
            } => {
                self.value(collection, known, seen);
                let mut known = known.clone();
                known.remove(binding);
                if let ValueExpr::Term(Term::Var(source)) = collection {
                    self.element(binding, source, &mut known);
                }
                for inner in body {
                    self.stmt(inner, &known, seen);
                    self.extend(inner, &mut known);
                }
            }
            Stmt::If {
                when,
                then,
                otherwise,
            } => {
                self.prop(when, known, seen);
                for branch in [then, otherwise] {
                    let mut known = known.clone();
                    for inner in branch {
                        self.stmt(inner, &known, seen);
                        self.extend(inner, &mut known);
                    }
                }
            }
        }
    }

    /// `binding` ranges over the elements of `source`: known as one of
    /// them when `source` is a parameter known as itself and nothing
    /// else binds the name.
    fn element(&self, binding: &Var, source: &Var, known: &mut Env) {
        if known.contains_key(binding) || self.let_bound.contains(binding) {
            return;
        }
        if matches!(known.get(source), Some(KnownTerm::Parameter(p)) if p == source) {
            known.insert(binding.clone(), KnownTerm::Element(source.clone()));
        }
    }

    /// The environment a conjunction's conjuncts are walked in: a
    /// variable a membership test ranges over a known parameter is one
    /// of its elements. The scan stops at the first conjunct that is
    /// neither a pattern nor a membership test: the kernel evaluates a
    /// conjunction left to right over every live binding, so a row the
    /// pattern matched outside the collection reaches each conjunct
    /// before the test, and a conjunct there that can raise would raise
    /// on it. A load without that row would answer differently.
    fn members(&self, conjuncts: &[Prop], known: &Env) -> Env {
        let mut inner = known.clone();
        for conjunct in conjuncts {
            match conjunct {
                Prop::Claim { .. } => {}
                Prop::In(Term::Var(x), ValueExpr::Term(Term::Var(source))) => {
                    self.element(x, source, &mut inner);
                }
                Prop::In(_, _) => {}
                Prop::Defined { .. }
                | Prop::Implies { .. }
                | Prop::Exists { .. }
                | Prop::And(_)
                | Prop::Or(_)
                | Prop::Pre(_)
                | Prop::Not(_)
                | Prop::Xor(..)
                | Prop::Eq(..)
                | Prop::Neq(..)
                | Prop::Compare { .. }
                | Prop::Forall { .. } => break,
            }
        }
        inner
    }

    /// What a statement makes known to the statements after it in its
    /// block: a `bind` of one claim pattern with a known coordinate fixes
    /// each variable it binds to that row's value at the variable's
    /// position. A name already known, or bound as the body runs, is left
    /// as it is.
    fn extend(&self, stmt: &Stmt, known: &mut Env) {
        let Stmt::BindOne {
            prop: Prop::Claim { predicate, args },
            ..
        } = stmt
        else {
            return;
        };
        self.fields(predicate, args, known);
    }

    /// What a claim pattern with a known coordinate makes known: each
    /// variable it binds is that row's value at the variable's position.
    fn fields(&self, predicate: &PredicateName, args: &[Term], known: &mut Env) {
        let of = coordinates(args, known);
        if of.is_empty() {
            return;
        }
        for (position, term) in args.iter().enumerate() {
            if let Term::Var(name) = term
                && !known.contains_key(name)
                && !self.rebound.contains(name)
            {
                known.insert(
                    name.clone(),
                    KnownTerm::Field {
                        predicate: predicate.clone(),
                        position,
                        of: of.clone(),
                    },
                );
            }
        }
    }

    fn pattern(&mut self, predicate: &PredicateName, args: &[Term], known: &Env) {
        let coordinates = coordinates(args, known);
        if let Some(trace) = self.trace.as_deref_mut()
            && coordinates.is_empty()
        {
            let read = WholeRead {
                predicate: predicate.clone(),
                statement: trace.statement,
                rule: trace.rule.clone(),
                through: trace.through.clone(),
            };
            if !trace.found.contains(&read) {
                trace.found.push(read);
            }
        }
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
                if let Some(trace) = self.trace.as_deref_mut() {
                    trace.through.push(name.clone());
                }
                definitions.enter(name, seen, |def, seen| {
                    // A condition that calls itself reads its claims
                    // again under other arguments. When it walks one
                    // link up from a known argument, every argument it
                    // asks about is reached from that one; otherwise
                    // nothing is known in its body.
                    let inner: Env = if crate::recursion::asks_itself(def) {
                        crate::recursion::single_link(def)
                            .and_then(|walk| {
                                let seed = known_term(args.get(walk.walked)?, known)?;
                                let mut inner = Env::new();
                                inner.insert(
                                    def.parameters[walk.walked].clone(),
                                    KnownTerm::Reached {
                                        seed: Box::new(seed),
                                        link: walk.link,
                                    },
                                );
                                Some(inner)
                            })
                            .unwrap_or_default()
                    } else {
                        def.parameters
                            .iter()
                            .zip(args)
                            .filter_map(|(param, arg)| {
                                known_term(arg, known).map(|term| (param.clone(), term))
                            })
                            .collect()
                    };
                    if let Some(body) = def.body.as_prop() {
                        self.prop(body, &inner, seen);
                    }
                });
                if let Some(trace) = self.trace.as_deref_mut() {
                    trace.through.pop();
                }
            }
            Prop::And(props) => {
                let known = self.members(props, known);
                for p in props {
                    self.prop(p, &known, seen);
                }
            }
            Prop::Or(props) => {
                for p in props {
                    self.prop(p, known, seen);
                }
            }
            Prop::Implies { left, right } | Prop::Xor(left, right) => {
                self.prop(left, known, seen);
                self.prop(right, known, seen);
            }
            Prop::Not(p) | Prop::Pre(p) => self.prop(p, known, seen),
            Prop::Exists { binding, body } => {
                let mut inner = known.clone();
                inner.remove(binding);
                self.prop(body, &inner, seen);
            }
            Prop::Forall {
                binding,
                source,
                body,
            } => {
                self.prop(source, known, seen);
                let mut inner = known.clone();
                inner.remove(binding);
                if let Prop::In(Term::Var(x), ValueExpr::Term(Term::Var(collection))) = &**source
                    && x == binding
                {
                    self.element(binding, collection, &mut inner);
                }
                self.prop(body, &inner, seen);
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
            // The body's reads are followed under what the arguments are
            // known as, as a condition's are.
            ValueExpr::Defined { name, args } => {
                for a in args {
                    self.value(a, known, seen);
                }
                let definitions = self.definitions;
                if let Some(trace) = self.trace.as_deref_mut() {
                    trace.through.push(name.clone());
                }
                definitions.enter(name, seen, |def, seen| {
                    let inner: Env = def
                        .parameters
                        .iter()
                        .zip(args)
                        .filter_map(|(param, arg)| match arg {
                            ValueExpr::Term(term) => {
                                known_term(term, known).map(|t| (param.clone(), t))
                            }
                            ValueExpr::Arith { .. }
                            | ValueExpr::Sum { .. }
                            | ValueExpr::Extremum { .. }
                            | ValueExpr::ValueOf { .. }
                            | ValueExpr::Cond { .. }
                            | ValueExpr::Call { .. }
                            | ValueExpr::Defined { .. } => None,
                        })
                        .collect();
                    if let Some(body) = def.body.as_value() {
                        self.value(body, &inner, seen);
                    }
                });
                if let Some(trace) = self.trace.as_deref_mut() {
                    trace.through.pop();
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
        Term::Wildcard | Term::DecidedAt => None,
    }
}

fn coordinates(args: &[Term], known: &Env) -> Vec<(usize, KnownTerm)> {
    args.iter()
        .enumerate()
        .filter_map(|(i, term)| known_term(term, known).map(|t| (i, t)))
        .collect()
}
