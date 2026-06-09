//! The canonical home for claim-discipline machinery: the lowering that
//! turns declared disciplines into ordinary generated invariants, the
//! deterministic names those invariants carry into rejection reasons
//! and audit rows, and the effective append-only set the static retract
//! ban consults. One place, so the enforcement a declaration implies
//! cannot drift between the validator, the formatter, and the
//! legibility surfaces.

use std::collections::BTreeSet;

use crate::ir::{
    Discipline, Invariant, InvariantOrigin, PredicateDecl, PredicateName, Program, Prop, Term,
    ValueExpr, Var,
};

/// Lower every declared discipline that generates an invariant into
/// [`Program::invariants`], with [`InvariantOrigin::Discipline`].
/// `unique by` and `current pointer by` each lower to one uniqueness
/// invariant on their own predicate; `superseded via L` lowers no-fork
/// on `L` (uniqueness on the prior - the second field, per the
/// `(successor, prior)` convention the worked examples established).
/// `append only` lowers nothing - it is enforced statically.
///
/// Materialising into `Program.invariants` is the point: proposal
/// checking, predicate-scoped loading, the audit row's
/// `invariants_checked`, `guarantees`, `controls`, and `explain` all
/// see generated invariants with no caller changes - no hidden
/// enforcement layer. The formatter omits Discipline-origin invariants
/// (the declaration clauses imply them) and reparsing regenerates them,
/// so round-trip holds.
///
/// Idempotent: a generated name already present with Discipline origin
/// is skipped. The same name present with Authored origin is left to
/// surface as the ordinary duplicate-declaration error. Clauses that
/// cannot be lowered soundly (unknown field names, no value fields,
/// a malformed lineage predicate) are skipped here; `Program::validate`
/// reports each with its own error.
pub fn lower_disciplines(program: &mut Program) {
    let mut generated: Vec<Invariant> = Vec::new();
    for decl in &program.predicates {
        for discipline in &decl.disciplines {
            match discipline {
                Discipline::UniqueBy { fields } | Discipline::CurrentPointerBy { fields } => {
                    if let Some(inv) = unique_invariant(decl, fields) {
                        generated.push(inv);
                    }
                }
                Discipline::AppendOnly => {}
                Discipline::SupersededVia { lineage } => {
                    let Some(lineage_decl) = program.predicates.iter().find(|p| p.name == *lineage)
                    else {
                        continue;
                    };
                    if lineage_decl.args.len() != 2 {
                        continue;
                    }
                    let prior_field = lineage_decl.args[1].name.clone();
                    if let Some(inv) = unique_invariant(lineage_decl, &[prior_field]) {
                        generated.push(inv);
                    }
                }
            }
        }
    }
    // Generated invariants go FIRST: a discipline is a precondition of
    // sense for the authored rules (uniqueness is what makes lookups
    // and aggregates well-defined), so when a proposal violates both,
    // the rejection names the root cause, not a knock-on. Dedupe both
    // against the programme (idempotence across calls) and within this
    // pass (a duplicate clause still gets its validation error, but
    // the generated IR never carries the duplicate) - invalid input
    // shapes must not leak into generated IR.
    let mut fresh: Vec<Invariant> = Vec::new();
    for inv in generated {
        let already = program
            .invariants
            .iter()
            .chain(fresh.iter())
            .any(|existing| existing.name == inv.name && existing.origin == inv.origin);
        if !already {
            fresh.push(inv);
        }
    }
    if !fresh.is_empty() {
        fresh.append(&mut program.invariants);
        program.invariants = fresh;
    }
}

/// The deterministic name a uniqueness lowering carries:
/// `{snake(Predicate)}_unique_by_{fields joined by _}`. Boring on
/// purpose - it appears in rejection reasons and audit rows, so it must
/// be stable, readable, and traceable back to the declaration.
pub(crate) fn unique_invariant_name(predicate: &PredicateName, fields: &[String]) -> String {
    format!(
        "{}_unique_by_{}",
        snake_case(predicate.as_str()),
        fields.join("_")
    )
}

/// The uniqueness invariant for `decl` keyed by `fields`:
/// `P(k.., a..) and P(k.., b..) implies (a1 = b1 and ...)` - full
/// agreement, the keys determine the whole claim. `None` when the
/// clause cannot be lowered soundly (an unknown field, or no value
/// fields left); validation owns the diagnostic.
fn unique_invariant(decl: &PredicateDecl, fields: &[String]) -> Option<Invariant> {
    let is_key: Vec<bool> = decl.args.iter().map(|a| fields.contains(&a.name)).collect();
    let all_known = fields
        .iter()
        .all(|f| decl.args.iter().any(|a| a.name == *f));
    if !all_known || is_key.iter().all(|k| *k) || fields.is_empty() {
        return None;
    }

    let mut args_a: Vec<Term> = Vec::with_capacity(decl.args.len());
    let mut args_b: Vec<Term> = Vec::with_capacity(decl.args.len());
    let mut agreements: Vec<Prop> = Vec::new();
    for (arg, key) in decl.args.iter().zip(&is_key) {
        if *key {
            let shared = Var::from(arg.name.as_str());
            args_a.push(Term::Var(shared.clone()));
            args_b.push(Term::Var(shared));
        } else {
            let a = Var::from(format!("{}_a", arg.name));
            let b = Var::from(format!("{}_b", arg.name));
            agreements.push(Prop::Eq(
                Box::new(ValueExpr::Term(Term::Var(a.clone()))),
                Box::new(ValueExpr::Term(Term::Var(b.clone()))),
            ));
            args_a.push(Term::Var(a));
            args_b.push(Term::Var(b));
        }
    }

    let left = Prop::And(vec![
        Prop::Claim {
            predicate: decl.name.clone(),
            args: args_a,
        },
        Prop::Claim {
            predicate: decl.name.clone(),
            args: args_b,
        },
    ]);
    let right = match agreements.pop() {
        Some(only) if agreements.is_empty() => only,
        Some(last) => {
            agreements.push(last);
            Prop::And(agreements)
        }
        None => return None,
    };
    Some(Invariant {
        name: unique_invariant_name(&decl.name, fields).into(),
        version: 1,
        body: Prop::Implies {
            left: Box::new(left),
            right: Box::new(right),
        },
        origin: InvariantOrigin::Discipline,
    })
}

