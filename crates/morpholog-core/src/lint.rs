//! Lint hints: shapes that are usually, but not always, a mistake.
//! `morpholog check` prints them as hints; `--strict` makes them errors.
//! A [`crate::ValidationError`] is different: it means the programme
//! cannot mean what it says.

use std::collections::BTreeSet;

use crate::analysis::{has_admission_gate, predicates_written_by};
use crate::definitions::DefinitionTable;
use crate::disciplines::append_only_predicates;
use crate::fold;
use crate::impact::ImpactPlan;
use crate::ir::{
    DefinitionName, Discipline, Invariant, InvariantOrigin, PredicateName, Program, Prop, Stmt,
    Term, Value, ValueExpr, Var,
};
use crate::prepared::PreparedProgram;

/// Python's hard keywords (3.12 floor). A declared name among them cannot
/// be a dataclass field of the generated client, and renaming it there
/// would break the link to the wire name.
pub const PYTHON_KEYWORDS: &[&str] = &[
    "False", "None", "True", "and", "as", "assert", "async", "await", "break", "class", "continue",
    "def", "del", "elif", "else", "except", "finally", "for", "from", "global", "if", "import",
    "in", "is", "lambda", "nonlocal", "not", "or", "pass", "raise", "return", "try", "while",
    "with", "yield",
];

/// Member names the generated client's classes define; a declared name
/// sharing one would shadow it. The uppercase entries are class metadata.
pub const PYTHON_CLIENT_MEMBERS: &[&str] = &[
    "to_args_named",
    "from_named",
    "from_args",
    "TRANSFORMATION",
    "PREDICATE",
    "INTENT",
    "_ARG_ORDER",
];

/// Whether the generated Python client refuses `name` as a field or
/// parameter.
pub fn client_refuses_name(name: &str) -> bool {
    PYTHON_KEYWORDS.contains(&name) || PYTHON_CLIENT_MEMBERS.contains(&name)
}

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

    /// An authored invariant that admission checks case by case over one
    /// claim pattern, where the pattern constrains no uniqueness key the
    /// disciplines enforce on that predicate. Its case is then every
    /// claim sharing the constrained fields, not the one claim a
    /// proposal identifies, so a touching proposal reads them all.
    /// `case` names the constrained fields; `key` the fields still
    /// missing from the enforced key closest to constrained (not the
    /// whole key), empty when the predicate declares none; `unnamed`
    /// every unconstrained field. A literal counts as constraining only
    /// where admission's impact plan compares it.
    ///
    /// A hint, because the rule means the same thing either way; the
    /// cost shows only when many claims share those values. Silent when
    /// admission checks the whole invariant anyway, when the body reads a
    /// second claim pattern, or when it calls a definition (the fix
    /// might belong in the definition, not here).
    CaseWiderThanClaim {
        invariant: String,
        predicate: String,
        case: Vec<String>,
        key: Vec<String>,
        unnamed: Vec<String>,
    },

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

    /// An authored implication whose antecedent joins an append-only
    /// predicate with a retractable one and whose consequent reads what
    /// the retractable pattern binds. Changing the retractable claim
    /// (retract, admit anew) joins the new figure to every append-only
    /// record ever admitted, so every one is checked again and a
    /// tightening is refused by history. A hint, because re-checking
    /// history can be the intended standing; otherwise a gate on the
    /// act that admits the append-only record checks the figure in
    /// force at the time.
    ///
    /// Claims written directly in the antecedent only; a retractable
    /// whose figure the consequent never reads is not reported, since
    /// retracting it makes the rule vacuous for those records.
    ChangeRechecksHistory {
        invariant: String,
        append_only: String,
        retractable: String,
    },

    /// A declared name the generated Python client cannot carry: a
    /// Python keyword, or a member name the generated classes define.
    /// `generate python-client` refuses the whole programme on it, after
    /// the programme has been hashed and provisioned; this says so at
    /// `check`. `owner` says whose name it is.
    ClientName { owner: String, name: String },

    /// A variable a `require` matches, named again by a later statement
    /// of the same transformation while still unbound. A `require` keeps
    /// none of its matches, so the later statement matches it afresh, and
    /// the two need not agree. `first` and `again` are 0-based top-level
    /// statement indexes. A hint, because two independent matches are
    /// sometimes meant; renaming one says so in the source.
    RequireRematch {
        transformation: String,
        variable: String,
        first: usize,
        again: usize,
    },

    /// A `periods` grid whose window gate still allows a very large grid:
    /// every rule ranging over it runs once per period on every
    /// proposal. `periods` is the most the grid can hold under the gate's
    /// bound and the call's step. A hint, because the cost may be meant;
    /// a shorter bound or a coarser step says so in the source.
    LargeGrid {
        transformation: String,
        bound: String,
        step: String,
        periods: u128,
    },

    /// A `bind` that matches a claim only so a later `retract` can name
    /// every field: none of the fields it binds is read anywhere else,
    /// and the fields it keeps cover an enforced uniqueness key, each
    /// grounded before the bind or a literal, so the pattern pins the
    /// one claim the bind did. A retract takes a pattern, so the fields
    /// need not be bound at all. `bind` and `retract` are 0-based
    /// top-level statement indexes; `pattern` is the retract in the
    /// named spelling. A hint, because a `bind` also refuses the act
    /// when no claim matches, where a pattern retract is a no-op; a
    /// `require` keeps that refusal.
    BindOnlyToRetract {
        transformation: String,
        predicate: String,
        bind: usize,
        retract: usize,
        pattern: String,
    },
}

