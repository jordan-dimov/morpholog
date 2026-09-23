//! Lint hints: shapes that are usually, but not always, a mistake.
//! `morpholog check` prints them as hints; `--strict` makes them errors.
//! A [`crate::ValidationError`] is different: it means the programme
//! cannot mean what it says.

use std::collections::BTreeSet;

use crate::analysis::{has_admission_gate, predicates_written_by};
use crate::compiled::CompiledProgram;
use crate::definitions::DefinitionTable;
use crate::disciplines::append_only_predicates;
use crate::ir::{
    DefinitionName, Discipline, Invariant, InvariantOrigin, PredicateName, Program, Prop, Term,
    ValueExpr,
};

/// One lint finding. See the module doc for the error-vs-lint line.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Lint {
    /// An invariant whose antecedent positively references an
    /// append-only predicate and whose consequent positively requires a
    /// current-pointer predicate. Retracting the pointer would make
    /// admitted records violate the rule, so the retraction is blocked
    /// or history must be rewritten. A hint, because re-checking standing
    /// over old records can be deliberate; otherwise move the check into
    /// the admitting transformation's gate.
    ///
    /// Forward direction only. The reverse ("the pointer names a figure
    /// that exists") is fine: retracting the pointer makes it vacuous.
    GateVsInvariant {
        invariant: String,
        append_only: String,
        pointer: String,
    },

    /// An authored invariant whose antecedent depends on predicates no
    /// transformation admits, so it cannot fire on a fresh ledger.
    /// `missing` lists them together: every branch of an `or`, each
    /// required conjunct of an `and`. Not proof of a dead rule: stored
    /// claims may still match.
    UnsuppliedAntecedent {
        invariant: String,
        missing: Vec<String>,
    },

    /// An authored invariant whose antecedent selects "the version of `P`
    /// in force at a date" (a dated `P` on or before it, with no strictly
    /// later `P`), while no other invariant backs `P`'s totality, either
    /// by declaring `total over` or by guaranteeing a dated `P` exists.
    /// Where no version is in force, the rule silently does not apply.
    /// `predicates` names only the unbacked predicates. Whether the
    /// backstop covers the same dates is not checked.
    GoverningSelectionWithoutTotality {
        invariant: String,
        predicates: Vec<String>,
    },

    /// A predicate declared `effective by` (not `partial`) with no
    /// invariant declaring `total over` it. Where no version is in force
    /// the selector returns nothing, so every rule reading it quietly
    /// does not apply, and the source does not say if that is intended.
    ///
    /// A rule that reads `P`'s own selector cannot declare `P`'s
    /// totality: it only applies where a version already exists. Such a
    /// declaration is ignored.
    ///
    /// A hint, because a rule that should not apply before the first
    /// version is a valid model.
    EffectiveWithoutDeclaredTotality { predicate: String },

    /// A transformation of this programme writes a predicate another
    /// programme also writes. In one database they share those rows, and
    /// neither is bound by the other's gates. Reads are not findings. A
    /// hint, because sharing may be deliberate. `guarded` says only
    /// whether a transformation has a top-level gate, not how strong.
    SharedWriter {
        transformation: String,
        predicate: String,
        guarded: bool,
        other_program: String,
        other_writers: Vec<SharedWriterPeer>,
    },
}

/// A writing transformation on the other side of a [`Lint::SharedWriter`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SharedWriterPeer {
    pub transformation: String,
    pub guarded: bool,
}

