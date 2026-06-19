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

use std::collections::BTreeSet;

use crate::definitions::DefinitionIndex;
use crate::disciplines::append_only_predicates;
use crate::ir::{
    Discipline, Invariant, InvariantOrigin, PredicateName, Program, Prop, Term, ValueExpr,
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
}

impl std::fmt::Display for Lint {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
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
        }
    }
}

/// Collect every lint finding for a programme. Pure and deterministic:
/// one pass over invariants in declaration order, each invariant's
/// findings emitted before the next invariant's. Within one invariant,
/// gate findings precede the unsupplied-antecedent finding; ordering by
/// sub-expression position awaits the node-identified IR.
pub fn lints(program: &Program) -> Vec<Lint> {
    let definitions = DefinitionIndex::new(&program.definitions);
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

    let mut out = Vec::new();
    for inv in &program.invariants {
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
        // see in source, so it gets no unsupplied-antecedent hint.
        if inv.origin == InvariantOrigin::Authored {
            unsupplied_antecedent_findings(inv, &implications, &declared, definitions, &mut out);
        }
    }
    out
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
        Prop::Defined { name, args } => {
            // `seen` is a recursion-STACK guard, not a visited set:
            // polarity is part of the meaning here, so the same
            // definition called again at a different polarity must be
            // expanded again. Insert before descending, remove after -
            // cycles still terminate (re-entry while on the stack).
            if seen.insert(name.clone()) {
                if let Some(def) = definitions.get(name) {
                    calls.push((name, args));
                    collect_implications(&def.body, positive, definitions, seen, calls, out);
                    calls.pop();
                }
                seen.remove(name);
            }
        }
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
        Prop::Defined { name, .. } => {
            // Recursion-stack guard, not a visited set - same polarity
            // reasoning as collect_implications above.
            if seen.insert(name.clone()) {
                if let Some(def) = definitions.get(name) {
                    positive_claims(&def.body, positive, definitions, seen, out);
                }
                seen.remove(name);
            }
        }
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
            positive_value_claims(l, positive, out);
            positive_value_claims(r, positive, out);
        }
        Prop::Compare { left, right, .. } => {
            positive_value_claims(left, positive, out);
            positive_value_claims(right, positive, out);
        }
        Prop::In(_, _) => {}
    }
}

/// Value-sort companion. A defaultless `value Pred(...)` lookup
/// *requires* a claim to be readable, so it counts at the enclosing
/// polarity; one with a `default` tolerates absence and contributes
/// only what its default expression carries. `sum` bodies tolerate
/// zero matches, so they contribute nothing.
fn positive_value_claims(expr: &ValueExpr, positive: bool, out: &mut BTreeSet<PredicateName>) {
    match expr {
        ValueExpr::Term(_) => {}
        ValueExpr::ValueOf {
            predicate, default, ..
        } => {
            if positive && default.is_none() {
                out.insert(predicate.clone());
            }
            if let Some(d) = default {
                positive_value_claims(d, positive, out);
            }
        }
        ValueExpr::Arith { left, right, .. } => {
            positive_value_claims(left, positive, out);
            positive_value_claims(right, positive, out);
        }
        // A sum tolerates zero matches; its body does not REQUIRE the
        // claims, so it contributes nothing.
        ValueExpr::Sum { .. } => {}
    }
}
