//! Lint-grade hints: findings that deserve an author's attention but
//! are not errors, because the flagged shape has a deliberate reading.
//! Surfaced by `morpholog check` as hints; `--strict` promotes them to
//! errors. Distinct from [`crate::ValidationError`] on purpose - an
//! error means the programme cannot mean what it says, a lint means it
//! says something that is usually, but not always, a mistake.
//!
//! The first lint is the gate-vs-invariant doctrine made mechanical:
//! with append-only and current-pointer classes declared as
//! disciplines, the revocation-rewrites-history shape - an invariant
//! conditioning permanent records on a retractable pointer's presence
//! - is detectable at check time.
//!
//! The third occupant is the effective-time vacuity smell: an
//! antecedent selecting "the governing version in force at a
//! coordinate" passes vacuously when no version exists there, unless
//! another invariant backstops totality. A shape smell, never a
//! vacuity proof - that is the verification arc's static-vacuity
//! tier.

use std::collections::BTreeSet;

use crate::compiled::CompiledProgram;
use crate::definitions::DefinitionIndex;
use crate::disciplines::append_only_predicates;
use crate::ir::{
    DefinitionName, Discipline, Invariant, InvariantOrigin, PredicateName, Prop, Term, ValueExpr,
};

/// One lint finding. See the module doc for the error-vs-lint line.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Lint {
    /// An invariant whose antecedent positively references an
    /// append-only predicate and whose consequent positively requires
    /// a current-pointer predicate. Retracting that pointer would make
    /// already-admitted records violate the rule - blocking the
    /// retraction or forcing history to be rewritten. The deliberate
    /// reading exists (continuous-compliance models re-check standing
    /// over admitted records on purpose), which is why this is a hint:
    /// keep it knowingly, or move the check into the admitting
    /// transformation's gate.
    ///
    /// Forward direction only. The reverse - a pointer's antecedent
    /// requiring an append-only consequent ("the pointer names a
    /// figure that exists") - is correct doctrine: retracting the
    /// pointer makes it vacuous, never violated.
    GateVsInvariant {
        invariant: String,
        append_only: String,
        pointer: String,
    },

    /// An authored invariant whose antecedent the current programme
    /// cannot satisfy on a fresh ledger, because it depends on
    /// predicates no transformation admits. `missing` lists those
    /// predicates (collectively the cause - for an `or`, every branch is
    /// unsupplied; for an `and`, each is a mandatory conjunct). A hint,
    /// not a proof of a dead rule: persisted or historically admitted
    /// claims may still match.
    UnsuppliedAntecedent {
        invariant: String,
        missing: Vec<String>,
    },

    /// An authored invariant whose antecedent appears to select "the
    /// governing version of `P` in force at a coordinate" - a dated
    /// `P` claim bounded on-or-before a coordinate, with a negated
    /// `exists` excluding a strictly later `P` - while no OTHER
    /// authored invariant carries the recognised totality-backstop
    /// shape for `P` (an implication guaranteeing an `exists` witness
    /// of `P` with a temporal bound). When no version is in force at
    /// a coordinate, such a selection passes vacuously - the rule
    /// silently does not apply at the edges. `predicates` names the
    /// UNBACKED selected predicates only. A shape smell, not a
    /// vacuity proof: coordinate agreement between selection and
    /// backstop is not verified.
    GoverningSelectionWithoutTotality {
        invariant: String,
        predicates: Vec<String>,
    },

    /// A predicate declared `effective by` with no invariant declaring
    /// `total over` it. The generated selector returns nothing where no
    /// version is in force, so every rule reading it goes quietly vacuous
    /// at the edges - and nothing in the source says whether that was
    /// intended.
    ///
    /// A rule that reads `P`'s in-force selector cannot declare `P`'s
    /// totality: it only applies where a version is already in force, so
    /// it cannot be the reason one exists. Such a declaration is ignored
    /// and the finding stands.
    ///
    /// A hint, not an error: a partial effective-dated predicate can be a
    /// correct model, where a rule genuinely should not apply before the
    /// first version exists. `--strict` promotes it for authors who want
    /// the pairing guaranteed rather than remembered.
    EffectiveWithoutDeclaredTotality { predicate: String },
}

impl std::fmt::Display for Lint {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Lint::EffectiveWithoutDeclaredTotality { predicate } => write!(
                f,
                "`{predicate}` is effective-dated but no invariant declares `total over \
                 {predicate}`: where no version is in force the generated selector matches \
                 nothing, so every rule reading it passes vacuously. Mark the invariant that \
                 guarantees a version exists with `total over {predicate}`; if the gap is \
                 intended, leaving this hint unaddressed is how you say so - it only refuses \
                 under `--strict`"
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
        }
    }
}