/// The cross-programme findings for `this` against `other`: one per
/// (transformation, predicate) of `this` that `other` also writes, in
/// declaration order. The caller names which file `other` came from.
pub fn shared_writer_lints(this: &Program, other: &Program) -> Vec<Lint> {
    let writers: Vec<(&crate::ir::Transformation, BTreeSet<PredicateName>)> = other
        .transformations
        .iter()
        .map(|t| (t, predicates_written_by(t)))
        .collect();
    let mut out = Vec::new();
    for t in &this.transformations {
        for predicate in predicates_written_by(t) {
            let other_writers: Vec<SharedWriterPeer> = writers
                .iter()
                .filter(|(_, written)| written.contains(&predicate))
                .map(|(o, _)| SharedWriterPeer {
                    transformation: o.name.to_string(),
                    guarded: has_admission_gate(o),
                })
                .collect();
            if other_writers.is_empty() {
                continue;
            }
            out.push(Lint::SharedWriter {
                transformation: t.name.to_string(),
                predicate: predicate.to_string(),
                guarded: has_admission_gate(t),
                other_program: other.name.to_string(),
                other_writers,
            });
        }
    }
    out
}

impl std::fmt::Display for Lint {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Lint::EffectiveWithoutDeclaredTotality { predicate } => write!(
                f,
                "`{predicate}` is effective-dated but no invariant declares `total over \
                 {predicate}`: where no version is in force the generated selector matches \
                 nothing, so every rule reading it passes vacuously. Mark the invariant that \
                 guarantees a version exists with `total over {predicate}`, or - if the \
                 gaps are intended, and rules reading it are meant not to apply there - say \
                 so on the declaration with `effective by (...) on (...) partial`"
            ),
            Lint::GateVsInvariant {
                invariant,
                append_only,
                pointer,
            } => write!(
                f,
                "invariant `{invariant}` conditions append-only `{append_only}` \
                 records on the presence of retractable pointer `{pointer}`; \
                 retracting `{pointer}` would make already-admitted \
                 `{append_only}` records violate this rule - blocking the \
                 retraction or forcing history to be rewritten. If continuous \
                 re-checking is intended (a compliance shape), keep it \
                 deliberately; otherwise the check belongs in the admitting \
                 transformation's gate"
            ),
            Lint::UnsuppliedAntecedent { invariant, missing } => {
                let names = missing
                    .iter()
                    .map(|m| format!("`{m}`"))
                    .collect::<Vec<_>>()
                    .join(", ");
                write!(
                    f,
                    "invariant `{invariant}` references predicates the \
                     current programme never admits in an implication \
                     antecedent: {names}. On a fresh ledger that \
                     implication is vacuous; persisted or historically \
                     admitted claims may still match it, so this is a \
                     hint - keep them if forward-declared or supplied by \
                     migration, otherwise check for a typo or a dropped \
                     transformation"
                )
            }
            Lint::GoverningSelectionWithoutTotality {
                invariant,
                predicates,
            } => {
                let names = predicates
                    .iter()
                    .map(|p| format!("`{p}`"))
                    .collect::<Vec<_>>()
                    .join(", ");
                write!(
                    f,
                    "invariant `{invariant}` appears to select the governing \
                     version of {names} in force at a coordinate (the \
                     not-a-later-one pattern), but no other invariant has \
                     the recognised totality shape for it. When no version \
                     is in force at a coordinate, this rule may pass \
                     vacuously - the edge the selection cannot see. Add a \
                     totality backstop (an invariant guaranteeing every \
                     governed coordinate an effective version, e.g. \
                     `... implies (exists e: {names_first}(..., e) and e \
                     on_or_before d)` - `at_or_before` for timestamps) \
                     beside the ordinary action's `require` gate",
                    names_first = predicates.first().map(String::as_str).unwrap_or("P"),
                )
            }
            Lint::SharedWriter {
                transformation,
                predicate,
                guarded,
                other_program,
                other_writers,
            } => {
                let gate = |g: bool| {
                    if g {
                        "has an admission gate"
                    } else {
                        "ungated"
                    }
                };
                let peers = other_writers
                    .iter()
                    .map(|p| format!("`{}`: {}", p.transformation, gate(p.guarded)))
                    .collect::<Vec<_>>()
                    .join(", ");
                write!(
                    f,
                    "`{predicate}` is writable by another programme `{other_program}` \
                     ({peers}); `{transformation}` here also writes it and {this}. Both \
                     programmes hold write authority over the same persisted predicate, \
                     each ungoverned by the other's gates",
                    this = if *guarded {
                        "has an admission gate"
                    } else {
                        "is ungated"
                    },
                )
            }
        }
    }
}

