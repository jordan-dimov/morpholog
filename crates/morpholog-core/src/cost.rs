//! The shape of the work over a `periods` grid: how many points the
//! grid can have, and how many distinct aggregates each point reaches.
//!
//! The two figures say different things. The points are a bound: the
//! rule's window bound fixes them, whatever the state. The aggregates
//! are a count of expressions, read from the text through the
//! definitions a point calls. A body `let` is substituted before the IR
//! exists, so a figure named once and used three times is three copies;
//! one evaluation computes identical copies once, so they count once.
//! How many times the aggregates are actually computed is not bounded
//! here. It can be fewer, when one reads nothing the point binds and is
//! computed once in all. It can be more, when a point ranges over claims
//! (a `for` over a pattern, or a pattern that binds what the aggregate
//! reads) and computes it once a match. Neither figure says how many
//! claims an aggregate reads.

use std::collections::BTreeSet;

use crate::analysis::prop_refs;
use crate::check::grid::{self, Frame};
use crate::definitions::DefinitionTable;
use crate::fold::{self, Node};
use crate::ir::{
    DefinitionBody, DefinitionName, ExtremumOp, PredicateName, Program, Prop, Stmt, Term,
    ValueExpr, Var,
};

/// Each grid whose points reach an aggregate, as the rule it sits in (in
/// a transformation, the top-level statement counted from 1) and the
/// shape of its work, such as `up to 745 points x 1 sum over Trade`.
/// Grids a point ranges over together multiply, `up to 25 x 97 points`,
/// whether nested or joined in one conjunction. A grid a definition
/// builds is reported in each rule that calls it. A grid whose size is
/// not read here, one over a name a `let` aliases, says `an unknown
/// number of points`.
pub fn grid_costs(program: &Program) -> Vec<(String, String)> {
    let definitions = DefinitionTable::new(&program.definitions);
    let mut out = Vec::new();
    for t in &program.transformations {
        let mut nodes = Vec::new();
        for stmt in &t.body {
            fold::walk_stmt(stmt, &mut |n| nodes.push(n));
        }
        let frame = grid::frame(&nodes);
        let lets: Vec<(&Var, &ValueExpr)> = nodes
            .iter()
            .filter_map(|n| {
                if let Node::Stmt(Stmt::Let { name, value }) = n {
                    Some((name, value))
                } else {
                    None
                }
            })
            .collect();
        for (index, stmt) in t.body.iter().enumerate() {
            let mut place = format!("transformation `{}`, statement {}", t.name, index + 1);
            if let Stmt::Require {
                name: Some(rule), ..
            } = stmt
            {
                place.push_str(&format!(" `{rule}`"));
            }
            let mut walk = Walk::new(definitions, frame.clone());
            walk.lets.clone_from(&lets);
            walk.stmt(stmt);
            out.extend(walk.lines().into_iter().map(|line| (place.clone(), line)));
        }
    }
    for inv in &program.invariants {
        let mut walk = Walk::new(definitions, prop_frame(&inv.body));
        walk.prop(&inv.body);
        let place = format!("invariant `{}`", inv.name);
        out.extend(walk.lines().into_iter().map(|line| (place.clone(), line)));
    }
    for d in &program.derived_claims {
        let mut nodes = Vec::new();
        fold::walk_prop(&d.domain, &mut |n| nodes.push(n));
        for v in &d.values {
            fold::walk_value(&v.expr, &mut |n| nodes.push(n));
        }
        let mut walk = Walk::new(definitions, grid::frame(&nodes));
        walk.prop(&d.domain);
        for v in &d.values {
            walk.value(&v.expr);
        }
        let place = format!("derived `{}`", d.predicate);
        out.extend(walk.lines().into_iter().map(|line| (place.clone(), line)));
    }
    out
}

fn prop_frame(prop: &Prop) -> Frame<'_> {
    let mut nodes = Vec::new();
    fold::walk_prop(prop, &mut |n| nodes.push(n));
    grid::frame(&nodes)
}

/// A grid the walk is inside: the node that ranges over it (an `in` or a
/// `for`), so two memberships of one grid are two factors, and its most
/// points when its size is read.
#[derive(Clone, Copy, PartialEq)]
struct Grid {
    at: usize,
    points: Option<u128>,
}

/// A definition call on the way to an aggregate. Two calls with the
/// same arguments are one frame to the memo.
#[derive(Clone, Copy, PartialEq)]
enum Call<'a> {
    Prop(&'a Prop),
    Value(&'a ValueExpr),
}

struct Found<'a> {
    grids: Vec<Grid>,
    calls: Vec<Call<'a>>,
    node: &'a ValueExpr,
    kind: &'static str,
    over: BTreeSet<PredicateName>,
    /// A definition that calls itself on the way, whose body runs once a
    /// round.
    recursive: Option<&'a DefinitionName>,
}

