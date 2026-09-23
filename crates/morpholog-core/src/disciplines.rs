//! Claim disciplines: lowering declared disciplines into ordinary
//! generated invariants and definitions, the names those carry into
//! rejections and audit rows, and the append-only set the retract ban
//! consults. Kept in one place so the validator, the formatter, and the
//! inspect views agree on what a declaration enforces.

use std::collections::BTreeSet;

use crate::ir::{
    CompareOp, Definition, Discipline, Invariant, InvariantOrigin, OrderedDomain, PredicateArgKind,
    PredicateDecl, PredicateName, Program, Prop, Term, ValueExpr, Var,
};

/// Add the definitions declared disciplines generate (the `effective
/// by` selectors) to [`Program::definitions`].
///
/// Must run before call resolution: a call is spelled like a claim, so a
/// selector not yet generated would resolve as an undeclared predicate.
/// [`lower_disciplines`] runs after resolution instead.
///
/// Idempotent: a generated definition already present is left alone.
pub fn lower_discipline_definitions(program: &mut Program) {
    let mut generated: Vec<Definition> = Vec::new();
    for decl in &program.predicates {
        for discipline in &decl.disciplines {
            if let Discipline::EffectiveBy { keys, on, .. } = discipline
                && let Some(def) = in_force_define(decl, keys, on)
                // Match on origin, not just name: an authored definition
                // of that name is a collision, not a sign lowering ran.
                && !program
                    .definitions
                    .iter()
                    .any(|d| d.name == def.name && d.origin == crate::ir::DefinitionOrigin::Discipline)
                && !generated.iter().any(|d| d.name == def.name)
            {
                generated.push(def);
            }
        }
    }
    program.definitions.extend(generated);
}

/// One uniqueness commitment a discipline clause implies: the
/// predicate it constrains (the lineage for `superseded via`, else the
/// declaring one), its key fields, and the clause that carried it. A
/// clause with an unknown or ill-shaped lineage yields nothing; the
/// validator reports it. Listed in declaration order, the order the
/// generated names appear in rejections and audit rows.
struct Uniqueness<'p> {
    target: &'p PredicateDecl,
    fields: Vec<String>,
    declared_on: &'p PredicateDecl,
    clause: &'p Discipline,
}

fn uniqueness_clauses(program: &Program) -> Vec<Uniqueness<'_>> {
    let mut out = Vec::new();
    for decl in &program.predicates {
        for clause in &decl.disciplines {
            let (target, fields) = match clause {
                Discipline::UniqueBy { fields } | Discipline::CurrentPointerBy { fields } => {
                    (decl, fields.clone())
                }
                // One version per key per date. Without it two rows tie
                // for "latest" and the selector returns both.
                Discipline::EffectiveBy { keys, on, .. } => {
                    let mut fields = keys.clone();
                    fields.push(on.clone());
                    (decl, fields)
                }
                Discipline::SupersededVia { lineage } => {
                    let Some(lineage_decl) = program.predicates.iter().find(|p| p.name == *lineage)
                    else {
                        continue;
                    };
                    if lineage_decl.args.len() != 2 {
                        continue;
                    }
                    (lineage_decl, vec![lineage_decl.args[1].name.clone()])
                }
                Discipline::AppendOnly => continue,
            };
            out.push(Uniqueness {
                target,
                fields,
                declared_on: decl,
                clause,
            });
        }
    }
    out
}