/// Collect every lint finding for a programme, deterministically:
/// `effective by` findings first, then each invariant's findings in
/// declaration order (gate-vs-invariant, unsupplied antecedent, then
/// governing selection).
pub fn lints(compiled: &CompiledProgram) -> Vec<Lint> {
    let program = compiled.program();
    let definitions = compiled.definition_table();
    let append_only = append_only_predicates(program);
    let pointers: BTreeSet<PredicateName> = program
        .predicates
        .iter()
        .filter(|d| {
            d.disciplines
                .iter()
                .any(|disc| matches!(disc, Discipline::CurrentPointerBy { .. }))
        })
        .map(|d| d.name.clone())
        .collect();
    let declared = crate::analysis::declared_supplier_predicates(program);
    let do_gate = !append_only.is_empty() && !pointers.is_empty();

    // The totality each invariant declares, by position so a consumer can
    // skip the invariant under test. Dropped when the rule reads the
    // selector for the predicate it vouches for: it only applies where a
    // version exists, so it cannot be why one exists.
    let declared_totality: Vec<Option<PredicateName>> = program
        .invariants
        .iter()
        .map(|inv| {
            if inv.origin != InvariantOrigin::Authored {
                return None;
            }
            let target = inv.totality_for.clone()?;
            let selector: DefinitionName = crate::in_force_define_name(&target).into();
            (!calls_definition(&inv.body, &selector, definitions)).then_some(target)
        })
        .collect();

    // The totality recognised by shape, for programmes that declare
    // nothing. By position, since a backstop must be a different rule.
    let witnesses: Vec<BTreeSet<PredicateName>> = program
        .invariants
        .iter()
        .map(|inv| {
            if inv.origin == InvariantOrigin::Authored {
                crate::analysis::guaranteed_dated_witnesses(&inv.body, definitions)
            } else {
                BTreeSet::new()
            }
        })
        .collect();

    let mut out = Vec::new();
    for decl in &program.predicates {
        // `partial` declares the gaps intended. Contradicting it with a
        // `total over` is a validation error.
        let effective = decl
            .disciplines
            .iter()
            .any(|d| matches!(d, crate::ir::Discipline::EffectiveBy { partial: false, .. }));
        let declared = declared_totality.iter().flatten().any(|p| *p == decl.name);
        if effective && !declared {
            out.push(Lint::EffectiveWithoutDeclaredTotality {
                predicate: decl.name.to_string(),
            });
        }
    }

    for (index, inv) in program.invariants.iter().enumerate() {
        let implications = implications_of(&inv.body, definitions);
        if do_gate {
            gate_vs_invariant_findings(
                inv,
                &implications,
                &append_only,
                &pointers,
                definitions,
                &mut out,
            );
        }
        // Generated discipline invariants are not in the source, so they
        // get no hints of these kinds.
        if inv.origin == InvariantOrigin::Authored {
            unsupplied_antecedent_findings(inv, &implications, &declared, definitions, &mut out);
            governing_selection_findings(
                inv,
                index,
                &implications,
                &witnesses,
                &declared_totality,
                definitions,
                &mut out,
            );
        }
    }
    out
}

/// Whether `body` reaches `target`, following definition calls through the
/// definitions they in turn call.
fn calls_definition(
    body: &Prop,
    target: &DefinitionName,
    definitions: DefinitionTable<'_>,
) -> bool {
    let mut reached = BTreeSet::new();
    crate::definitions::defined_calls_in_prop(body, &mut reached);
    let mut frontier: Vec<DefinitionName> = reached.iter().cloned().collect();
    while let Some(name) = frontier.pop() {
        if &name == target {
            return true;
        }
        if let Some(def) = definitions.get(&name) {
            let mut inner = BTreeSet::new();
            crate::definitions::defined_calls_in_prop(&def.body, &mut inner);
            for call in inner {
                if reached.insert(call.clone()) {
                    frontier.push(call);
                }
            }
        }
    }
    false
}