/// Above this many periods per proposal, a grid earns [`Lint::LargeGrid`].
pub const LARGE_GRID_PERIODS: u128 = 10_000;

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
            Lint::LargeGrid {
                transformation,
                bound,
                step,
                periods,
            } => write!(
                f,
                "`{transformation}` builds a grid of up to {periods} periods on every proposal \
                 (a window up to {bound} in steps of {step}), and each rule over the grid runs \
                 once per period. If that cost is not meant, shorten the window bound or coarsen \
                 the step"
            ),
            Lint::BindOnlyToRetract {
                transformation,
                predicate,
                bind,
                retract,
                pattern,
            } => write!(
                f,
                "`{transformation}` binds `{predicate}` in statement {} only to retract it in \
                 statement {}; `retract {pattern}` retracts the matching claim without binding \
                 its fields, and is a no-op when none matches, so keep a `require` before it if \
                 the act must refuse then",
                bind + 1,
                retract + 1
            ),
            Lint::RequireRematch {
                transformation,
                variable,
                first,
                again,
            } => {
                if first == again {
                    write!(
                        f,
                        "`{variable}` is matched by a `require` inside statement {} of \
                         `{transformation}` and named again later inside it",
                        first + 1
                    )?;
                } else {
                    write!(
                        f,
                        "`{variable}` is matched by the `require` at statement {} of \
                         `{transformation}` and named again at statement {}",
                        first + 1,
                        again + 1
                    )?;
                }
                write!(
                    f,
                    "; a `require` keeps none of its matches, so the second is a fresh match \
                     that need not agree with the first. If they must agree, match it with \
                     `bind` instead; if they are independent, renaming one says so"
                )
            }
            Lint::CaseWiderThanClaim {
                invariant,
                predicate,
                case,
                key,
                unnamed,
            } => {
                let sharing = backticked(case);
                if key.is_empty() {
                    write!(
                        f,
                        "invariant `{invariant}` checks every `{predicate}` sharing {sharing}: its \
                         pattern leaves {} unnamed, and `{predicate}` has no declared uniqueness \
                         key proving the remaining bindings identify one claim. Name a field \
                         that identifies the claim, if one exists",
                        backticked(unnamed)
                    )
                } else {
                    let (fields, vars) = if key.len() == 1 {
                        ("field", "a fresh variable")
                    } else {
                        ("fields", "fresh, distinct variables")
                    };
                    let key = backticked(key);
                    write!(
                        f,
                        "invariant `{invariant}` checks every `{predicate}` sharing {sharing}, \
                         because its pattern leaves declared-key {fields} {key} unconstrained; \
                         constraining {key} completes a uniqueness key and narrows the case \
                         to the one `{predicate}` a proposal identifies. Binding {key} to \
                         {vars} keeps the rule's meaning and moves the programme's hash; a \
                         variable the rule already uses, or a literal, narrows the rule"
                    )
                }
            }
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
            Lint::ChangeRechecksHistory {
                invariant,
                append_only,
                retractable,
            } => write!(
                f,
                "invariant `{invariant}` joins retractable `{retractable}` with append-only \
                 `{append_only}` and compares what `{retractable}` binds; whenever \
                 `{retractable}` changes, every `{append_only}` ever admitted is checked \
                 against the new figure, so tightening it is refused by old records. If \
                 re-checking history is intended, keep it; otherwise a gate on the act that \
                 admits `{append_only}` checks the `{retractable}` in force at the time"
            ),
            Lint::ClientName { owner, name } => write!(
                f,
                "`{name}` ({owner}) is a Python keyword or a name the generated client \
                 reserves, so `generate python-client` refuses this programme; rename it"
            ),
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
pub fn lints(prepared: &PreparedProgram) -> Vec<Lint> {
    let program = prepared.program();
    let definitions = prepared.definition_table();
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

    client_name_findings(program, &mut out);
    for t in &program.transformations {
        require_rematch_findings(t, &mut out);
        bind_only_to_retract_findings(program, t, &mut out);
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
            change_rechecks_history_findings(inv, &implications, &append_only, &mut out);
            case_width_findings(program, inv, &prepared.admission().plans()[index], &mut out);
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
    for t in &program.transformations {
        large_grid_findings(t, &mut out);
    }
    out
}

/// Each `periods` grid in `t` whose gate still allows more than
/// [`LARGE_GRID_PERIODS`] periods, in statement order. The gate is
/// matched by name here; validation has already held every grid to one.
fn large_grid_findings(t: &crate::ir::Transformation, out: &mut Vec<Lint>) {
    use crate::check::grid;
    let mut bounds: Vec<(&Var, &Var, jiff::SignedDuration)> = Vec::new();
    for stmt in &t.body {
        if let Stmt::Require { prop, .. } = stmt {
            for conjunct in grid::conjuncts(prop) {
                if let Some(b) = grid::window_bound(conjunct) {
                    bounds.push((b.until, b.from, b.bound));
                }
            }
        }
        fold::walk_stmt(stmt, &mut |node| {
            let fold::Node::Value(expr) = node else {
                return;
            };
            let Some((from, until, step)) = grid::grid_call(expr) else {
                return;
            };
            let (
                ValueExpr::Term(Term::Var(from)),
                ValueExpr::Term(Term::Var(until)),
                ValueExpr::Term(Term::Literal(Value::Duration(step))),
            ) = (from, until, step)
            else {
                return;
            };
            let Some(bound) = bounds
                .iter()
                .find(|(u, f, _)| *u == until && *f == from)
                .map(|(_, _, b)| *b)
            else {
                return;
            };
            let periods = grid::max_periods(bound, *step);
            if periods > LARGE_GRID_PERIODS {
                out.push(Lint::LargeGrid {
                    transformation: t.name.to_string(),
                    bound: bound.to_string(),
                    step: step.to_string(),
                    periods,
                });
            }
        });
    }
}

/// Declared names the generated client would refuse: predicate and
/// intent fields, transformation parameters.
fn client_name_findings(program: &Program, out: &mut Vec<Lint>) {
    let mut push = |owner: String, name: &str| {
        if client_refuses_name(name) {
            out.push(Lint::ClientName {
                owner,
                name: name.to_string(),
            });
        }
    };
    for decl in &program.predicates {
        for arg in &decl.args {
            push(
                format!("a field of predicate `{}`", decl.name),
                arg.name.as_str(),
            );
        }
    }
    for decl in &program.intents {
        for arg in &decl.args {
            push(
                format!("a field of intent `{}`", decl.name),
                arg.name.as_str(),
            );
        }
    }
    for t in &program.transformations {
        for param in &t.parameters {
            push(
                format!("a parameter of transformation `{}`", t.name),
                param.as_str(),
            );
        }
    }
}

/// The third gate-vs-invariant shape: an append-only record and a
/// retractable claim joined in the antecedent, the consequent comparing
/// a value the retractable pattern introduces. A variable the append-only
/// patterns also bind is a join key, not a figure: changing the claim
/// cannot move the rule's truth through it. Once per retractable
/// predicate, at the first pattern that supplies such a value.
fn change_rechecks_history_findings(
    inv: &Invariant,
    implications: &[CollectedImplication<'_>],
    append_only: &BTreeSet<PredicateName>,
    out: &mut Vec<Lint>,
) {
    for implication in implications {
        if !implication.calls.is_empty() {
            continue;
        }
        let patterns = direct_positive_patterns(implication.antecedent);
        let Some(first_append_only) = patterns
            .iter()
            .map(|(p, _)| *p)
            .find(|p| append_only.contains(*p))
        else {
            continue;
        };
        let join_keys: BTreeSet<&crate::ir::Var> = patterns
            .iter()
            .filter(|(p, _)| append_only.contains(*p))
            .flat_map(|(_, args)| pattern_vars(args))
            .collect();
        let compared = compared_vars(implication.consequent);
        let mut reported: BTreeSet<&PredicateName> = BTreeSet::new();
        for (predicate, args) in &patterns {
            if append_only.contains(*predicate) || reported.contains(predicate) {
                continue;
            }
            let introduces_a_compared_figure =
                pattern_vars(args).any(|v| !join_keys.contains(v) && compared.contains(v));
            if introduces_a_compared_figure {
                reported.insert(predicate);
                out.push(Lint::ChangeRechecksHistory {
                    invariant: inv.name.to_string(),
                    append_only: first_append_only.to_string(),
                    retractable: predicate.to_string(),
                });
            }
        }
    }
}

/// The variables a claim pattern names.
fn pattern_vars(args: &[Term]) -> impl Iterator<Item = &crate::ir::Var> {
    args.iter().filter_map(|t| match t {
        Term::Var(v) => Some(v),
        _ => None,
    })
}

/// The variables a proposition compares: those named inside the operands
/// of its comparisons and equalities, anywhere in it.
fn compared_vars(prop: &Prop) -> BTreeSet<&crate::ir::Var> {
    let mut out = BTreeSet::new();
    crate::fold::walk_prop(prop, &mut |node| {
        let operands: &[&ValueExpr] = match node {
            crate::fold::Node::Prop(Prop::Compare { left, right, .. })
            | crate::fold::Node::Prop(Prop::Eq(left, right))
            | crate::fold::Node::Prop(Prop::Neq(left, right)) => &[left, right],
            _ => return,
        };
        for operand in operands {
            crate::fold::walk_value(operand, &mut |n| {
                if let crate::fold::Node::Value(ValueExpr::Term(Term::Var(v))) = n {
                    out.insert(v);
                }
            });
        }
    });
    out
}

/// The claim patterns an antecedent asserts positively, written in it
/// directly: through `and`, and under `not` only at the flipped
/// polarity, which is never positive here. Calls are not followed, since
/// the variables a body binds are its own.
fn direct_positive_patterns(prop: &Prop) -> Vec<(&PredicateName, &[Term])> {
    fn go<'a>(prop: &'a Prop, positive: bool, out: &mut Vec<(&'a PredicateName, &'a [Term])>) {
        match prop {
            Prop::Claim { predicate, args } => {
                if positive {
                    out.push((predicate, args));
                }
            }
            Prop::Not(inner) => go(inner, !positive, out),
            Prop::And(props) => {
                for p in props {
                    go(p, positive, out);
                }
            }
            _ => {}
        }
    }
    let mut out = Vec::new();
    go(prop, true, &mut out);
    out
}

/// The re-match hints for one transformation: once per variable, at the
/// first later statement that names it while it is still unbound.
fn bind_only_to_retract_findings(
    program: &Program,
    t: &crate::ir::Transformation,
    out: &mut Vec<Lint>,
) {
    use crate::ir::Stmt;
    let mut bound: BTreeSet<&crate::ir::Var> = t.parameters.iter().collect();
    for (b, stmt) in t.body.iter().enumerate() {
        if let Stmt::BindOne {
            prop: Prop::Claim { predicate, args },
            ..
        } = stmt
        {
            let introduced: BTreeSet<&crate::ir::Var> =
                pattern_vars(args).filter(|v| !bound.contains(v)).collect();
            let retract = t.body.iter().enumerate().skip(b + 1).find(|(_, s)| {
                matches!(s, Stmt::Retract { predicate: p, args: a } if p == predicate && a == args)
            });
            // A `bind` refuses when several claims match; a pattern
            // retract removes them all. The hint is safe only where the
            // fields kept pin one claim: an enforced uniqueness key, every
            // field of it grounded before the bind or a literal.
            let kept = |i: usize| match &args[i] {
                Term::Var(v) => !introduced.contains(v),
                Term::Literal(_) => true,
                Term::Wildcard | Term::Actor => false,
            };
            let keyed = crate::disciplines::uniqueness_keys_for(program, predicate)
                .iter()
                .any(|key| key.iter().all(|&i| kept(i)));
            if let Some((r, _)) = retract
                && !introduced.is_empty()
                && keyed
                && !t.body.iter().enumerate().any(|(i, s)| {
                    i != b && i != r && mentions(s).iter().any(|v| introduced.contains(v))
                })
            {
                out.push(Lint::BindOnlyToRetract {
                    transformation: t.name.to_string(),
                    predicate: predicate.to_string(),
                    bind: b,
                    retract: r,
                    pattern: pattern_without(program, predicate, args, &introduced),
                });
            }
        }
        match stmt {
            Stmt::BindOne { prop, .. } => bound.extend(matched_by(prop)),
            Stmt::Let { name, .. } | Stmt::LetNewSubject { name } => {
                bound.insert(name);
            }
            _ => {}
        }
    }
}

/// The claim pattern with `dropped` elided: the named form over the
/// declaration's field names where every kept term is a variable, the
/// positional form with wildcards otherwise.
fn pattern_without(
    program: &Program,
    predicate: &PredicateName,
    args: &[Term],
    dropped: &BTreeSet<&crate::ir::Var>,
) -> String {
    let decl = program.predicates.iter().find(|d| d.name == *predicate);
    let kept: Vec<(usize, &crate::ir::Var)> = args
        .iter()
        .enumerate()
        .filter_map(|(i, term)| match term {
            Term::Var(v) if !dropped.contains(v) => Some((i, v)),
            _ => None,
        })
        .collect();
    let all_vars = kept.len() + dropped.len() == args.len();
    if let Some(decl) = decl
        && all_vars
    {
        let fields: Vec<String> = kept
            .iter()
            .map(|(i, v)| format!("{}: {v}", decl.args[*i].name))
            .collect();
        return format!("{predicate}({}, ..)", fields.join(", "));
    }
    crate::format::format_prop_inline(&Prop::Claim {
        predicate: predicate.clone(),
        args: args
            .iter()
            .map(|term| match term {
                Term::Var(v) if dropped.contains(v) => Term::Wildcard,
                other => other.clone(),
            })
            .collect(),
    })
}

/// Every variable a statement names, its `for` body included.
fn mentions(stmt: &crate::ir::Stmt) -> BTreeSet<&crate::ir::Var> {
    let mut out: BTreeSet<&crate::ir::Var> = named_here(stmt).into_iter().collect();
    if let crate::ir::Stmt::For { body, .. } = stmt {
        for inner in body {
            out.extend(mentions(inner));
        }
    }
    out
}

fn require_rematch_findings(t: &crate::ir::Transformation, out: &mut Vec<Lint>) {
    let mut rematch = Rematch {
        transformation: t,
        matched: std::collections::BTreeMap::new(),
        reported: BTreeSet::new(),
        visited: 0,
        out,
    };
    let mut bound: BTreeSet<&crate::ir::Var> = t.parameters.iter().collect();
    for (index, stmt) in t.body.iter().enumerate() {
        rematch.visit(stmt, index, &mut bound);
    }
}

struct Rematch<'a, 'o> {
    transformation: &'a crate::ir::Transformation,
    /// Each variable a `require` matched, by the first such statement:
    /// its place in visiting order, and the top-level statement holding it.
    matched: std::collections::BTreeMap<&'a crate::ir::Var, (usize, usize)>,
    reported: BTreeSet<&'a crate::ir::Var>,
    visited: usize,
    out: &'o mut Vec<Lint>,
}

impl<'a> Rematch<'a, '_> {
    /// `index` is the top-level statement, which a nested `for` body keeps.
    fn visit(
        &mut self,
        stmt: &'a crate::ir::Stmt,
        index: usize,
        bound: &mut BTreeSet<&'a crate::ir::Var>,
    ) {
        use crate::ir::Stmt;
        let here = self.visited;
        self.visited += 1;
        for var in named_here(stmt) {
            if bound.contains(var) || !self.reported.insert(var) {
                continue;
            }
            match self.matched.get(var) {
                Some(&(seen, first)) if seen != here => self.out.push(Lint::RequireRematch {
                    transformation: self.transformation.name.to_string(),
                    variable: var.to_string(),
                    first,
                    again: index,
                }),
                _ => {
                    self.reported.remove(var);
                }
            }
        }
        match stmt {
            Stmt::Require { prop, .. } => {
                for var in matched_by(prop) {
                    if !bound.contains(var) {
                        self.matched.entry(var).or_insert((here, index));
                    }
                }
            }
            Stmt::BindOne { prop, .. } => bound.extend(matched_by(prop)),
            Stmt::Let { name, .. } | Stmt::LetNewSubject { name } => {
                bound.insert(name);
            }
            Stmt::For { binding, body, .. } => {
                let mut inner = bound.clone();
                inner.insert(binding);
                for s in body {
                    self.visit(s, index, &mut inner);
                }
            }
            Stmt::Assert(_) | Stmt::Retract { .. } | Stmt::Emit(_) => {}
        }
    }
}