/// Every (predicate, generated-invariant-name) pair the programme's
/// disciplines imply, for clauses that lower soundly. The validator
/// checks each is present with Discipline origin, so hand-built IR
/// that skipped `lower_disciplines` fails loudly instead of carrying
/// silently unenforced commitments. Derived from the same clause walk
/// as the lowering, so the expectation and the generation cannot
/// drift.
pub(crate) fn expected_generated_invariants(program: &Program) -> Vec<(PredicateName, String)> {
    let mut out = Vec::new();
    for decl in &program.predicates {
        for discipline in &decl.disciplines {
            match discipline {
                Discipline::UniqueBy { fields } | Discipline::CurrentPointerBy { fields } => {
                    if unique_invariant(decl, fields).is_some() {
                        out.push((decl.name.clone(), unique_invariant_name(&decl.name, fields)));
                    }
                }
                Discipline::AppendOnly => {}
                Discipline::SupersededVia { lineage } => {
                    let Some(lineage_decl) = program.predicates.iter().find(|p| p.name == *lineage)
                    else {
                        continue;
                    };
                    if lineage_decl.args.len() != 2 {
                        continue;
                    }
                    let prior = vec![lineage_decl.args[1].name.clone()];
                    if unique_invariant(lineage_decl, &prior).is_some() {
                        out.push((
                            lineage_decl.name.clone(),
                            unique_invariant_name(&lineage_decl.name, &prior),
                        ));
                    }
                }
            }
        }
    }
    out
}

/// Generated-invariant-name -> the declaration clause that implied it,
/// rendered for the legibility surfaces ("predicate CurrentFigure,
/// current pointer by (owner)") - so a rejection naming a generated
/// invariant traces back to its declaration in one hop.
pub(crate) fn discipline_provenance(
    program: &Program,
) -> std::collections::HashMap<String, String> {
    let mut out = std::collections::HashMap::new();
    for decl in &program.predicates {
        for discipline in &decl.disciplines {
            match discipline {
                Discipline::UniqueBy { fields } => {
                    if unique_invariant(decl, fields).is_some() {
                        out.insert(
                            unique_invariant_name(&decl.name, fields),
                            format!("predicate {}, unique by ({})", decl.name, fields.join(", ")),
                        );
                    }
                }
                Discipline::CurrentPointerBy { fields } => {
                    if unique_invariant(decl, fields).is_some() {
                        out.insert(
                            unique_invariant_name(&decl.name, fields),
                            format!(
                                "predicate {}, current pointer by ({})",
                                decl.name,
                                fields.join(", ")
                            ),
                        );
                    }
                }
                Discipline::AppendOnly => {}
                Discipline::SupersededVia { lineage } => {
                    let Some(lineage_decl) = program.predicates.iter().find(|l| l.name == *lineage)
                    else {
                        continue;
                    };
                    if lineage_decl.args.len() != 2 {
                        continue;
                    }
                    let prior = vec![lineage_decl.args[1].name.clone()];
                    if unique_invariant(lineage_decl, &prior).is_some() {
                        out.insert(
                            unique_invariant_name(&lineage_decl.name, &prior),
                            format!("predicate {}, superseded via {}", decl.name, lineage),
                        );
                    }
                }
            }
        }
    }
    out
}

/// The predicates no transformation may retract: those declared
/// `append only`, plus every lineage predicate named by a
/// `superseded via` (lineage is the doctrine's append-only third
/// class). Consulted by the static retract ban in `Program::validate`.
pub(crate) fn append_only_predicates(program: &Program) -> BTreeSet<PredicateName> {
    let mut out = BTreeSet::new();
    for decl in &program.predicates {
        for discipline in &decl.disciplines {
            match discipline {
                Discipline::AppendOnly => {
                    out.insert(decl.name.clone());
                }
                Discipline::SupersededVia { lineage } => {
                    out.insert(lineage.clone());
                }
                Discipline::UniqueBy { .. } | Discipline::CurrentPointerBy { .. } => {}
            }
        }
    }
    out
}

/// `OfficialPrice` -> `official_price`. ASCII CamelCase only - the
/// shape every predicate name in the surface convention takes.
fn snake_case(name: &str) -> String {
    let mut out = String::with_capacity(name.len() + 4);
    for (i, c) in name.chars().enumerate() {
        if c.is_ascii_uppercase() {
            if i > 0 {
                out.push('_');
            }
            out.push(c.to_ascii_lowercase());
        } else {
            out.push(c);
        }
    }
    out
}