/// An antecedent that selects the version of a predicate in force at a
/// date, where no other invariant declares or guarantees that
/// predicate's totality. Names only the unbacked predicates.
fn governing_selection_findings(
    inv: &Invariant,
    index: usize,
    implications: &[CollectedImplication<'_>],
    witnesses: &[BTreeSet<PredicateName>],
    declared_totality: &[Option<PredicateName>],
    definitions: DefinitionTable<'_>,
    out: &mut Vec<Lint>,
) {
    let mut selected = BTreeSet::new();
    for implication in implications {
        selected.extend(crate::analysis::governing_selections(
            implication.antecedent,
            definitions,
        ));
    }
    if selected.is_empty() {
        return;
    }
    // A declared backstop or a recognised backstop shape in another
    // invariant both count.
    let unbacked: Vec<String> = selected
        .iter()
        .filter(|p| {
            let declared = declared_totality
                .iter()
                .enumerate()
                .any(|(j, d)| j != index && d.as_ref() == Some(*p));
            let shaped = witnesses
                .iter()
                .enumerate()
                .any(|(j, w)| j != index && w.contains(*p));
            !declared && !shaped
        })
        .map(ToString::to_string)
        .collect();
    if unbacked.is_empty() {
        return;
    }
    out.push(Lint::GoverningSelectionWithoutTotality {
        invariant: inv.name.to_string(),
        predicates: unbacked,
    });
}

/// The revocation-rewrites-history shape: an antecedent positively
/// referencing an append-only predicate, with a consequent positively
/// requiring a current-pointer predicate.
fn gate_vs_invariant_findings(
    inv: &Invariant,
    implications: &[CollectedImplication<'_>],
    append_only: &BTreeSet<PredicateName>,
    pointers: &BTreeSet<PredicateName>,
    definitions: DefinitionTable<'_>,
    out: &mut Vec<Lint>,
) {
    for implication in implications {
        let antecedent_refs = positive_claims_of(implication.antecedent, definitions);
        let consequent_refs = positive_claims_of(implication.consequent, definitions);
        for a in antecedent_refs.iter().filter(|p| append_only.contains(*p)) {
            for q in consequent_refs.iter().filter(|p| pointers.contains(*p)) {
                out.push(Lint::GateVsInvariant {
                    invariant: inv.name.to_string(),
                    append_only: a.to_string(),
                    pointer: q.to_string(),
                });
            }
        }
    }
}

/// An implication whose antecedent the current programme cannot satisfy
/// on a fresh ledger, because it depends on predicates no transformation
/// admits. One finding per invariant, naming only the predicates that
/// genuinely force the result, deduped across its implications.
fn unsupplied_antecedent_findings(
    inv: &Invariant,
    implications: &[CollectedImplication<'_>],
    declared: &BTreeSet<PredicateName>,
    definitions: DefinitionTable<'_>,
    out: &mut Vec<Lint>,
) {
    let mut missing = BTreeSet::new();
    for implication in implications {
        if let Some(blockers) =
            crate::analysis::undeclared_blockers(implication.antecedent, declared, definitions)
        {
            missing.extend(blockers);
        }
    }
    if missing.is_empty() {
        return;
    }
    out.push(Lint::UnsuppliedAntecedent {
        invariant: inv.name.to_string(),
        missing: missing.iter().map(ToString::to_string).collect(),
    });
}

/// One `Defined` call on the way to a collected implication: the
/// definition's name and the call-site arguments. Coverage replays the
/// chain so literal or pre-bound arguments constrain the antecedent. The
/// lint ignores it, since substitution never changes predicate names.
pub(crate) type DefinedCall<'a> = (&'a crate::ir::DefinitionName, &'a [Term]);

/// One collected implication: antecedent, consequent, and the
/// `Defined` calls (outermost first) it was found under, empty when
/// written directly in the invariant body.
pub(crate) struct CollectedImplication<'a> {
    pub(crate) antecedent: &'a Prop,
    pub(crate) consequent: &'a Prop,
    pub(crate) calls: Vec<DefinedCall<'a>>,
}

/// Every implication `prop` asserts, at top level or behind a defined
/// call, each with the call chain it was found under.
pub(crate) fn implications_of<'a>(
    prop: &'a Prop,
    definitions: DefinitionTable<'a>,
) -> Vec<CollectedImplication<'a>> {
    let mut out = Vec::new();
    collect_implications(
        prop,
        true,
        definitions,
        &mut BTreeSet::new(),
        &mut Vec::new(),
        &mut out,
    );
    out
}