/// Add the invariants declared disciplines generate to
/// [`Program::invariants`], with [`InvariantOrigin::Discipline`].
/// `unique by`, `current pointer by`, and `effective by` each lower to
/// one uniqueness invariant on their own predicate. `superseded via L`
/// lowers no-fork on `L`: uniqueness on the prior, its second field.
/// `append only` lowers nothing; it is enforced statically.
///
/// Because they are ordinary invariants, proposal checking, scoped
/// loading, audit, and the inspect views all see them. The formatter
/// omits them and reparsing regenerates them, so round-trip holds.
///
/// Idempotent: a generated name already present with Discipline origin
/// is skipped. The same name with Authored origin is left for the
/// duplicate-declaration error. Clauses that cannot lower soundly are
/// skipped; `Program::validate` reports each.
pub fn lower_disciplines(program: &mut Program) {
    let generated: Vec<Invariant> = uniqueness_clauses(program)
        .iter()
        .filter_map(|u| unique_invariant(u.target, &u.fields))
        .collect();
    // Generated invariants go first: uniqueness is what makes lookups
    // and sums well-defined, so a rejection names the root cause rather
    // than a knock-on. Dedupe against the programme and within this
    // pass, so a duplicate clause never reaches the generated IR.
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

/// The name a uniqueness lowering carries:
/// `{snake(Predicate)}_unique_by_{fields joined by _}`. It appears in
/// rejections and audit rows, so it must stay stable and readable.
pub(crate) fn unique_invariant_name(predicate: &PredicateName, fields: &[String]) -> String {
    format!(
        "{}_unique_by_{}",
        snake_case(predicate.as_str()),
        fields.join("_")
    )
}

/// The name of the selector `effective by` generates.
pub fn in_force_define_name(predicate: &PredicateName) -> String {
    format!("{}_in_force_on", snake_case(predicate.as_str()))
}

/// The in-force-on-a-date selector for `decl`, keyed by `keys` and dated
/// by `on`: the dated claim, an on-or-before bound, and no strictly
/// later version. A definition, because the author calls it.
///
/// Parameters are the keys, an as-of date, then every payload field. The
/// as-of binds nothing, so it must arrive bound at each call. Callers
/// wildcard the payload fields they do not want.
///
/// `None` when the clause cannot be lowered soundly (an unknown field, or
/// a key that is also the date); validation owns the diagnostic.
fn in_force_define(decl: &PredicateDecl, keys: &[String], on: &str) -> Option<Definition> {
    let known = |f: &String| decl.args.iter().any(|a| a.name == *f);
    if !keys.iter().all(known)
        || !decl.args.iter().any(|a| a.name == on)
        || keys.contains(&on.to_string())
    {
        return None;
    }
    let domain = match decl.args.iter().find(|a| a.name == on)?.kind {
        PredicateArgKind::Date => OrderedDomain::Date,
        PredicateArgKind::Timestamp => OrderedDomain::Timestamp,
        // Validation refuses any other kind; don't invent an ordering.
        _ => return None,
    };

    // Must not clash with a field name, or a field called `as_of` would
    // yield a DuplicateParameter in a definition the author never wrote.
    // Append underscores until unique.
    let taken: Vec<&str> = decl.args.iter().map(|a| a.name.as_str()).collect();
    let fresh = |base: &str, also: &[&Var]| {
        let mut name = base.to_string();
        while taken.contains(&name.as_str()) || also.iter().any(|v| v.as_str() == name) {
            name.push('_');
        }
        Var::from(name)
    };
    let as_of = fresh("as_of", &[]);
    let effective = fresh("effective_from", &[&as_of]);
    let later = fresh("later_effective_from", &[&as_of, &effective]);

    // Positional: the date field and keys can sit anywhere.
    let mut parameters: Vec<Var> = Vec::new();
    let mut outer: Vec<Term> = Vec::new();
    let mut inner: Vec<Term> = Vec::new();
    let mut payload: Vec<Var> = Vec::new();
    for arg in &decl.args {
        if keys.contains(&arg.name) {
            let k = Var::from(arg.name.as_str());
            parameters.push(k.clone());
            outer.push(Term::Var(k.clone()));
            inner.push(Term::Var(k));
        } else if arg.name == on {
            outer.push(Term::Var(effective.clone()));
            inner.push(Term::Var(later.clone()));
        } else {
            let v = Var::from(arg.name.as_str());
            payload.push(v.clone());
            outer.push(Term::Var(v));
            // Only the later version's date matters, not its payload.
            inner.push(Term::Wildcard);
        }
    }
    parameters.push(as_of.clone());
    parameters.extend(payload);

    let var = |v: &Var| Box::new(ValueExpr::Term(Term::Var(v.clone())));
    let no_later = Prop::Not(Box::new(Prop::Exists {
        binding: later.clone(),
        body: Box::new(Prop::And(vec![
            Prop::Claim {
                predicate: decl.name.clone(),
                args: inner,
            },
            Prop::Compare {
                op: CompareOp::Le,
                domain,
                left: var(&later),
                right: var(&as_of),
            },
            Prop::Compare {
                op: CompareOp::Gt,
                domain,
                left: var(&later),
                right: var(&effective),
            },
        ])),
    }));

    Some(Definition {
        origin: crate::ir::DefinitionOrigin::Discipline,
        name: in_force_define_name(&decl.name).into(),
        parameters,
        body: Prop::And(vec![
            Prop::Claim {
                predicate: decl.name.clone(),
                args: outer,
            },
            Prop::Compare {
                op: CompareOp::Le,
                domain,
                left: var(&effective),
                right: var(&as_of),
            },
            no_later,
        ]),
    })
}

/// The uniqueness invariant for `decl` keyed by `fields`:
/// `P(k.., a..) and P(k.., b..) implies (a1 = b1 and ...)`, so the keys
/// determine the whole claim. `None` for an unknown field or no value
/// fields left; validation reports it.
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
        totality_for: None,
        name: unique_invariant_name(&decl.name, fields).into(),
        version: 1,
        body: Prop::Implies {
            left: Box::new(left),
            right: Box::new(right),
        },
        origin: InvariantOrigin::Discipline,
    })
}

