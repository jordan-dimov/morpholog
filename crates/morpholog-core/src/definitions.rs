//! Definition machinery: name lookup ([`DefinitionTable`]), the call graph
//! ([`definition_topo_order`]), and the direct-call collector. Everything
//! that looks through a [`Prop::Defined`] call resolves it here, so the
//! evaluator, the static checks and the analysis walkers agree on what a
//! call expands to.
//!
//! [`DefinitionTable`] is a cheap `Copy` view over a definitions slice.

use std::collections::{BTreeSet, HashMap};

use crate::ir::{Definition, DefinitionName, Program, Prop, Stmt, ValueExpr};

/// Turn every `Prop::Claim` whose name is a declared definition into the
/// `Prop::Defined` call it means.
///
/// The parser runs this once the whole programme is read, since a call can
/// come before the definition it names. Hand-built IR with such
/// `Prop::Claim` nodes must run it before validating, or build calls with
/// `ir_builder::defined` instead.
///
/// Only proposition positions resolve. `admit` / `retract` / `emit` targets
/// and `value` lookups stay claim-shaped, so a definition name there gets
/// its own unresolved-call error.
pub fn resolve_defined_calls(program: &mut Program) {
    let names: BTreeSet<String> = program
        .definitions
        .iter()
        .map(|d| d.name.to_string())
        .collect();
    if names.is_empty() {
        return;
    }
    for def in &mut program.definitions {
        resolve_in_prop(&mut def.body, &names);
    }
    for inv in &mut program.invariants {
        resolve_in_prop(&mut inv.body, &names);
    }
    for t in &mut program.transformations {
        for stmt in &mut t.body {
            resolve_in_stmt(stmt, &names);
        }
    }
    for dc in &mut program.derived_claims {
        resolve_in_prop(&mut dc.domain, &names);
        for v in &mut dc.values {
            resolve_in_value(&mut v.expr, &names);
        }
    }
}

fn resolve_in_prop(prop: &mut Prop, names: &BTreeSet<String>) {
    match prop {
        Prop::Claim { predicate, args } => {
            if names.contains(predicate.as_str()) {
                *prop = Prop::Defined {
                    name: DefinitionName::from(predicate.as_str()),
                    args: std::mem::take(args),
                };
            }
        }
        Prop::Defined { .. } | Prop::In(_, _) => {}
        Prop::And(props) | Prop::Or(props) => {
            for p in props {
                resolve_in_prop(p, names);
            }
        }
        Prop::Implies { left, right } | Prop::Xor(left, right) => {
            resolve_in_prop(left, names);
            resolve_in_prop(right, names);
        }
        Prop::Not(p) | Prop::Exists { body: p, .. } | Prop::Pre(p) => {
            resolve_in_prop(p, names);
        }
        Prop::Forall { source, body, .. } => {
            resolve_in_prop(source, names);
            resolve_in_prop(body, names);
        }
        Prop::Eq(l, r) | Prop::Neq(l, r) => {
            resolve_in_value(l, names);
            resolve_in_value(r, names);
        }
        Prop::Compare { left, right, .. } => {
            resolve_in_value(left, names);
            resolve_in_value(right, names);
        }
    }
}

fn resolve_in_value(value: &mut ValueExpr, names: &BTreeSet<String>) {
    match value {
        ValueExpr::Term(_) => {}
        // `ValueOf` is a value lookup against a claim, never a call.
        ValueExpr::ValueOf { default, .. } => {
            if let Some(d) = default {
                resolve_in_value(d, names);
            }
        }
        ValueExpr::Arith { left, right, .. } => {
            resolve_in_value(left, names);
            resolve_in_value(right, names);
        }
        ValueExpr::Sum { value, body, .. } => {
            resolve_in_value(value, names);
            resolve_in_prop(body, names);
        }
        ValueExpr::Extremum { body, .. } => resolve_in_prop(body, names),
        ValueExpr::Cond {
            when,
            then,
            otherwise,
        } => {
            resolve_in_prop(when, names);
            resolve_in_value(then, names);
            resolve_in_value(otherwise, names);
        }
        ValueExpr::Call { args, .. } => {
            for a in args {
                resolve_in_value(a, names);
            }
        }
    }
}

fn resolve_in_stmt(stmt: &mut Stmt, names: &BTreeSet<String>) {
    match stmt {
        Stmt::Require { prop: p, .. } | Stmt::BindOne { prop: p, .. } => resolve_in_prop(p, names),
        Stmt::Let { value, .. } => resolve_in_value(value, names),
        // State changes and emissions target predicates and intents,
        // never definitions.
        Stmt::Assert(_) | Stmt::Retract { .. } | Stmt::Emit(_) | Stmt::LetNewSubject { .. } => {}
        Stmt::For {
            collection, body, ..
        } => {
            resolve_in_value(collection, names);
            for inner in body {
                resolve_in_stmt(inner, names);
            }
        }
    }
}

/// Name-keyed lookup over a programme's definitions. Built once per
/// entry point (a proposal, an invariant evaluation, a static walk) and
/// threaded by reference.
#[derive(Clone, Copy)]
pub(crate) struct DefinitionTable<'a> {
    definitions: &'a [Definition],
}

impl<'a> DefinitionTable<'a> {
    pub(crate) fn new(definitions: &'a [Definition]) -> Self {
        Self { definitions }
    }