/// The variables a statement names outside any quantifier binding them
/// (a `forall` binds its own in its source too) and outside any `sum`,
/// `max` or `min` body, whose variables are its own; for a `for`, only its
/// collection, since its body is visited apart.
fn named_here<'a>(stmt: &'a crate::ir::Stmt) -> Vec<&'a crate::ir::Var> {
    use crate::fold::Node;
    let term = |node: Node<'a>| match node {
        Node::Slot(t) | Node::Value(ValueExpr::Term(t)) => Some(t),
        _ => None,
    };
    let mut local: BTreeSet<*const Term> = BTreeSet::new();
    crate::fold::walk_stmt(stmt, &mut |node| match node {
        Node::Value(aggregate @ (ValueExpr::Sum { .. } | ValueExpr::Extremum { .. })) => {
            crate::fold::walk_value(aggregate, &mut |inner| {
                local.extend(term(inner).map(std::ptr::from_ref));
            });
        }
        Node::Prop(Prop::Forall {
            binding, source, ..
        }) => crate::fold::walk_prop(source, &mut |inner| {
            if let Some(t @ Term::Var(v)) = term(inner)
                && v == binding
            {
                local.insert(std::ptr::from_ref(t));
            }
        }),
        _ => {}
    });
    let mut out = Vec::new();
    let mut take = |node: Node<'a>, scope: &[&'a crate::ir::Var]| {
        if let Some(t @ Term::Var(v)) = term(node)
            && !scope.contains(&v)
            && !local.contains(&std::ptr::from_ref(t))
        {
            out.push(v);
        }
    };
    match stmt {
        crate::ir::Stmt::For { collection, .. } => {
            crate::fold::walk_value_scoped(collection, &mut take);
        }
        other => crate::fold::walk_stmt_scoped(other, &mut take),
    }
    out
}