/// The predicates `prop` asserts positively, descending defined calls.
pub(crate) fn positive_claims_of(
    prop: &Prop,
    definitions: DefinitionTable<'_>,
) -> BTreeSet<PredicateName> {
    let mut out = BTreeSet::new();
    positive_claims(prop, true, definitions, &mut BTreeSet::new(), &mut out);
    out
}

/// Every `Implies` the invariant asserts, so only at positive polarity:
/// `not (A implies B)` means `A and not B`, and an implication inside
/// another's antecedent enforces nothing. `Not` flips polarity, as does
/// an `Implies` on its left side; everything else keeps it. `Defined`
/// calls are followed (cycles guarded), and each implication carries its
/// call chain so coverage can evaluate it in context.
fn collect_implications<'a>(
    prop: &'a Prop,
    positive: bool,
    definitions: DefinitionTable<'a>,
    seen: &mut BTreeSet<crate::ir::DefinitionName>,
    calls: &mut Vec<DefinedCall<'a>>,
    out: &mut Vec<CollectedImplication<'a>>,
) {
    match prop {
        Prop::Implies { left, right } => {
            if positive {
                out.push(CollectedImplication {
                    antecedent: left,
                    consequent: right,
                    calls: calls.clone(),
                });
            }
            collect_implications(left, !positive, definitions, seen, calls, out);
            collect_implications(right, positive, definitions, seen, calls, out);
        }
        // The same definition at another polarity must be expanded again,
        // which `DefinitionTable::enter`'s stack guard allows.
        Prop::Defined { name, args } => definitions.enter(name, seen, |def_, seen| {
            calls.push((name, args));
            collect_implications(&def_.body, positive, definitions, seen, calls, out);
            calls.pop();
        }),
        Prop::Claim { .. } | Prop::In(_, _) => {}
        Prop::And(props) | Prop::Or(props) => {
            for p in props {
                collect_implications(p, positive, definitions, seen, calls, out);
            }
        }
        Prop::Xor(left, right) => {
            collect_implications(left, positive, definitions, seen, calls, out);
            collect_implications(right, positive, definitions, seen, calls, out);
        }
        Prop::Not(p) => collect_implications(p, !positive, definitions, seen, calls, out),
        Prop::Exists { body: p, .. } | Prop::Pre(p) => {
            collect_implications(p, positive, definitions, seen, calls, out);
        }
        Prop::Forall { source, body, .. } => {
            collect_implications(source, positive, definitions, seen, calls, out);
            collect_implications(body, positive, definitions, seen, calls, out);
        }
        Prop::Eq(_, _) | Prop::Neq(_, _) | Prop::Compare { .. } => {}
    }
}