struct Walk<'a> {
    definitions: DefinitionTable<'a>,
    /// The frame being walked.
    frame: Frame<'a>,
    stack: BTreeSet<DefinitionName>,
    calls: Vec<Call<'a>>,
    recursive: Vec<&'a DefinitionName>,
    grids: Vec<Grid>,
    /// A transformation body's `let`s, which name a grid a `forall` or a
    /// `sum` ranges over.
    lets: Vec<(&'a Var, &'a ValueExpr)>,
    found: Vec<Found<'a>>,
}

impl<'a> Walk<'a> {
    fn new(definitions: DefinitionTable<'a>, frame: Frame<'a>) -> Self {
        Self {
            definitions,
            frame,
            stack: BTreeSet::new(),
            calls: Vec::new(),
            recursive: Vec::new(),
            grids: Vec::new(),
            lets: Vec::new(),
            found: Vec::new(),
        }
    }

    /// The grid `expr` is, directly or through a `let`, ranged over by
    /// the node at `at`. A name two `let`s bind, in two branches say,
    /// names a grid of unknown size: which one holds is the checker's
    /// scopes, not read here.
    fn grid(&self, at: usize, expr: &'a ValueExpr) -> Option<Grid> {
        let expr = if let ValueExpr::Term(Term::Var(v)) = expr {
            let values: Vec<&ValueExpr> = self
                .lets
                .iter()
                .filter(|(n, _)| *n == v)
                .map(|(_, e)| *e)
                .collect();
            match values.as_slice() {
                [] => return None,
                [one] => *one,
                several => {
                    return several
                        .iter()
                        .any(|e| grid::grid_call(e).is_some())
                        .then_some(Grid { at, points: None });
                }
            }
        } else {
            expr
        };
        grid::grid_call(expr)?;
        Some(Grid {
            at,
            points: grid::grid_size(expr, &self.frame).map(|size| size.periods),
        })
    }

    /// The grids `prop` ranges over: each `in` a grid among its
    /// conjuncts. An `in` whose element is already bound only tests, but
    /// telling which are would need the checker's scopes, so each counts:
    /// the points can be overstated, never understated.
    fn memberships(&self, prop: &'a Prop) -> Vec<Grid> {
        grid::conjuncts(prop)
            .into_iter()
            .filter_map(|p| {
                if let Prop::In(_, collection) = p {
                    self.grid(std::ptr::from_ref(p).addr(), collection)
                } else {
                    None
                }
            })
            .collect()
    }

    /// Walk `f` inside `grids`, each counted once however often it is met.
    fn within(&mut self, grids: Vec<Grid>, f: impl FnOnce(&mut Self)) {
        let before = self.grids.len();
        for g in grids {
            if !self.grids.iter().any(|outer| outer.at == g.at) {
                self.grids.push(g);
            }
        }
        f(self);
        self.grids.truncate(before);
    }

    fn stmt(&mut self, stmt: &'a Stmt) {
        match stmt {
            Stmt::Require { prop, .. } | Stmt::BindOne { prop, .. } => self.prop(prop),
            Stmt::Let { value, .. } => self.value(value),
            Stmt::LetNewSubject { .. } | Stmt::Assert(_) | Stmt::Retract { .. } | Stmt::Emit(_) => {
            }
            Stmt::ForEach { body, .. } => {
                for s in body {
                    self.stmt(s);
                }
            }
            Stmt::For {
                collection, body, ..
            } => {
                self.value(collection);
                let grid = self.grid(std::ptr::from_ref(collection).addr(), collection);
                self.within(grid.into_iter().collect(), |w| {
                    for s in body {
                        w.stmt(s);
                    }
                });
            }
            Stmt::If {
                when,
                then,
                otherwise,
            } => {
                self.prop(when);
                for s in then.iter().chain(otherwise) {
                    self.stmt(s);
                }
            }
        }
    }

    fn prop(&mut self, prop: &'a Prop) {
        match prop {
            Prop::Claim { .. } => {}
            Prop::Defined { name, .. } => self.call(Call::Prop(prop), name),
            Prop::In(_, collection) => self.value(collection),
            Prop::And(parts) => {
                let grids = self.memberships(prop);
                self.within(grids, |w| {
                    for p in parts {
                        w.prop(p);
                    }
                });
            }
            Prop::Or(parts) => {
                for p in parts {
                    self.prop(p);
                }
            }
            Prop::Implies { left, right } | Prop::Xor(left, right) => {
                self.prop(left);
                self.prop(right);
            }
            Prop::Not(p) | Prop::Pre(p) | Prop::Exists { body: p, .. } => self.prop(p),
            Prop::Forall { source, body, .. } => {
                self.prop(source);
                let grids = self.memberships(source);
                self.within(grids, |w| w.prop(body));
            }
            Prop::Eq(left, right) | Prop::Neq(left, right) | Prop::Compare { left, right, .. } => {
                self.value(left);
                self.value(right);
            }
        }
    }

