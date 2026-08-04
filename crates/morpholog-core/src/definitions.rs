//! The canonical home for definition machinery: name lookup
//! ([`DefinitionTable`]), the definition reference graph
//! ([`definition_topo_order`], shared by cycle detection, kind
//! inference, and the depth budget), and the direct-call collector they
//! build on. Every subsystem that must see through a [`Prop::Defined`]
//! call resolves it here, so there is exactly one notion of "what does
//! this call expand to" - the evaluator, the failure walk, the static
//! checks, and the analysis walkers cannot drift apart on it.
//!
//! [`DefinitionTable`] is a borrowed view, not a built structure: it is
//! `Copy`, constructed wherever a definitions slice is in hand, and
//! passed by value.

use std::collections::{BTreeSet, HashMap};

use crate::ir::{Definition, DefinitionName, Program, Prop, ValueExpr};

/// Resolve claim-shaped references against the programme's definitions:
/// every `Prop::Claim` whose name is a declared definition becomes the
/// `Prop::Defined` call it means. The parser runs this at the end of
/// lowering (a reference can precede the definition it names, so
/// resolution needs the whole programme); hand-built IR that authors
/// `Prop::Claim` nodes directly runs it before validating, or constructs
/// calls with `ir_builder::defined` and skips it.
///
/// Only proposition positions resolve. `admit` / `retract` / `emit`
/// targets and `value` lookups stay claim-shaped: a definition is
/// proposition-valued only, so a definition name there surfaces the
/// dedicated unresolved-call error with its own guidance.
pub fn resolve_defined_calls(program: &mut Program) {
    let names: BTreeSet<String> = program
        .definitions
        .iter()
        .map(|d| d.name.to_string())
        .collect();
    if names.is_empty() {
        return;
    }
    let mut resolver = ResolveCalls { names: &names };
    for def in &mut program.definitions {
        crate::fold::rewrite_prop(&mut def.body, &mut resolver);
    }
    for inv in &mut program.invariants {
        crate::fold::rewrite_prop(&mut inv.body, &mut resolver);
    }
    for t in &mut program.transformations {
        for stmt in &mut t.body {
            crate::fold::rewrite_stmt(stmt, &mut resolver);
        }
    }
    for dc in &mut program.derived_claims {
        crate::fold::rewrite_prop(&mut dc.domain, &mut resolver);
        for v in &mut dc.values {
            crate::fold::rewrite_value(&mut v.expr, &mut resolver);
        }
    }
}

/// Claim-shaped references to a declared definition become the calls
/// they mean. Only the node matters here - the descent is
/// [`crate::fold`]'s, so a new IR variant does not reach this file.
struct ResolveCalls<'a> {
    names: &'a BTreeSet<String>,
}

impl crate::fold::Rewrite for ResolveCalls<'_> {
    fn prop(&mut self, prop: &mut Prop) -> crate::fold::Descend {
        if let Prop::Claim { predicate, args } = prop
            && self.names.contains(predicate.as_str())
        {
            *prop = Prop::Defined {
                name: DefinitionName::from(predicate.as_str()),
                args: std::mem::take(args),
            };
        }
        crate::fold::Descend::Into
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

    /// Deliberately a scan. Programmes name few definitions and no
    /// measured hot path asks for more, so a second index would be
    /// weight without evidence. Lookup is centralised here precisely
    /// so indexing can go behind this method if profiling ever forces
    /// it - the point of one table is one authority for "which
    /// definition is this".
    pub(crate) fn get(&self, name: &DefinitionName) -> Option<&'a Definition> {
        self.definitions.iter().find(|d| &d.name == name)
    }

    /// Run `f` against `name`'s body under the recursion-STACK guard
    /// every static walker shares: returns `T::default()` when the
    /// name is already on the stack (a cycle) or undeclared;
    /// otherwise pushes, runs, pops. A stack guard, not a visited
    /// set - a polarity-sensitive walker must re-expand a definition
    /// reached again once it is off the stack. `Default` for both
    /// refusals is the shared policy on purpose: validation already
    /// guarantees every call resolves, so the undeclared arm is
    /// unreachable on a compiled programme, and every walker treats
    /// a cycle as contributing nothing.
    /// The callback receives the whole [`Definition`], not just its
    /// body: a walker that needs the parameter list to match call
    /// arguments against would otherwise have to look the definition
    /// up a second time, which is how a third lookup site grew here
    /// once already.
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
/// transitively). The recursion mirrors `predicates_referenced_by_prop`:
/// every `Prop` position that can carry a sub-proposition is walked,
/// including `Sum` bodies on the value sort.
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
        ValueExpr::Sum { body, .. } | ValueExpr::Extremum { body, .. } => {
            defined_calls_in_prop(body, out)
        }
        ValueExpr::Cond {
            when,
            then,
            otherwise,
        } => {
            defined_calls_in_prop(when, out);
            defined_calls_in_value(then, out);
            defined_calls_in_value(otherwise, out);
        }
        ValueExpr::PeriodIndex { anchor, span, at } => {
            defined_calls_in_value(anchor, out);
            defined_calls_in_value(span, out);
            defined_calls_in_value(at, out);
        }
        ValueExpr::Abs(operand) => defined_calls_in_value(operand, out),
        ValueExpr::Round { value, quantum } => {
            defined_calls_in_value(value, out);
            defined_calls_in_value(quantum, out);
        }
    }
}

/// Order definitions so every definition appears after the definitions
/// its body calls. `Err` carries the names participating in a reference
/// cycle, sorted, for the validation error. Calls to names that are not
/// definitions (predicates, or simply undeclared) are ignored here -
/// they are the resolution pass's and the reference check's concern.
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
                    // A grey callee is on the current path: a cycle. The
                    // cycle's members are the stack's sub-path from that
                    // callee back to the top - NOT the whole grey stack,
                    // whose lower entries merely *reach* the cycle and
                    // would mislead the diagnostic. Names sorted for
                    // determinism.
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