/// The variables a proposition certainly binds: those of claim and call
/// patterns it matches positively, through `and`, `pre` and `exists`, and
/// those every branch of an `or` binds. Conservative: a hint built on it
/// may miss a re-match, never invent one.
fn matched_by(prop: &Prop) -> BTreeSet<&crate::ir::Var> {
    match prop {
        Prop::Claim { args, .. } | Prop::Defined { args, .. } => args
            .iter()
            .filter_map(|t| match t {
                Term::Var(v) => Some(v),
                _ => None,
            })
            .collect(),
        Prop::And(props) => props.iter().flat_map(matched_by).collect(),
        Prop::Or(props) => {
            let mut branches = props.iter().map(matched_by);
            let first = branches.next().unwrap_or_default();
            branches.fold(first, |acc, b| acc.intersection(&b).copied().collect())
        }
        Prop::Pre(p) => matched_by(p),
        Prop::Exists { binding, body } => {
            let mut vars = matched_by(body);
            vars.remove(binding);
            vars
        }
        Prop::Not(_)
        | Prop::Implies { .. }
        | Prop::Forall { .. }
        | Prop::Xor(_, _)
        | Prop::Eq(_, _)
        | Prop::Neq(_, _)
        | Prop::Compare { .. }
        | Prop::In(_, _) => BTreeSet::new(),
    }
}