    /// A linear scan: programmes have few definitions. If profiling ever
    /// asks for an index, it goes behind this method.
    pub(crate) fn get(&self, name: &DefinitionName) -> Option<&'a Definition> {
        self.definitions.iter().find(|d| &d.name == name)
    }

    /// Run `f` on `name`'s definition, guarding against recursion.
    ///
    /// Returns `T::default()` when `name` is already on the stack (a cycle)
    /// or undeclared; otherwise pushes, runs, pops. It is a stack, not a
    /// visited set, because a polarity-sensitive walker must expand a
    /// definition again once it is off the stack. Every walker treats a
    /// cycle as contributing nothing, and validation rules out undeclared
    /// calls. `f` gets the whole [`Definition`] so it can match call
    /// arguments to parameters without a second lookup.
    pub(crate) fn enter<T: Default>(
        &self,
        name: &DefinitionName,
        seen: &mut BTreeSet<DefinitionName>,
        f: impl FnOnce(&'a Definition, &mut BTreeSet<DefinitionName>) -> T,
    ) -> T {
        if !seen.insert(name.clone()) {
            return T::default();
        }
        let out = match self.get(name) {
            Some(def) => f(def, seen),
            None => T::default(),
        };
        seen.remove(name);
        out
    }
}

/// Collect the definitions a proposition calls directly (not
/// transitively), including inside value positions such as `Sum` bodies.
pub(crate) fn defined_calls_in_prop(prop: &Prop, out: &mut BTreeSet<DefinitionName>) {
    match prop {
        Prop::Defined { name, .. } => {
            out.insert(name.clone());
        }
        Prop::Claim { .. } | Prop::In(_, _) => {}
        Prop::And(props) | Prop::Or(props) => {
            for p in props {
                defined_calls_in_prop(p, out);
            }
        }
        Prop::Implies { left, right } | Prop::Xor(left, right) => {
            defined_calls_in_prop(left, out);
            defined_calls_in_prop(right, out);
        }
        Prop::Not(p) | Prop::Exists { body: p, .. } | Prop::Pre(p) => {
            defined_calls_in_prop(p, out);
        }
        Prop::Forall { source, body, .. } => {
            defined_calls_in_prop(source, out);
            defined_calls_in_prop(body, out);
        }
        Prop::Eq(l, r) | Prop::Neq(l, r) => {
            defined_calls_in_value(l, out);
            defined_calls_in_value(r, out);
        }
        Prop::Compare { left, right, .. } => {
            defined_calls_in_value(left, out);
            defined_calls_in_value(right, out);
        }
    }
}

pub(crate) fn defined_calls_in_value(value: &ValueExpr, out: &mut BTreeSet<DefinitionName>) {
    match value {
        ValueExpr::Term(_) | ValueExpr::ValueOf { .. } => {}
        ValueExpr::Arith { left, right, .. } => {
            defined_calls_in_value(left, out);
            defined_calls_in_value(right, out);
        }
        ValueExpr::Sum { value, body, .. } => {
            defined_calls_in_value(value, out);
            defined_calls_in_prop(body, out);
        }
        ValueExpr::Extremum { body, .. } => defined_calls_in_prop(body, out),
        ValueExpr::Cond {
            when,
            then,
            otherwise,
        } => {
            defined_calls_in_prop(when, out);
            defined_calls_in_value(then, out);
            defined_calls_in_value(otherwise, out);
        }
        ValueExpr::Call { args, .. } => {
            for a in args {
                defined_calls_in_value(a, out);
            }
        }
    }
}

/// Order definitions so each one comes after the definitions it calls.
/// `Err` carries the sorted names in a call cycle. Calls to names that are
/// not definitions are ignored; other checks report those.
pub(crate) fn definition_topo_order(definitions: &[Definition]) -> Result<Vec<usize>, Vec<String>> {
    let position: HashMap<&str, usize> = definitions
        .iter()
        .enumerate()
        .map(|(i, d)| (d.name.as_str(), i))
        .collect();

    // Direct callee indices per definition.
    let callees: Vec<Vec<usize>> = definitions
        .iter()
        .map(|d| {
            let mut names = BTreeSet::new();
            defined_calls_in_prop(&d.body, &mut names);
            names
                .iter()
                .filter_map(|n| position.get(n.as_str()).copied())
                .collect()
        })
        .collect();

    // Iterative DFS with three-colour marking; `Done` nodes are pushed
    // post-order, which is exactly callee-before-caller.
    #[derive(Clone, Copy, PartialEq)]
    enum Mark {
        White,
        Grey,
        Done,
    }
    let mut marks = vec![Mark::White; definitions.len()];
    let mut order = Vec::with_capacity(definitions.len());

    for start in 0..definitions.len() {
        if marks[start] != Mark::White {
            continue;
        }
        // Stack of (node, next-callee-cursor).
        let mut stack = vec![(start, 0usize)];
        marks[start] = Mark::Grey;
        while let Some(&mut (node, ref mut cursor)) = stack.last_mut() {
            if let Some(&callee) = callees[node].get(*cursor) {
                *cursor += 1;
                match marks[callee] {
                    Mark::White => {
                        marks[callee] = Mark::Grey;
                        stack.push((callee, 0));
                    }
                    // A grey callee is on the current path: a cycle. Report
                    // only the stack from that callee up; entries below it
                    // merely reach the cycle.
                    Mark::Grey => {
                        let cycle_start = stack.iter().position(|&(i, _)| i == callee).unwrap_or(0);
                        let mut names: Vec<String> = stack[cycle_start..]
                            .iter()
                            .map(|&(i, _)| definitions[i].name.to_string())
                            .collect();
                        names.sort_unstable();
                        names.dedup();
                        return Err(names);
                    }
                    Mark::Done => {}
                }
            } else {
                marks[node] = Mark::Done;
                order.push(node);
                stack.pop();
            }
        }
    }
    Ok(order)
}