/// Collect every lint finding for a programme. Pure and deterministic:
/// one pass over invariants in declaration order, each invariant's
/// findings emitted before the next invariant's. Within one invariant,
/// gate findings precede the unsupplied-antecedent finding; ordering by
/// sub-expression position awaits the node-identified IR.
pub fn lints(compiled: &CompiledProgram) -> Vec<Lint> {
    let program = compiled.program();
    let definitions = compiled.definition_index();
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

    // What an author has DECLARED they backstop, whatever shape the rule
    // takes. Positional for the same reason `witnesses` is - so a consumer
    // can skip the invariant under test. A declaration is also dropped
    // outright when the declaring rule consults the in-force selector for
    // the predicate it vouches for: a rule that only applies where a
    // version is in force cannot be the reason one exists, so it is no
    // one's companion, not merely not its own.
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

    // The shape-recognised side, for programmes that declare nothing.
    // Positional, so the consumer can skip the invariant under test: a
    // companion is by definition a DIFFERENT rule.
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
    // An `effective by` predicate with nothing declaring its totality: the
    // selector goes quiet where no version is in force, and the omission
    // is invisible. A hint rather than an error because a partial
    // effective-dated predicate can be correct - a rule that should not
    // apply before the first version exists is a legitimate model - but
    // `--strict` turns it into the refusal an author who wants the pairing
    // guaranteed is asking for.
    for decl in &program.predicates {
        let effective = decl
            .disciplines
            .iter()
            .any(|d| matches!(d, crate::ir::Discipline::EffectiveBy { .. }));
        let declared = declared_totality.iter().flatten().any(|p| *p == decl.name);
        if effective && !declared {
            out.push(Lint::EffectiveWithoutDeclaredTotality {
                predicate: decl.name.to_string(),
            });
        }
    }

    for (index, inv) in program.invariants.iter().enumerate() {
        let mut implications = Vec::new();
        collect_implications(
            &inv.body,
            true,
            definitions,
            &mut BTreeSet::new(),
            &mut Vec::new(),
            &mut implications,
        );
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
        // A generated discipline invariant is machinery the author cannot
        // see in source, so it gets no unsupplied-antecedent hint - and no
        // governing-selection hint either.
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

/// The effective-time vacuity smell: an antecedent that selects the
/// governing version of a predicate at a coordinate, in a programme
/// where no OTHER invariant carries the recognised totality-backstop
/// shape for it. Names only the unbacked predicates.
/// Whether `body` reaches `target`, following definition calls through the
/// definitions they in turn call.
fn calls_definition(
    body: &Prop,
    target: &DefinitionName,
    definitions: DefinitionIndex<'_>,
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

fn governing_selection_findings(
    inv: &Invariant,
    index: usize,
    implications: &[CollectedImplication<'_>],
    witnesses: &[BTreeSet<PredicateName>],
    declared_totality: &[Option<PredicateName>],
    definitions: DefinitionIndex<'_>,
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
    // A DECLARED backstop settles it. Shape-matching stays as the
    // fallback for programmes that never declare one, but where the
    // author has said which rule backstops the predicate, the pairing is
    // checked rather than guessed - an unusual-but-intended backstop
    // counts, and a shape that matched by accident does not.
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
    definitions: DefinitionIndex<'_>,
    out: &mut Vec<Lint>,
) {
    for implication in implications {
        let mut antecedent_refs = BTreeSet::new();
        positive_claims(
            implication.antecedent,
            true,
            definitions,
            &mut BTreeSet::new(),
            &mut antecedent_refs,
        );
        let mut consequent_refs = BTreeSet::new();
        positive_claims(
            implication.consequent,
            true,
            definitions,
            &mut BTreeSet::new(),
            &mut consequent_refs,
        );
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
    definitions: DefinitionIndex<'_>,
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

/// One `Defined` call traversed on the way to a collected
/// implication: the definition's name plus the argument terms at the
/// call site. Coverage replays the chain through the canonical call
/// frames so a call-site-constrained antecedent (a literal argument,
/// a pre-bound variable) is evaluated under that constraint instead
/// of with the definition's parameters free; the lint ignores it
/// (substitution never changes predicate names).
pub(crate) type DefinedCall<'a> = (&'a crate::ir::DefinitionName, &'a [Term]);

/// One collected implication: antecedent, consequent, and the stack
/// of `Defined` calls (outermost first) it was found under - empty
/// for an implication spelled directly in the invariant body.
pub(crate) struct CollectedImplication<'a> {
    pub(crate) antecedent: &'a Prop,
    pub(crate) consequent: &'a Prop,
    pub(crate) calls: Vec<DefinedCall<'a>>,
}

/// Every `Implies` node the invariant actually ASSERTS - collected
/// only at positive polarity, because a negated implication
/// (`not (A implies B)` is `A and not B`) and an implication sitting
/// in another implication's antecedent enforce nothing of the shape
/// the lint reads. Enclosing `And`/`Or`/quantifiers preserve polarity;
/// `Not` flips it; an `Implies` flips its own left side. `Defined`
/// calls descend into their bodies (recursion-stack guard against
/// cycles, the walker red line): an implication hidden behind a named
/// condition is still an implication the invariant asserts. The
/// collected antecedent/consequent references may therefore point
/// into a definition's body; each implication carries the call chain
/// it was found under so coverage can evaluate it in call context.
pub(crate) fn collect_implications<'a>(
    prop: &'a Prop,
    positive: bool,
    definitions: DefinitionIndex<'a>,
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
        // Polarity is part of the meaning here, so the same definition
        // called again at a different polarity must be expanded again -
        // exactly the stack-guard semantics `DefinitionIndex::enter`
        // provides.
        Prop::Defined { name, args } => definitions.enter(name, seen, |body, seen| {
            calls.push((name, args));
            collect_implications(body, positive, definitions, seen, calls, out);
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

/// Predicates referenced at POSITIVE polarity: required to hold, not
/// required absent. `Not` flips polarity; a nested `Implies` flips its
/// left side (an implication is satisfied by its antecedent failing);
/// everything else preserves it. Negative-polarity references are
/// dropped - `implies not Pointer(...)` gets STRONGER when the pointer
/// is retracted, which is the opposite of the bug. `Defined` calls
/// descend into their bodies (with a seen-set, mirroring the analysis
/// walkers), since a named condition hides its claims behind the call.
pub(crate) fn positive_claims(
    prop: &Prop,
    positive: bool,
    definitions: DefinitionIndex<'_>,
    seen: &mut BTreeSet<crate::ir::DefinitionName>,
    out: &mut BTreeSet<PredicateName>,
) {
    match prop {
        Prop::Claim { predicate, .. } => {
            if positive {
                out.insert(predicate.clone());
            }
        }
        Prop::Defined { name, .. } => definitions.enter(name, seen, |body, seen| {
            positive_claims(body, positive, definitions, seen, out);
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
            // Exactly-one holds each side in both polarities; treat
            // both as referenced at the current polarity (the
            // conservative reading for a hint).
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

/// Value-sort companion. A defaultless `value Pred(...)` lookup
/// *requires* a claim to be readable, so it counts at the enclosing
/// polarity; one with a `default` tolerates absence and contributes
/// only what its default expression carries. `sum` bodies tolerate
/// zero matches, so they contribute nothing.
fn positive_value_claims(
    expr: &ValueExpr,
    positive: bool,
    definitions: DefinitionIndex<'_>,
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
        // abs reads its operand, so it requires whatever the operand does.
        ValueExpr::Abs(operand) => positive_value_claims(operand, positive, definitions, seen, out),
        // round reads both operands, so it requires whatever they do.
        ValueExpr::Round { value, quantum } => {
            positive_value_claims(value, positive, definitions, seen, out);
            positive_value_claims(quantum, positive, definitions, seen, out);
        }
        // A sum tolerates zero matches; its body does not REQUIRE the
        // claims, so it contributes nothing.
        ValueExpr::Sum { .. } => {}
        // An extremum is the opposite, which is why it cannot share the
        // arm above: zero matches is an error, so its body IS required.
        // Grouping the two would hide a rule over a permanent record that
        // reads a retractable pointer - exactly the shape this lint
        // exists to surface.
        ValueExpr::Extremum { body, .. } => positive_claims(body, positive, definitions, seen, out),
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use crate::definitions::DefinitionIndex;
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
        let mut out = Vec::new();
        collect_implications(
            prop,
            true,
            DefinitionIndex::new(&[]),
            &mut BTreeSet::new(),
            &mut Vec::new(),
            &mut out,
        );
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
        let mut out = BTreeSet::new();
        positive_claims(
            prop,
            true,
            DefinitionIndex::new(&[]),
            &mut BTreeSet::new(),
            &mut out,
        );
        out
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