/// The case-width hint for one authored invariant. Eligibility is the
/// impact plan's: one bounded claim pattern. Identity is the
/// disciplines': a uniqueness key fully constrained (by a case variable
/// or a literal) means the case is already one claim.
fn case_width_findings(program: &Program, inv: &Invariant, plan: &ImpactPlan, out: &mut Vec<Lint>) {
    if crate::fold::any_prop_node(&inv.body, &|p| matches!(p, Prop::Defined { .. })) {
        return;
    }
    let Some(occ) = plan.single_bounded_occurrence() else {
        return;
    };
    let Some(decl) = program.predicates.iter().find(|d| d.name == *occ.predicate) else {
        return;
    };
    let field = |pos: &usize| {
        decl.args
            .get(*pos)
            .map(|a| a.name.clone())
            .unwrap_or_default()
    };
    let unconstrained: Vec<usize> = (0..decl.args.len())
        .filter(|pos| !occ.constrained.contains(pos))
        .collect();
    if unconstrained.is_empty() {
        return;
    }
    let keys = crate::disciplines::uniqueness_keys_for(program, occ.predicate);
    let key: Vec<usize> = if keys.is_empty() {
        Vec::new()
    } else {
        let missing = |k: &Vec<usize>| -> Vec<usize> {
            k.iter()
                .copied()
                .filter(|pos| !occ.constrained.contains(pos))
                .collect()
        };
        let closest = keys
            .iter()
            .map(missing)
            .min_by_key(Vec::len)
            .unwrap_or_default();
        if closest.is_empty() {
            return;
        }
        closest
    };
    out.push(Lint::CaseWiderThanClaim {
        invariant: inv.name.to_string(),
        predicate: decl.name.to_string(),
        case: occ.constrained.iter().map(field).collect(),
        key: key.iter().map(field).collect(),
        unnamed: unconstrained.iter().map(field).collect(),
    });
}

/// `a`, `` `a` and `b` ``, `` `a`, `b` and `c` ``.
fn backticked(names: &[String]) -> String {
    let quoted: Vec<String> = names.iter().map(|n| format!("`{n}`")).collect();
    match quoted.as_slice() {
        [] => String::new(),
        [one] => one.clone(),
        [init @ .., last] => format!("{} and {last}", init.join(", ")),
    }
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