/// Every (predicate, generated-invariant-name) pair the disciplines
/// imply. The validator checks each is present, so hand-built IR that
/// skipped `lower_disciplines` fails instead of going unenforced. Uses
/// the same clause walk as the lowering, so the two cannot drift.
pub(crate) fn expected_generated_invariants(program: &Program) -> Vec<(PredicateName, String)> {
    uniqueness_clauses(program)
        .iter()
        .filter(|u| unique_invariant(u.target, &u.fields).is_some())
        .map(|u| {
            (
                u.target.name.clone(),
                unique_invariant_name(&u.target.name, &u.fields),
            )
        })
        .collect()
}

/// Generated invariant name -> the declaration clause that implied it,
/// rendered as "predicate CurrentFigure, current pointer by (owner)",
/// so a rejection traces back to its declaration.
pub(crate) fn discipline_provenance(
    program: &Program,
) -> std::collections::HashMap<String, String> {
    uniqueness_clauses(program)
        .iter()
        .filter(|u| unique_invariant(u.target, &u.fields).is_some())
        .filter_map(|u| {
            let declared = &u.declared_on.name;
            let clause = match u.clause {
                Discipline::UniqueBy { fields } => {
                    format!("predicate {declared}, unique by ({})", fields.join(", "))
                }
                Discipline::CurrentPointerBy { fields } => {
                    format!(
                        "predicate {declared}, current pointer by ({})",
                        fields.join(", ")
                    )
                }
                Discipline::SupersededVia { lineage } => {
                    format!("predicate {declared}, superseded via {lineage}")
                }
                Discipline::EffectiveBy { .. } | Discipline::AppendOnly => return None,
            };
            Some((unique_invariant_name(&u.target.name, &u.fields), clause))
        })
        .collect()
}

/// The predicates no transformation may retract: those declared
/// `append only`, plus every lineage named by a `superseded via`.
/// Consulted by the static retract ban in `Program::validate`.
pub(crate) fn append_only_predicates(program: &Program) -> BTreeSet<PredicateName> {
    let mut out = BTreeSet::new();
    for decl in &program.predicates {
        for discipline in &decl.disciplines {
            match discipline {
                // Effective-dating says nothing about retraction; that is
                // `append only`'s business.
                Discipline::EffectiveBy { .. } => {}
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

/// `OfficialPrice` -> `official_price`. ASCII CamelCase only.
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