    fn value(&mut self, expr: &'a ValueExpr) {
        match expr {
            ValueExpr::Term(_) => {}
            ValueExpr::Arith { left, right, .. } => {
                self.value(left);
                self.value(right);
            }
            ValueExpr::Sum { value, body, .. } => {
                self.aggregate(expr, "sum", body);
                let grids = self.memberships(body);
                self.within(grids, |w| w.value(value));
                self.prop(body);
            }
            ValueExpr::Extremum { op, body, .. } => {
                let kind = match op {
                    ExtremumOp::Min => "min",
                    ExtremumOp::Max => "max",
                };
                self.aggregate(expr, kind, body);
                self.prop(body);
            }
            ValueExpr::ValueOf { default, .. } => {
                if let Some(d) = default {
                    self.value(d);
                }
            }
            ValueExpr::Cond {
                when,
                then,
                otherwise,
            } => {
                self.prop(when);
                self.value(then);
                self.value(otherwise);
            }
            ValueExpr::Call { args, .. } => {
                for a in args {
                    self.value(a);
                }
            }
            ValueExpr::Defined { name, args } => {
                for a in args {
                    self.value(a);
                }
                self.call(Call::Value(expr), name);
            }
        }
    }

    fn aggregate(&mut self, node: &'a ValueExpr, kind: &'static str, body: &'a Prop) {
        if self.grids.is_empty() {
            return;
        }
        let mut over = BTreeSet::new();
        prop_refs(body, self.definitions, &mut BTreeSet::new(), &mut over);
        self.found.push(Found {
            grids: self.grids.clone(),
            calls: self.calls.clone(),
            node,
            kind,
            over,
            recursive: self.recursive.first().copied(),
        });
    }

    /// Walk a called definition's body in its own frame. A definition
    /// already on the stack contributes nothing more.
    fn call(&mut self, call: Call<'a>, name: &'a DefinitionName) {
        let Some(def) = self.definitions.get(name) else {
            return;
        };
        if !self.stack.insert(name.clone()) {
            return;
        }
        let mut nodes = Vec::new();
        match &def.body {
            DefinitionBody::Prop(body) => fold::walk_prop(body, &mut |n| nodes.push(n)),
            DefinitionBody::Value(body) => fold::walk_value(body, &mut |n| nodes.push(n)),
        }
        let outer = std::mem::replace(&mut self.frame, grid::frame(&nodes));
        self.calls.push(call);
        let recursive = def.calls_itself();
        if recursive {
            self.recursive.push(&def.name);
        }
        match &def.body {
            DefinitionBody::Prop(body) => self.prop(body),
            DefinitionBody::Value(body) => self.value(body),
        }
        if recursive {
            self.recursive.pop();
        }
        self.calls.pop();
        self.frame = outer;
        self.stack.remove(name);
    }

    /// One line per grid, outermost first in the order met, each
    /// aggregate counted once per frame.
    fn lines(self) -> Vec<String> {
        let mut groups: Vec<(Vec<Grid>, Vec<&Found<'a>>)> = Vec::new();
        for f in &self.found {
            let index = if let Some(i) = groups.iter().position(|(g, _)| *g == f.grids) {
                i
            } else {
                groups.push((f.grids.clone(), Vec::new()));
                groups.len() - 1
            };
            let group = &mut groups[index].1;
            if !group.iter().any(|m| m.calls == f.calls && m.node == f.node) {
                group.push(f);
            }
        }
        groups
            .into_iter()
            .map(|(grids, members)| describe(&grids, &members))
            .collect()
    }
}

fn describe(grids: &[Grid], members: &[&Found<'_>]) -> String {
    let points = if grids.iter().all(|g| g.points.is_some()) {
        let sizes: Vec<String> = grids
            .iter()
            .filter_map(|g| g.points.map(|n| n.to_string()))
            .collect();
        format!("up to {} points", sizes.join(" x "))
    } else {
        "up to an unknown number of points".to_string()
    };
    let mut counts = Vec::new();
    for (kind, plural) in [("sum", "sums"), ("min", "mins"), ("max", "maxes")] {
        match members.iter().filter(|m| m.kind == kind).count() {
            0 => {}
            1 => counts.push(format!("1 {kind}")),
            n => counts.push(format!("{n} {plural}")),
        }
    }
    let over: BTreeSet<&PredicateName> = members.iter().flat_map(|m| &m.over).collect();
    let mut line = format!("{points} x {}", counts.join(" and "));
    if !over.is_empty() {
        let names: Vec<String> = over.iter().map(ToString::to_string).collect();
        line.push_str(&format!(" over {}", names.join(", ")));
    }
    if let Some(name) = members.iter().find_map(|m| m.recursive) {
        line.push_str(&format!(
            "; `{name}` calls itself, so its aggregates run once a round"
        ));
    }
    line
}