/// Predicates referenced at positive polarity: required to hold, not
/// required absent. `Not` flips polarity, as does a nested `Implies` on
/// its left side. Negative references are dropped: `implies not
/// Pointer(...)` only gets stronger when the pointer is retracted.
/// `Defined` calls are followed.
fn positive_claims(
    prop: &Prop,
    positive: bool,
    definitions: DefinitionTable<'_>,
    seen: &mut BTreeSet<crate::ir::DefinitionName>,
    out: &mut BTreeSet<PredicateName>,
) {
    match prop {
        Prop::Claim { predicate, .. } => {
            if positive {
                out.insert(predicate.clone());
            }
        }
        Prop::Defined { name, .. } => definitions.enter(name, seen, |def_, seen| {
            positive_claims(&def_.body, positive, definitions, seen, out);
        }),
        Prop::Not(inner) => positive_claims(inner, !positive, definitions, seen, out),
        Prop::Implies { left, right } => {
            positive_claims(left, !positive, definitions, seen, out);
            positive_claims(right, positive, definitions, seen, out);
        }
        Prop::And(props) | Prop::Or(props) => {
            for p in props {
                positive_claims(p, positive, definitions, seen, out);
            }
        }
        Prop::Xor(left, right) => {
            // Xor uses each side in both polarities; count both at the
            // current one.
            positive_claims(left, positive, definitions, seen, out);
            positive_claims(right, positive, definitions, seen, out);
        }
        Prop::Exists { body, .. } | Prop::Pre(body) => {
            positive_claims(body, positive, definitions, seen, out);
        }
        Prop::Forall { source, body, .. } => {
            positive_claims(source, positive, definitions, seen, out);
            positive_claims(body, positive, definitions, seen, out);
        }
        Prop::Eq(l, r) | Prop::Neq(l, r) => {
            positive_value_claims(l, positive, definitions, seen, out);
            positive_value_claims(r, positive, definitions, seen, out);
        }
        Prop::Compare { left, right, .. } => {
            positive_value_claims(left, positive, definitions, seen, out);
            positive_value_claims(right, positive, definitions, seen, out);
        }
        Prop::In(_, _) => {}
    }
}

/// Value-sort companion. A `value Pred(...)` lookup without a default
/// requires the claim, so it counts at the enclosing polarity; with a
/// default only the default expression counts. `sum` bodies tolerate
/// zero matches, so they contribute nothing.
fn positive_value_claims(
    expr: &ValueExpr,
    positive: bool,
    definitions: DefinitionTable<'_>,
    seen: &mut BTreeSet<crate::ir::DefinitionName>,
    out: &mut BTreeSet<PredicateName>,
) {
    match expr {
        ValueExpr::Term(_) => {}
        ValueExpr::ValueOf {
            predicate, default, ..
        } => {
            if positive && default.is_none() {
                out.insert(predicate.clone());
            }
            if let Some(d) = default {
                positive_value_claims(d, positive, definitions, seen, out);
            }
        }
        ValueExpr::Arith { left, right, .. } => {
            positive_value_claims(left, positive, definitions, seen, out);
            positive_value_claims(right, positive, definitions, seen, out);
        }
        // A builtin adds nothing itself, but its arguments may.
        ValueExpr::Call { args, .. } => {
            for a in args {
                positive_value_claims(a, positive, definitions, seen, out);
            }
        }
        // A sum tolerates zero matches, so it requires nothing.
        ValueExpr::Sum { .. } => {}
        // An extremum is the opposite: zero matches is an error, so its
        // body is required.
        ValueExpr::Extremum { body, .. } => positive_claims(body, positive, definitions, seen, out),
        // The condition picks the expected value, so every predicate in
        // it counts, whatever the polarity: retracting a pointer read only
        // here still changes what the rule expects of old records. Either
        // branch may be taken, so both count, as with `or`.
        ValueExpr::Cond {
            when,
            then,
            otherwise,
        } => {
            crate::analysis::prop_refs(when, definitions, &mut BTreeSet::new(), out);
            positive_value_claims(then, positive, definitions, seen, out);
            positive_value_claims(otherwise, positive, definitions, seen, out);
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use crate::definitions::DefinitionTable;
    use crate::ir::Term;
    use std::collections::BTreeSet;

    fn claim(pred: &str) -> Prop {
        Prop::Claim {
            predicate: pred.into(),
            args: vec![Term::Wildcard],
        }
    }

    fn implies(l: Prop, r: Prop) -> Prop {
        Prop::Implies {
            left: Box::new(l),
            right: Box::new(r),
        }
    }

    fn implications_in(prop: &Prop) -> usize {
        let out = implications_of(prop, DefinitionTable::new(&[]));
        out.len()
    }

    /// Polarity is part of an implication's meaning: one nested in an
    /// antecedent (or under `not`) is not a rule of its own. Only the
    /// outer, positively-placed implication is collected.
    #[test]
    fn implications_are_collected_at_positive_polarity_only() {
        let nested_in_antecedent = implies(implies(claim("A"), claim("B")), claim("C"));
        assert_eq!(implications_in(&nested_in_antecedent), 1);

        let negated = Prop::Not(Box::new(implies(claim("A"), claim("B"))));
        assert_eq!(implications_in(&negated), 0);

        // In a consequent, polarity is preserved: both count.
        let nested_in_consequent = implies(claim("A"), implies(claim("B"), claim("C")));
        assert_eq!(implications_in(&nested_in_consequent), 2);
    }

    fn positives_in(prop: &Prop) -> BTreeSet<crate::PredicateName> {
        positive_claims_of(prop, DefinitionTable::new(&[]))
    }

    /// `not` flips claim polarity, and flips it back when doubled.
    #[test]
    fn negation_flips_claim_polarity_both_ways() {
        assert!(positives_in(&Prop::Not(Box::new(claim("A")))).is_empty());
        let doubled = Prop::Not(Box::new(Prop::Not(Box::new(claim("A")))));
        assert!(positives_in(&doubled).contains(&"A".into()));
    }

    /// A defaultless `value` lookup demands its claim exist, so the
    /// predicate counts as positively required - but only at positive
    /// polarity, and not once a default absorbs the zero-match case.
    #[test]
    fn value_lookups_count_only_defaultless_and_positive() {
        let lookup = |default: Option<Box<ValueExpr>>| {
            Prop::Eq(
                Box::new(ValueExpr::ValueOf {
                    predicate: "Looked".into(),
                    args: vec![Term::Wildcard],
                    extract: 0,
                    default,
                }),
                Box::new(ValueExpr::Term(Term::Wildcard)),
            )
        };
        assert!(positives_in(&lookup(None)).contains(&"Looked".into()));
        let defaulted = lookup(Some(Box::new(ValueExpr::Term(Term::Wildcard))));
        assert!(positives_in(&defaulted).is_empty());
        assert!(positives_in(&Prop::Not(Box::new(lookup(None)))).is_empty());
    }

    /// The hint text is what `check` prints to stderr: it names the
    /// invariant and, for the unsupplied case, every blocker.
    #[test]
    fn lint_display_names_the_rule_and_its_cause() {
        let gate = Lint::GateVsInvariant {
            invariant: "books_balance".to_string(),
            append_only: "Entry".to_string(),
            pointer: "CurrentTotal".to_string(),
        };
        let rendered = format!("{gate}");
        assert!(rendered.contains("books_balance") && rendered.contains("Entry"));

        let unsupplied = Lint::UnsuppliedAntecedent {
            invariant: "haunting_is_real".to_string(),
            missing: vec!["Ghost".to_string()],
        };
        let rendered = format!("{unsupplied}");
        assert!(rendered.contains("haunting_is_real") && rendered.contains("Ghost"));

        let governing = Lint::GoverningSelectionWithoutTotality {
            invariant: "priced_by_governing_tariff".to_string(),
            predicates: vec!["Tariff".to_string()],
        };
        let rendered = format!("{governing}");
        assert!(
            rendered.contains("priced_by_governing_tariff")
                && rendered.contains("`Tariff`")
                && rendered.contains("totality backstop")
                && rendered.contains("may pass"),
            "the hint names the rule, the predicate, and the mitigation, \
             without overclaiming: {rendered}"
        );
    }
}
